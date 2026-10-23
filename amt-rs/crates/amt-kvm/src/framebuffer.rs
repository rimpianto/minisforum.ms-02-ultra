//! RGBA framebuffer with the two pixel formats Intel AMT actually sends.

pub struct Framebuffer {
    pub width: usize,
    pub height: usize,
    /// RGBA8, row-major.
    pub pixels: Vec<u8>,
}

impl Framebuffer {
    pub fn new(width: usize, height: usize) -> Self {
        let mut fb = Self {
            width,
            height,
            pixels: vec![0; width * height * 4],
        };
        fb.clear();
        fb
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.pixels = vec![0; width * height * 4];
        self.clear();
    }

    pub fn clear(&mut self) {
        for px in self.pixels.chunks_exact_mut(4) {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
            px[3] = 255;
        }
    }

    #[inline]
    fn put_rgba(&mut self, x: usize, y: usize, r: u8, g: u8, b: u8) {
        if x >= self.width || y >= self.height {
            return;
        }
        let i = (y * self.width + x) * 4;
        self.pixels[i] = r;
        self.pixels[i + 1] = g;
        self.pixels[i + 2] = b;
        self.pixels[i + 3] = 255;
    }

    /// 16-bit RGB565, the format the reference uses for RLE16.
    #[inline]
    pub fn put_rgb565(&mut self, x: usize, y: usize, v: u16) {
        let r = ((v >> 8) & 0xF8) as u8;
        let g = ((v >> 3) & 0xFC) as u8;
        let b = ((v & 0x1F) << 3) as u8;
        self.put_rgba(x, y, r, g, b);
    }

    /// 8-bit RGB332 (3-3-2), used for RLE8.
    #[inline]
    pub fn put_rgb332(&mut self, x: usize, y: usize, v: u8) {
        let r = v & 0xE0;
        let g = (v & 0x1C) << 3;
        let b = (v & 0x03) * 85;
        self.put_rgba(x, y, r, g, b);
    }

    /// Write the framebuffer as a PNG (headless tests, screenshots).
    pub fn save_png(&self, path: &str) -> Result<(), String> {
        let img: image::RgbaImage = image::ImageBuffer::from_raw(
            self.width as u32,
            self.height as u32,
            self.pixels.clone(),
        )
        .ok_or_else(|| "framebuffer size mismatch".to_string())?;
        img.save(path).map_err(|e| e.to_string())
    }
}
