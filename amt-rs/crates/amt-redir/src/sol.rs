//! Serial-over-LAN helpers on top of the redirection session.

use anyhow::Result;

use crate::session::{Protocol, Session};

/// Open an authenticated SOL session.
pub async fn open(host: &str, port: u16, user: &str, pass: &str) -> Result<Session> {
    let mut session = Session::connect(host, port, Protocol::Sol).await?;
    session.handshake(user, pass).await?;
    Ok(session)
}
