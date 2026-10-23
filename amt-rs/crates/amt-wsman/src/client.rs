use reqwest::{Client, IntoUrl};
use anyhow::Result;

pub fn build_client(base_url: impl IntoUrl) -> Result<Client> {
    // Placeholder: build a reqwest client with rustls and digest (not implemented)
    Ok(Client::new())
}
