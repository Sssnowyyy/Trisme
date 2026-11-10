//! the game's names and icon from its SMDH, and the pictures a Wii U title
//! shows: its icon and the TV's and GamePad's splash screens, as TGA files.

use std::path::Path;

/// an RGBA picture.
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<[u8; 4]>,
}

impl Picture {
    pub fn filled(width: usize, height: usize, color: [u8; 4]) -> Picture {
        Picture { width, height, pixels: vec![color; width * height] }
    }

    /// this picture grown or shrunk to width x height, bilinear.
    pub fn resized(&self, width: usize, height: usize) -> Picture {
        let mut out = Picture::filled(width, height, [0; 4]);
        for y in 0..height {
            let fy = ((y as f32 + 0.5) * self.height as f32 / height as f32 - 0.5).max(0.0);
            let (y0, ty) = (fy as usize, fy.fract());
            let y1 = (y0 + 1).min(self.height - 1);
            for x in 0..width {
                let fx = ((x as f32 + 0.5) * self.width as f32 / width as f32 - 0.5).max(0.0);
                let (x0, tx) = (fx as usize, fx.fract());
                let x1 = (x0 + 1).min(self.width - 1);
                let at = |x: usize, y: usize| self.pixels[y * self.width + x];
                let mut pixel = [0u8; 4];
                for (c, value) in pixel.iter_mut().enumerate() {
                    let top = at(x0, y0)[c] as f32 * (1.0 - tx) + at(x1, y0)[c] as f32 * tx;
                    let bottom = at(x0, y1)[c] as f32 * (1.0 - tx) + at(x1, y1)[c] as f32 * tx;
                    *value = (top * (1.0 - ty) + bottom * ty).round().clamp(0.0, 255.0) as u8;
                }
                out.pixels[y * width + x] = pixel;
            }
        }
        out
    }

    /// another picture drawn over this one at (x, y), blended by its alpha.
    pub fn draw(&mut self, other: &Picture, x: usize, y: usize) {
        for oy in 0..other.height {
            for ox in 0..other.width {
                let (tx, ty) = (x + ox, y + oy);
                if tx >= self.width || ty >= self.height {
                    continue;
                }
                let source = other.pixels[oy * other.width + ox];
                let target = &mut self.pixels[ty * self.width + tx];
                let alpha = source[3] as u32;
                for c in 0..3 {
                    target[c] = ((source[c] as u32 * alpha + target[c] as u32 * (255 - alpha)) / 255) as u8;
                }
                target[3] = target[3].max(source[3]);
            }
        }
    }

