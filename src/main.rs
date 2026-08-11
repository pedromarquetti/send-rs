#![allow(dead_code)]

mod backend;
mod notify;
mod tui;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tui::run().await
}
