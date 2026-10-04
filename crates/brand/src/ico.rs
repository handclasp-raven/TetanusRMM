//! Windows `.ico` files: for the executables' resources and the MSI's
//! entry in Add/Remove Programs.

use crate::raster::Image;

/// The PNG signature.
const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

fn header(count: u16) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // an icon
    out.extend_from_slice(&count.to_le_bytes());
    out
}

fn entry(size: u32, len: u32, offset: u32) -> [u8; 16] {
    // 256 is written as 0.
    let side = if size >= 256 { 0 } else { size as u8 };
    let mut e = [0u8; 16];
    e[0] = side;
    e[1] = side;
    e[4..6].copy_from_slice(&1u16.to_le_bytes()); // colour planes
    e[6..8].copy_from_slice(&32u16.to_le_bytes()); // bits per pixel
    e[8..12].copy_from_slice(&len.to_le_bytes());
    e[12..16].copy_from_slice(&offset.to_le_bytes());
    e
}

/// One picture as an icon's bitmap: 32-bit BGRA, bottom row first, then
/// an (empty) 1-bit mask, as every Windows version reads it.
fn bitmap(image: &Image) -> Vec<u8> {
    let size = image.size;
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&(size as i32 * 2).to_le_bytes()); // picture + mask
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]); // uncompressed, sizes and palette unset
    for y in (0..size).rev() {
        for x in 0..size {
            let [r, g, b, a] = image.pixel(x, y);
            out.extend_from_slice(&[b, g, r, a]);
        }
    }
    // The mask: rows padded to 32 bits, all clear (alpha decides).
    let mask_row = size.div_ceil(32) * 4;
    out.resize(out.len() + (mask_row * size) as usize, 0);
    out
}

/// An `.ico` holding `images`, one entry each.
pub fn encode(images: &[Image]) -> Vec<u8> {
    let bitmaps: Vec<Vec<u8>> = images.iter().map(bitmap).collect();
    let mut out = header(images.len() as u16);
    let mut offset = (6 + 16 * images.len()) as u32;
    for (image, data) in images.iter().zip(&bitmaps) {
        out.extend_from_slice(&entry(image.size, data.len() as u32, offset));
        offset += data.len() as u32;
    }
    for data in bitmaps {
        out.extend_from_slice(&data);
    }
    out
}

/// The width and height a PNG file says it has, if `png` is one.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    let rest = png.strip_prefix(PNG_MAGIC)?;
    // The first chunk is IHDR: length, "IHDR", width, height.
    if rest.len() < 16 || &rest[4..8] != b"IHDR" {
        return None;
    }
    let be = |at: usize| u32::from_be_bytes(rest[at..at + 4].try_into().expect("four bytes"));
    let (width, height) = (be(8), be(12));
    (width > 0 && height > 0).then_some((width, height))
}

/// An `.ico` with the PNG `png` as its one entry (a company's logo, for
/// Add/Remove Programs). `None` if it is not a PNG or is over 256 pixels
/// a side, the most an icon holds.
pub fn from_png(png: &[u8]) -> Option<Vec<u8>> {
    let (width, height) = png_size(png)?;
    if width > 256 || height > 256 {
        return None;
    }
    let mut out = header(1);
    let mut e = entry(width.max(height), png.len() as u32, 22);
    e[0] = if width >= 256 { 0 } else { width as u8 };
    e[1] = if height >= 256 { 0 } else { height as u8 };
    out.extend_from_slice(&e);
    out.extend_from_slice(png);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::{app_icons, ICON_SIZES};
    use crate::theme::palette;

    fn le16(b: &[u8], at: usize) -> u32 {
        u32::from(u16::from_le_bytes([b[at], b[at + 1]]))
    }

    fn le32(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn the_app_icon_file_has_every_size_where_its_directory_says() {
        let ico = encode(&app_icons(palette::RUST));
        assert_eq!((le16(&ico, 0), le16(&ico, 2), le16(&ico, 4)), (0, 1, 5));
        let mut end = 6 + 16 * 5;
        for (i, size) in ICON_SIZES.into_iter().enumerate() {
            let e = 6 + 16 * i;
            let side = if size == 256 { 0 } else { size };
            assert_eq!(u32::from(ico[e]), side);
            assert_eq!(u32::from(ico[e + 1]), side);
            assert_eq!(le16(&ico, e + 6), 32);
            let (len, offset) = (le32(&ico, e + 8), le32(&ico, e + 12));
            assert_eq!(offset as usize, end);
            // Header, pixels, and a mask row of whole 32-bit words.
            let mask = size.div_ceil(32) * 4 * size;
            assert_eq!(len, 40 + size * size * 4 + mask, "{size}");
            let data = &ico[offset as usize..];
            assert_eq!(le32(data, 0), 40);
            assert_eq!(le32(data, 4), size);
            assert_eq!(le32(data, 8), size * 2);
            end += len as usize;
        }
        assert_eq!(end, ico.len());
        // Bottom row first, BGRA: the 256 px icon's centre top is rust.
        let big = &ico[le32(&ico, 6 + 16 * 4 + 12) as usize + 40..];
        let top_row = 255 * 256 * 4;
        assert_eq!(big[top_row + 128 * 4..][..4], [0x1C, 0x44, 0xB5, 0xFF]);
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = PNG_MAGIC.to_vec();
        out.extend_from_slice(&13u32.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        out
    }

    #[test]
    fn png_sizes_are_read_and_non_pngs_refused() {
        assert_eq!(png_size(&png(128, 64)), Some((128, 64)));
        assert_eq!(png_size(&png(0, 64)), None);
        assert_eq!(png_size(b"GIF89a"), None);
        assert_eq!(png_size(&png(128, 64)[..20]), None);
        assert_eq!(png_size(&[]), None);
    }

    #[test]
    fn a_logo_goes_into_an_icon_as_it_is() {
        let logo = png(256, 128);
        let ico = from_png(&logo).unwrap();
        assert_eq!((le16(&ico, 2), le16(&ico, 4)), (1, 1));
        assert_eq!((ico[6], ico[7]), (0, 128));
        assert_eq!(le32(&ico, 14), logo.len() as u32);
        assert_eq!(&ico[le32(&ico, 18) as usize..], &logo[..]);
        assert!(from_png(&png(512, 512)).is_none());
        assert!(from_png(b"not a png").is_none());
    }
}
