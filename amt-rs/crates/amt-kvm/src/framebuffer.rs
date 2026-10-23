// Simple framebuffer representation
pub struct Framebuffer {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>, // RGBA8 per pixel
}

impl Framebuffer {
    pub fn new(width: usize, height: usize) -> Self {
        Self { width, height, pixels: vec![0; width * height * 4] }
    }
    pub fn clear(&mut self) {
        self.pixels.fill(0);
    }
}
