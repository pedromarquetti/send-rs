use crate::backend::{MediaKind, MessageMedia};

use whatsapp_rust::prelude::MessageExt;
use whatsapp_rust::wacore::download::MediaType;
use whatsapp_rust::waproto::whatsapp::Message as WaMessage;

/// CDN fields needed to re-download one media message's bytes on demand,
/// mirroring whatsapp-rust's `DownloadParams` but kept serde-friendly (its
/// `MediaType` has no serde impls). Built from an image/audio/video message's
/// raw fields and rebuilt into `DownloadParams`
/// (`MediaType::{Image, Audio, Video}`) at fetch time. Holds no media bytes —
/// only the references that fetch them.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(super) struct MediaRef {
    /// Which provider-neutral media kind the CDN fields hold, so `media_bytes`
    /// rebuilds the right `MediaType`. Old caches only ever stored images.
    pub(super) kind: MediaKind,
    pub(super) direct_path: String,
    pub(super) media_key: Vec<u8>,
    pub(super) file_sha256: Vec<u8>,
    pub(super) file_enc_sha256: Vec<u8>,
    pub(super) file_length: u64,
}

impl Default for MediaRef {
    fn default() -> Self {
        Self {
            kind: MediaKind::Image,
            file_length: Default::default(),
            file_enc_sha256: Default::default(),
            media_key: Default::default(),
            direct_path: Default::default(),
            file_sha256: Default::default(),
        }
    }
}

/// The persisted CDN reference for a message's image, if it carries one with
/// every field needed to download+decrypt it later. Messages lacking any of
/// them (e.g. newsletter media with only a `static_url`) are not stored.
fn image_media_ref(msg: &WaMessage) -> Option<MediaRef> {
    let img = msg.get_base_message().image_message.as_option()?;

    Some(MediaRef {
        kind: MediaKind::Image,
        direct_path: img.direct_path.as_ref()?.clone(),
        media_key: img.media_key.as_ref()?.clone(),
        file_sha256: img.file_sha256.as_ref()?.clone(),
        file_enc_sha256: img.file_enc_sha256.as_ref()?.clone(),
        file_length: img.file_length?,
    })
}

/// The persisted CDN reference for an audio/voice message, if it carries every
/// field needed to download+decrypt it later.
fn audio_media_ref(msg: &WaMessage) -> Option<MediaRef> {
    let audio = msg.get_base_message().audio_message.as_option()?;

    Some(MediaRef {
        kind: MediaKind::Audio {
            duration_secs: audio.seconds,
            is_voice: audio.ptt.unwrap_or(false),
            waveform: audio.waveform.clone(),
        },
        direct_path: audio.direct_path.as_ref()?.clone(),
        media_key: audio.media_key.as_ref()?.clone(),
        file_sha256: audio.file_sha256.as_ref()?.clone(),
        file_enc_sha256: audio.file_enc_sha256.as_ref()?.clone(),
        file_length: audio.file_length?,
    })
}

/// The persisted CDN reference for a video message, if it carries every field
/// needed to download+decrypt it later. `VideoMessage` exposes the same five
/// CDN fields as an image.
fn video_media_ref(msg: &WaMessage) -> Option<MediaRef> {
    let video = msg.get_base_message().video_message.as_option()?;

    Some(MediaRef {
        kind: MediaKind::Video,
        direct_path: video.direct_path.as_ref()?.clone(),
        media_key: video.media_key.as_ref()?.clone(),
        file_sha256: video.file_sha256.as_ref()?.clone(),
        file_enc_sha256: video.file_enc_sha256.as_ref()?.clone(),
        file_length: video.file_length?,
    })
}

/// CDN reference for a message's on-demand media (image, video or audio),
/// whichever it carries, or `None` when there is none or it is incomplete.
pub(super) fn wa_media_ref(msg: &WaMessage) -> Option<MediaRef> {
    image_media_ref(msg)
        .or_else(|| video_media_ref(msg))
        .or_else(|| audio_media_ref(msg))
}

/// The `MediaType` that decrypts a stored ref. The reference records which
/// media kind it came from because the kinds use different CDN keys;
/// `None` for kinds that are never captured into `media_refs`.
pub(super) fn ref_media_type(kind: &MediaKind) -> Option<MediaType> {
    match kind {
        MediaKind::Image => Some(MediaType::Image),
        MediaKind::Video => Some(MediaType::Video),
        MediaKind::Audio { .. } => Some(MediaType::Audio),
        _ => None,
    }
}

/// Convert [`WaMessage`] to [`MessageMedia`]
pub(super) fn handle_wa_media(msg: &WaMessage) -> Option<MessageMedia> {
    let base = msg.get_base_message();
    let kind = if base.image_message.is_set() {
        MediaKind::Image
    } else if base.video_message.is_set() {
        MediaKind::Video
    } else if let Some(audio) = base.audio_message.as_option() {
        // Voice notes/audio report their playback length; the TUI shows it
        // before the first decode.
        MediaKind::Audio {
            duration_secs: audio.seconds,
            is_voice: audio.ptt.unwrap_or(false),
            waveform: audio.waveform.clone(),
        }
    } else if base.sticker_message.is_set() {
        MediaKind::Sticker
    } else if base.document_message.is_set() {
        MediaKind::Document
    } else {
        return None;
    };

    let caption = base.get_caption().map(str::to_string);

    let file_name = base
        .document_message
        .as_option()
        .and_then(|d| d.file_name.clone());

    Some(MessageMedia {
        kind,
        caption,
        file_name,
    })
}
