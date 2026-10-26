// Scaffolding kept for the next step (IDER, WS-Man Get of other classes).
#![allow(dead_code)]
use reqwest::{Client, IntoUrl};
use anyhow::Result;

pub fn build_client(_base_url: impl IntoUrl) -> Result<Client> {
    // Placeholder: build a reqwest client with rustls and digest (not implemented)
    Ok(Client::new())
}