    /// an uncompressed TGA, 32 bits with alpha or 24 without, the way
    /// Nintendo's own meta textures are: rows bottom to top (descriptor 0x08
    /// or 0x00) and the TGA 2.0 footer. the Wii U menu rejects top-down
    /// textures without the footer: "???" for the title and an error when
    /// it starts. wuhbtool takes these too.
    pub fn write_tga(&self, path: &Path, alpha: bool) -> Result<(), String> {
        let bytes_per_pixel = if alpha { 4 } else { 3 };
        let mut data = Vec::with_capacity(18 + self.width * self.height * bytes_per_pixel);
        data.extend_from_slice(&[0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        data.extend_from_slice(&(self.width as u16).to_le_bytes());
        data.extend_from_slice(&(self.height as u16).to_le_bytes());
        data.push(if alpha { 32 } else { 24 });
        // bottom-left origin, and the alpha bits
        data.push(if alpha { 8 } else { 0 });
        for row in self.pixels.chunks(self.width).rev() {
            for pixel in row {
                data.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
                if alpha {
                    data.push(pixel[3]);
                }
            }
        }
        // no extension or developer area, then the signature
        data.extend_from_slice(&[0; 8]);
        data.extend_from_slice(b"TRUEVISION-XFILE.\0");
        std::fs::write(path, data).map_err(|e| format!("could not write {}, {e}", path.display()))
    }
}

/// what an SMDH says of the game, in English when it has it.
pub struct Smdh {
    pub short_name: String,
    pub long_name: String,
    pub publisher: String,
    /// the 48x48 icon.
    pub icon: Picture,
}

fn utf16(data: &[u8]) -> String {
    let units: Vec<u16> = data.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).take_while(|&unit| unit != 0).collect();
    String::from_utf16_lossy(&units).replace('\n', " ").trim().to_owned()
}

/// the index of a pixel within an 8x8 tile, which the 3DS lays out in
/// Morton (Z) order.
fn morton(x: usize, y: usize) -> usize {
    let mut index = 0;
    for bit in 0..3 {
        index |= ((x >> bit) & 1) << (2 * bit);
        index |= ((y >> bit) & 1) << (2 * bit + 1);
    }
    index
}

/// a tiled RGB565 icon as a picture.
fn decode_icon(data: &[u8], size: usize) -> Picture {
    let mut picture = Picture::filled(size, size, [0, 0, 0, 255]);
    let tiles = size / 8;
    for y in 0..size {
        for x in 0..size {
            let tile = (y / 8) * tiles + x / 8;
            let index = tile * 64 + morton(x % 8, y % 8);
            let Some(pair) = data.get(index * 2..index * 2 + 2) else { continue };
            let value = u16::from_le_bytes([pair[0], pair[1]]);
            let r = ((value >> 11) & 0x1F) as u32;
            let g = ((value >> 5) & 0x3F) as u32;
            let b = (value & 0x1F) as u32;
            picture.pixels[y * size + x] = [(r * 255 / 31) as u8, (g * 255 / 63) as u8, (b * 255 / 31) as u8, 255];
        }
    }
    picture
}

impl Smdh {
    pub fn parse(data: &[u8]) -> Option<Smdh> {
        if data.len() < 0x36C0 || &data[..4] != b"SMDH" {
            return None;
        }
        // sixteen titles of 0x200 bytes from 0x8: Japanese, English, ...;
        // English first, then whichever has a name
        let title = |index: usize| {
            let at = 8 + index * 0x200;
            (utf16(&data[at..at + 0x80]), utf16(&data[at + 0x80..at + 0x180]), utf16(&data[at + 0x180..at + 0x200]))
        };
        let (short_name, long_name, publisher) =
            std::iter::once(1).chain(0..16).map(title).find(|(short, _, _)| !short.is_empty()).unwrap_or_default();
        Some(Smdh { short_name, long_name, publisher, icon: decode_icon(&data[0x24C0..0x36C0], 48) })
    }
}

/// a splash screen: the icon, large, on a dark background.
pub fn splash(icon: Option<&Picture>, width: usize, height: usize) -> Picture {
    let mut picture = Picture::filled(width, height, [16, 16, 24, 255]);
    if let Some(icon) = icon {
        let side = height / 2;
        let large = icon.resized(side, side);
        picture.draw(&large, (width - side) / 2, (height - side) / 2);
    }
    picture
}

/// the icon at 128x128 for the Wii U menu, or a plain one.
pub fn menu_icon(icon: Option<&Picture>) -> Picture {
    match icon {
        Some(icon) => icon.resized(128, 128),
        None => Picture::filled(128, 128, [200, 40, 60, 255]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn morton_order_matches_the_3ds() {
        assert_eq!(morton(0, 0), 0);
        assert_eq!(morton(1, 0), 1);
        assert_eq!(morton(0, 1), 2);
        assert_eq!(morton(1, 1), 3);
        assert_eq!(morton(2, 0), 4);
        assert_eq!(morton(7, 7), 63);
    }

    #[test]
    fn smdh_names_and_icon() {
        let mut data = vec![0u8; 0x36C0];
        data[..4].copy_from_slice(b"SMDH");
        let english = 8 + 0x200;
        for (i, unit) in "Game".encode_utf16().enumerate() {
            data[english + i * 2..english + i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        for (i, unit) in "Maker".encode_utf16().enumerate() {
            data[english + 0x180 + i * 2..english + 0x180 + i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        // the icon's first pixel pure red, tile 0 index 0
        data[0x24C0..0x24C2].copy_from_slice(&0xF800u16.to_le_bytes());
        let smdh = Smdh::parse(&data).unwrap();
        assert_eq!(smdh.short_name, "Game");
        assert_eq!(smdh.publisher, "Maker");
        assert_eq!(smdh.icon.pixels[0], [255, 0, 0, 255]);
        assert_eq!(menu_icon(Some(&smdh.icon)).width, 128);
    }

    #[test]
    fn tga_header() {
        let dir = std::env::temp_dir().join(format!("cia2wiiu-tga-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.tga");
        let mut picture = Picture::filled(3, 2, [1, 2, 3, 4]);
        picture.pixels[0] = [9, 8, 7, 6]; // top left
        picture.write_tga(&path, true).unwrap();
        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), 18 + 3 * 2 * 4 + 26);
        assert_eq!(&data[12..18], &[3, 0, 2, 0, 32, 0x08]);
        // the bottom row first, BGRA
        assert_eq!(&data[18..22], &[3, 2, 1, 4]);
        assert_eq!(&data[18 + 12..18 + 16], &[7, 8, 9, 6]);
        assert_eq!(&data[data.len() - 18..], b"TRUEVISION-XFILE.\0");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
