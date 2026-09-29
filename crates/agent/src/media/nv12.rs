//! BGRA to NV12 conversion, region by region.
//!
//! Encoders take NV12 (a full-resolution Y plane, then interleaved U/V at half
//! resolution). The capture side keeps one persistent NV12 frame and
//! re-converts only the rectangles DXGI reports as changed, so an idle desktop
//! costs almost nothing. Colours use BT.601 limited range, which is what
//! H.264 decoders assume when the stream does not say otherwise.

/// An axis-aligned rectangle in pixels, `[left, right) x [top, bottom)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl Rect {
    pub fn full(width: u32, height: u32) -> Self {
        Self {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        }
    }

    /// Clip to `width x height` and grow to even coordinates (NV12 chroma is
    /// 2x2 subsampled, so conversion works on whole 2x2 blocks).
    pub fn aligned_within(self, width: u32, height: u32) -> Option<Self> {
        let r = Self {
            left: self.left.min(width) & !1,
            top: self.top.min(height) & !1,
            right: (self.right.min(width) + 1).min(width) & !1,
            bottom: (self.bottom.min(height) + 1).min(height) & !1,
        };
        (r.right > r.left && r.bottom > r.top).then_some(r)
    }

    pub fn area(&self) -> u64 {
        u64::from(self.right - self.left) * u64::from(self.bottom - self.top)
    }
}

/// An NV12 image. Width and height are even.
#[derive(Clone, PartialEq, Eq)]
pub struct Nv12Frame {
    pub width: u32,
    pub height: u32,
    /// `width * height` luma bytes followed by `width * height / 2`
    /// interleaved U,V bytes: the layout Media Foundation expects.
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Nv12Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Nv12Frame({}x{})", self.width, self.height)
    }
}

impl Nv12Frame {
    /// Black frame. Odd dimensions are rounded down to even.
    pub fn new(width: u32, height: u32) -> Self {
        let (width, height) = (width & !1, height & !1);
        let luma = (width * height) as usize;
        let mut data = vec![16u8; luma + luma / 2];
        data[luma..].fill(128);
        Self {
            width,
            height,
            data,
        }
    }

    pub fn y(&self, x: u32, y: u32) -> u8 {
        self.data[(y * self.width + x) as usize]
    }

    pub fn uv(&self, x: u32, y: u32) -> (u8, u8) {
        let i = (self.width * self.height + (y / 2) * self.width + (x & !1)) as usize;
        (self.data[i], self.data[i + 1])
    }

    /// Convert `rect` of a BGRA image (`pitch` bytes per row) into this frame.
    /// The BGRA image must be at least as large as this frame.
    pub fn update_from_bgra(&mut self, bgra: &[u8], pitch: usize, rect: Rect) {
        let Some(r) = rect.aligned_within(self.width, self.height) else {
            return;
        };
        let w = self.width as usize;
        let luma_len = w * self.height as usize;
        let (luma, chroma) = self.data.split_at_mut(luma_len);
        for y in (r.top..r.bottom).step_by(2) {
            let (y0, y1) = (y as usize, y as usize + 1);
            let row0 = &bgra[y0 * pitch..];
            let row1 = &bgra[y1 * pitch..];
            for x in (r.left..r.right).step_by(2) {
                let x = x as usize;
                let mut sum_u = 0i32;
                let mut sum_v = 0i32;
                for (row, yy) in [(row0, y0), (row1, y1)] {
                    for xx in [x, x + 1] {
                        let p = &row[xx * 4..xx * 4 + 3];
                        let (b, g, r) = (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
                        luma[yy * w + xx] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                        sum_u += ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                        sum_v += ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                    }
                }
                let c = (y0 / 2) * w + x;
                chroma[c] = ((sum_u + 2) / 4) as u8;
                chroma[c + 1] = ((sum_v + 2) / 4) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_bgra(w: u32, h: u32, bgr: [u8; 3]) -> Vec<u8> {
        (0..w * h)
            .flat_map(|_| [bgr[0], bgr[1], bgr[2], 255])
            .collect()
    }

    #[test]
    fn converts_primary_colours_to_bt601_limited_range() {
        for (bgr, yuv) in [
            ([0, 0, 0], (16, 128, 128)),
            ([255, 255, 255], (235, 128, 128)),
            ([0, 0, 255], (82, 90, 240)),  // red
            ([0, 255, 0], (144, 54, 34)),  // green
            ([255, 0, 0], (41, 240, 110)), // blue
        ] {
            let mut f = Nv12Frame::new(4, 4);
            f.update_from_bgra(&solid_bgra(4, 4, bgr), 16, Rect::full(4, 4));
            let (u, v) = f.uv(0, 0);
            assert_eq!((f.y(0, 0), u, v), yuv, "bgr {bgr:?}");
        }
    }

    #[test]
    fn only_the_given_rect_is_touched() {
        let mut f = Nv12Frame::new(8, 8);
        let white = solid_bgra(8, 8, [255, 255, 255]);
        f.update_from_bgra(
            &white,
            32,
            Rect {
                left: 2,
                top: 2,
                right: 4,
                bottom: 4,
            },
        );
        assert_eq!(f.y(2, 2), 235);
        assert_eq!(f.y(3, 3), 235);
        assert_eq!(f.y(0, 0), 16);
        assert_eq!(f.y(4, 4), 16);
        assert_eq!(f.y(7, 7), 16);
    }

    #[test]
    fn odd_rects_grow_to_whole_chroma_blocks_and_clip() {
        let r = Rect {
            left: 3,
            top: 1,
            right: 6,
            bottom: 3,
        };
        assert_eq!(
            r.aligned_within(8, 8),
            Some(Rect {
                left: 2,
                top: 0,
                right: 6,
                bottom: 4
            })
        );
        let big = Rect {
            left: 5,
            top: 5,
            right: 100,
            bottom: 100,
        };
        assert_eq!(
            big.aligned_within(8, 8),
            Some(Rect {
                left: 4,
                top: 4,
                right: 8,
                bottom: 8
            })
        );
        let outside = Rect {
            left: 10,
            top: 10,
            right: 20,
            bottom: 20,
        };
        assert_eq!(outside.aligned_within(8, 8), None);
    }

    #[test]
    fn pitch_padding_is_ignored() {
        // Rows padded to 24 bytes (6 pixels) for a 4-pixel-wide image.
        let mut bgra = Vec::new();
        for _ in 0..4 {
            bgra.extend(solid_bgra(4, 1, [255, 255, 255]));
            bgra.extend([0u8; 8]);
        }
        let mut f = Nv12Frame::new(4, 4);
        f.update_from_bgra(&bgra, 24, Rect::full(4, 4));
        assert!((0..4).all(|y| (0..4).all(|x| f.y(x, y) == 235)));
    }

    #[test]
    fn new_frame_is_black_and_even() {
        let f = Nv12Frame::new(5, 3);
        assert_eq!((f.width, f.height), (4, 2));
        assert_eq!(f.data.len(), 4 * 2 * 3 / 2);
        assert_eq!((f.y(0, 0), f.uv(0, 0)), (16, (128, 128)));
    }
}
