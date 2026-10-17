//! Read the saved merged image from PSD v1 (public Photoshop File Formats specification).
//! Layers stay in the source document; no Photoshop installation is needed. All section reads
//! and decompression are bounded. Unsupported modes fail before allocating a canvas.

use std::io::Read;

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let part = self.rest.get(..n)?;
        self.rest = self.rest.get(n..)?;
        Some(part)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn section(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.u32()?).ok()?;
        self.take(n)
    }
}

struct Header {
    width: u32,
    height: u32,
    channels: usize,
    depth: u16,
    colors: usize,
}

fn header(r: &mut Reader<'_>) -> Option<Header> {
    if r.take(4)? != b"8BPS" || r.u16()? != 1 || r.take(6)? != [0; 6] {
        return None;
    }
    let channels = usize::from(r.u16()?);
    let height = r.u32()?;
    let width = r.u32()?;
    let depth = r.u16()?;
    let colors = match r.u16()? {
        1 => 1, // Grayscale
        3 => 3, // RGB
        _ => return None,
    };
    if !(1..=30_000).contains(&width) || !(1..=30_000).contains(&height) || !(colors..=56).contains(&channels) || !matches!(depth, 8 | 16) {
        return None;
    }
    // Bound both planar and RGBA allocations, including extra alpha/spot channels.
    let pixels = u64::from(width) * u64::from(height);
    if pixels * 4 > 256 << 20 || pixels * channels as u64 * u64::from(depth / 8) > 256 << 20 {
        return None;
    }
    Some(Header { width, height, channels, depth, colors })
}

pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let h = header(&mut Reader { rest: bytes })?;
    Some((h.width, h.height))
}

fn merged_alpha(layer_data: &[u8], depth: u16) -> Option<bool> {
    if layer_data.is_empty() {
        return Some(true);
    }
    let mut r = Reader { rest: layer_data };
    let info = r.section()?;
    let mut alpha = if info.is_empty() { false } else { i16::from_be_bytes(info.get(..2)?.try_into().ok()?) < 0 };
    if r.rest.is_empty() {
        return Some(alpha);
    }
    r.section()?; // Global layer mask
    while !r.rest.is_empty() {
        if r.rest.len() <= 3 && r.rest.iter().all(|b| *b == 0) {
            break;
        }
        if r.take(4)? != b"8BIM" {
            return None;
        }
        let key = r.take(4)?;
        let block = r.section()?;
        if key == b"Mtrn" || key == b"Mt16" {
            alpha = true;
        }
        if depth == 16 && key == b"Lr16" && !block.is_empty() {
            alpha |= i16::from_be_bytes(block.get(..2)?.try_into().ok()?) < 0;
        }
        r.take((4 - block.len() % 4) % 4)?;
    }
    Some(alpha)
}

fn zeros(n: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    out.try_reserve_exact(n).ok()?;
    out.resize(n, 0);
    Some(out)
}

fn packbits(bytes: &[u8], out: &mut [u8]) -> Option<()> {
    let mut r = Reader { rest: bytes };
    let mut pos = 0usize;
    while !r.rest.is_empty() {
        let control = *r.take(1)?.first()? as i8;
        match control {
            0..=127 => {
                let n = control as usize + 1;
                out.get_mut(pos..pos.checked_add(n)?)?.copy_from_slice(r.take(n)?);
                pos += n;
            }
            -127..=-1 => {
                let n = (1 - i16::from(control)) as usize;
                out.get_mut(pos..pos.checked_add(n)?)?.fill(*r.take(1)?.first()?);
                pos += n;
            }
            -128 => {}
        }
    }
    (pos == out.len()).then_some(())
}

pub fn decode(bytes: &[u8]) -> Option<image::RgbaImage> {
    let mut r = Reader { rest: bytes };
    let h = header(&mut r)?;
    r.section()?; // Color mode data
    r.section()?; // Image resources
    let layer_data = r.section()?;
    // A negative layer count denotes transparency in the first extra merged channel.
    // Other extra channels are user masks, not opacity.
    let transparent = merged_alpha(layer_data, h.depth)? && h.channels > h.colors;
    let compression = r.u16()?;
    let width = usize::try_from(h.width).ok()?;
    let height = usize::try_from(h.height).ok()?;
    let sample_bytes = usize::from(h.depth / 8);
    let row_bytes = width.checked_mul(sample_bytes)?;
    let rows = height.checked_mul(h.channels)?;
    let total = row_bytes.checked_mul(rows)?;
    let mut planes = zeros(total)?;
    match compression {
        0 => planes.copy_from_slice(r.take(total)?),
        1 => {
            let mut counts = Reader { rest: r.take(rows.checked_mul(2)?)? };
            for row in planes.chunks_exact_mut(row_bytes) {
                let n = usize::from(counts.u16()?);
                packbits(r.take(n)?, row)?;
            }
        }
        2 | 3 => {
            let mut decoder = flate2::read::ZlibDecoder::new(r.rest);
            decoder.read_exact(&mut planes).ok()?;
            // Consume the trailer/checksum and reject surplus decompressed data.
            if decoder.read(&mut [0u8; 1]).ok()? != 0 {
                return None;
            }
            if compression == 3 {
                for row in planes.chunks_exact_mut(row_bytes) {
                    if h.depth == 8 {
                        let mut prev = 0u8;
                        for value in row {
                            *value = value.wrapping_add(prev);
                            prev = *value;
                        }
                    } else {
                        let mut prev = 0u16;
                        for sample in row.chunks_exact_mut(2) {
                            let value = u16::from_be_bytes(sample.try_into().ok()?).wrapping_add(prev);
                            sample.copy_from_slice(&value.to_be_bytes());
                            prev = value;
                        }
                    }
                }
            }
        }
        _ => return None,
    }
    let pixels = width.checked_mul(height)?;
    let plane_size = pixels.checked_mul(sample_bytes)?;
    let mut rgba = zeros(pixels.checked_mul(4)?)?;
    let sample = |channel: usize, pixel: usize| -> Option<f64> {
        let offset = channel.checked_mul(plane_size)?.checked_add(pixel.checked_mul(sample_bytes)?)?;
        if h.depth == 8 {
            planes.get(offset).map(|v| f64::from(*v) / 255.0)
        } else {
            let value = u16::from_be_bytes(planes.get(offset..offset + 2)?.try_into().ok()?);
            Some(f64::from(value) / 65535.0)
        }
    };
    for (pixel, out) in rgba.chunks_exact_mut(4).enumerate() {
        let red = sample(0, pixel)?;
        let green = sample(if h.colors == 3 { 1 } else { 0 }, pixel)?;
        let blue = sample(if h.colors == 3 { 2 } else { 0 }, pixel)?;
        let alpha = if transparent { sample(h.colors, pixel)? } else { 1.0 };
        // PSD's merged RGB is composited against white. Recover straight RGB before
        // quantizing 16-bit samples, otherwise transparent edges get a white halo.
        let color = |c: f64| {
            let c = if transparent && h.colors == 3 && alpha > 0.0 { (c + alpha - 1.0) / alpha } else { c };
            (c.clamp(0.0, 1.0) * 255.0).round() as u8
        };
        out.copy_from_slice(&[color(red), color(green), color(blue), (alpha * 255.0).round() as u8]);
    }
    image::RgbaImage::from_raw(h.width, h.height, rgba)
}
