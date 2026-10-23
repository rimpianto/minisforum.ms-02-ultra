pub mod session;
pub mod digest;
pub mod frame;
pub mod kvm;
pub mod sol;

pub struct AmtRedirectionSession {
    // Placeholder fields
    pub address: String,
    pub port: u16,
}

impl AmtRedirectionSession {
    pub fn new(address: &str, port: u16) -> Self {
        Self { address: address.to_string(), port }
    }
    // TODO: implement connection, handshake, etc.
}
