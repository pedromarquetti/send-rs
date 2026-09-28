//! WhatsApp WebSocket transport that races every address the host resolves to.
//!
//! `tokio-websockets`' own `Gai` resolver takes only the *first* address
//! `getaddrinfo` returns (`resolver.rs`: `lookup_host(..).next()`), and its
//! `connect()` then dials that single `SocketAddr` with no fallback. A host
//! that resolves to an unreachable IPv6 address first — e.g. a machine that
//! holds an IPv6 address but has no IPv6 default route — therefore hangs on
//! the dead address until whatsapp-rust's 20s connect timeout expires, and
//! the IPv4 address that would have worked is never tried.
//!
//! This factory resolves the host itself, orders the candidates so both
//! families are tried early, and races them: the first socket to connect wins
//! while the others keep dialing in the background.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::net::TcpStream;
use tracing::debug;
use whatsapp_rust::transport::{
    Connector, Transport, TransportEvent, TransportFactory, default_tls_connector, from_websocket,
};
use whatsapp_rust::wacore::net::{WHATSAPP_WEB_ORIGIN, WHATSAPP_WEB_WS_URL};

/// Delay before each successive candidate starts dialing. RFC 8305 recommends
/// 150-250ms: long enough that a working first candidate is not slowed down,
/// short enough that a dead one costs one stagger instead of a connect timeout.
pub(super) const STAGGER: Duration = Duration::from_millis(200);

/// Opens the WhatsApp Web websocket, racing all resolved addresses.
///
/// Wired into the bot through `BotBuilder::with_transport_factory`, replacing
/// the `tokio-transport` default. Everything except address selection matches
/// that default: the same endpoint, the same `Origin` header, and the same TLS
/// connector, which is built once and kept so its session-resumption store
/// survives reconnects (rebuilding it per dial left the store permanently
/// empty, so resumption could never fire).
pub struct WebSocketTransportFactory {
    default_connector: OnceLock<Connector>,
}

impl WebSocketTransportFactory {
    pub fn new() -> Self {
        Self {
            default_connector: OnceLock::new(),
        }
    }
}

impl Default for WebSocketTransportFactory {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TransportFactory for WebSocketTransportFactory {
    async fn create_transport(
        &self,
    ) -> Result<(Arc<dyn Transport>, async_channel::Receiver<TransportEvent>), anyhow::Error> {
        let uri: http::Uri = WHATSAPP_WEB_WS_URL
            .parse()
            .context("WhatsApp: websocket URL is not a valid URI")?;

        // `Uri::host` keeps the brackets around a literal IPv6 host, which do
        // not resolve and are not valid TLS server names.
        let host = uri
            .host()
            .context("WhatsApp: websocket URL has no host")?
            .trim_start_matches('[')
            .trim_end_matches(']');
        let port = uri.port_u16().unwrap_or(443);

        let tcp = connect_racing(host, port).await?;

        let connector = self.default_connector.get_or_init(default_tls_connector);
        let tls = connector
            .wrap(host, tcp)
            .await
            .context("WhatsApp: TLS handshake failed")?;

        // `connect_on` takes over an already-established TLS stream and only
        // performs the HTTP upgrade, skipping the library's own resolve/dial
        // (the part that commits to a single address).
        let (ws, _) = tokio_websockets::ClientBuilder::from_uri(uri)
            .add_header(
                http::header::ORIGIN,
                http::HeaderValue::from_static(WHATSAPP_WEB_ORIGIN),
            )
            .context("WhatsApp: could not set the Origin header")?
            .connect_on(tls)
            .await
            .context("WhatsApp: websocket upgrade failed")?;

        Ok(from_websocket(ws))
    }
}

/// Resolves `host` and returns the first socket to connect.
pub(super) async fn connect_racing(host: &str, port: u16) -> Result<TcpStream> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("WhatsApp: DNS lookup failed for {host}"))?
        .collect();

    if addrs.is_empty() {
        bail!("WhatsApp: {host} resolved to no addresses");
    }

    connect_addrs(interleave_families(&addrs)).await
}

/// Dials every candidate, staggered, and yields the first that connects. A
/// candidate that fails is logged and skipped while the others are still in
/// flight; only when all of them have failed does this return the last error.
/// Dropping the future aborts the losers, so a cancelled dial leaves no socket
/// behind.
pub(super) async fn connect_addrs(addrs: Vec<SocketAddr>) -> Result<TcpStream> {
    let mut dials = FuturesUnordered::new();
    for (attempt, addr) in addrs.into_iter().enumerate() {
        dials.push(async move {
            tokio::time::sleep(STAGGER * attempt as u32).await;
            TcpStream::connect(addr).await.map(|tcp| (addr, tcp))
        });
    }

    let mut last_err = None;
    while let Some(result) = dials.next().await {
        match result {
            Ok((addr, tcp)) => {
                debug!(%addr, "WhatsApp transport connected");
                return Ok(tcp);
            }
            Err(err) => {
                debug!(%err, "WhatsApp transport candidate failed");
                last_err = Some(err);
            }
        }
    }

    Err(last_err
        .unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no candidate address")
        })
        .into())
}

/// Orders candidates so both address families are tried early: v4[0], v6[0],
/// v4[1], v6[1], … A host whose IPv6 address is unroutable still reaches its
/// IPv4 address after a single stagger rather than being written off.
pub(super) fn interleave_families(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    let mut v4: Vec<SocketAddr> = addrs.iter().copied().filter(SocketAddr::is_ipv4).collect();
    let mut v6: Vec<SocketAddr> = addrs.iter().copied().filter(|a| a.is_ipv6()).collect();

    let mut ordered = Vec::with_capacity(addrs.len());
    while !v4.is_empty() || !v6.is_empty() {
        if !v4.is_empty() {
            ordered.push(v4.remove(0));
        }
        if !v6.is_empty() {
            ordered.push(v6.remove(0));
        }
    }

    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_alternate_between_families() {
        let v4 = |port| SocketAddr::from(([127, 0, 0, 1], port));
        let v6 = |port| SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1], port));

        assert_eq!(
            interleave_families(&[v6(1), v6(2), v4(3), v6(4), v4(5)]),
            vec![v4(3), v6(1), v4(5), v6(2), v6(4)]
        );
    }

    #[test]
    fn a_single_family_keeps_the_resolved_order() {
        let addrs: Vec<SocketAddr> = (1..=3)
            .map(|port| SocketAddr::from(([127, 0, 0, 1], port)))
            .collect();

        assert_eq!(interleave_families(&addrs), addrs);
    }

    /// The regression this factory exists for: a host that resolves to an
    /// unroutable IPv6 address first must still connect over IPv4. A refused
    /// local port stands in for the dead address — both fail the same way from
    /// the racing loop's point of view (the candidate errors, the dial goes on).
    #[tokio::test]
    async fn a_failing_candidate_does_not_sink_the_dial() {
        let live = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = live.local_addr().unwrap();

        // Bind then drop, so the port is certainly closed: nothing else on the
        // host can hand it back during the test.
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);

        let tcp = connect_addrs(vec![dead_addr, live_addr]).await.unwrap();
        assert_eq!(tcp.peer_addr().unwrap(), live_addr);
    }
}
