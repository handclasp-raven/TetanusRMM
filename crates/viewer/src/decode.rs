//! H.264 decoding with OpenH264 (portable; builds on Linux, macOS, Windows).

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use protocol::media::MediaFrame;

/// A decoded picture as `0x00RRGGBB` pixels, ready for the window buffer.
#[derive(Clone)]
pub struct Picture {
    pub monitor: u32,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
    pub seq: u64,
}

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Picture({}x{}, monitor {}, seq {})",
            self.width, self.height, self.monitor, self.seq
        )
    }
}

impl Picture {
    pub fn pixel(&self, x: u32, y: u32) -> (u8, u8, u8) {
        let p = self.pixels[(y * self.width + x) as usize];
        ((p >> 16) as u8, (p >> 8) as u8, p as u8)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("malformed frame payload: {0}")]
    Payload(#[from] postcard::Error),
    #[error("H.264 decode failed: {0}")]
    H264(#[from] openh264::Error),
}

pub struct VideoDecoder {
    decoder: Decoder,
    /// Until a keyframe arrives there is nothing to decode against.
    waiting_for_keyframe: bool,
}

impl VideoDecoder {
    pub fn new() -> Result<Self, DecodeError> {
        Ok(Self {
            decoder: Decoder::new()?,
            waiting_for_keyframe: true,
        })
    }

    /// Decode one frame. `Ok(None)`: nothing to show yet (waiting for a
    /// keyframe, or the decoder is buffering). On `Err`, the caller should
    /// request a keyframe; frames are skipped until one arrives.
    pub fn decode(&mut self, frame: &MediaFrame) -> Result<Option<Picture>, DecodeError> {
        self.decode_as(frame, true)
    }

    /// [`VideoDecoder::decode`], but with `show` false the picture is only
    /// decoded (later frames build on it), not converted for display: for
    /// a frame a newer one will replace before it could be shown. That
    /// conversion is most of the work.
    pub fn decode_as(
        &mut self,
        frame: &MediaFrame,
        show: bool,
    ) -> Result<Option<Picture>, DecodeError> {
        if self.waiting_for_keyframe && !frame.keyframe {
            return Ok(None);
        }
        let video = frame.video()?;
        let yuv = match self.decoder.decode(&video.h264) {
            Ok(Some(yuv)) => yuv,
            Ok(None) => return Ok(None),
            Err(e) => {
                self.waiting_for_keyframe = true;
                return Err(e.into());
            }
        };
        self.waiting_for_keyframe = false;
        if !show {
            return Ok(None);
        }
        let (w, h) = yuv.dimensions();
        let mut pixels = vec![0u32; w * h];
        // SAFETY: a u32 slice viewed as its bytes: same memory, u8 has no
        // alignment requirement, and every byte pattern is a valid u32.
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(pixels.as_mut_ptr().cast::<u8>(), w * h * 4) };
        yuv.write_rgba8(bytes);
        for p in &mut pixels {
            *p = rgba_to_xrgb(*p);
        }
        Ok(Some(Picture {
            monitor: video.monitor,
            width: w as u32,
            height: h as u32,
            pixels,
            seq: frame.seq,
        }))
    }
}

/// Bytes R, G, B, A read as a native `u32` to `0x00RRGGBB`.
#[inline]
fn rgba_to_xrgb(p: u32) -> u32 {
    let [r, g, b, _] = p.to_ne_bytes();
    (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_bytes_become_window_pixels() {
        let p = u32::from_ne_bytes([0x12, 0x34, 0x56, 0xff]);
        assert_eq!(rgba_to_xrgb(p), 0x0012_3456);
    }
}
