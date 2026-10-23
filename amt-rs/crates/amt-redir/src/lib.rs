//! Intel AMT redirection: TLS transport, AAAH handshake, SOL/KVM/IDER.

pub mod digest;
pub mod frame;
pub mod kvm;
pub mod session;
pub mod sol;

pub use session::{Protocol, Session};
