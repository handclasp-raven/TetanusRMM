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
    rgb: Vec<u8>,
}

impl VideoDecoder {
    pub fn new() -> Result<Self, DecodeError> {
        Ok(Self {
            decoder: Decoder::new()?,
            waiting_for_keyframe: true,
            rgb: Vec::new(),
        })
    }

    /// Decode one frame. `Ok(None)`: nothing to show yet (waiting for a
    /// keyframe, or the decoder is buffering). On `Err`, the caller should
    /// request a keyframe; frames are skipped until one arrives.
    pub fn decode(&mut self, frame: &MediaFrame) -> Result<Option<Picture>, DecodeError> {
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
        let (w, h) = yuv.dimensions();
        self.rgb.resize(w * h * 3, 0);
        yuv.write_rgb8(&mut self.rgb);
        let pixels = self
            .rgb
            .as_chunks::<3>()
            .0
            .iter()
            .map(|[r, g, b]| (u32::from(*r) << 16) | (u32::from(*g) << 8) | u32::from(*b))
            .collect();
        Ok(Some(Picture {
            monitor: video.monitor,
            width: w as u32,
            height: h as u32,
            pixels,
            seq: frame.seq,
        }))
    }
}
