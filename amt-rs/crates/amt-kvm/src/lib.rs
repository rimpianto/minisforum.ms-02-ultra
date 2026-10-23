//! KVM video: framebuffer and the Intel AMT encodings (RAW, RLE).

pub mod decoder;
pub mod framebuffer;
pub mod input;
pub mod palette;

pub use framebuffer::Framebuffer;
