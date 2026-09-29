//! Screen streaming types.
//!
//! Video flows agent -> server -> viewers on dedicated QUIC unidirectional
//! streams (one per stream start), separate from the control stream, as a
//! sequence of framed [`MediaFrame`]s.
//!
//! The server is a relay: it routes and fans out frames but never looks
//! inside [`MediaFrame::payload`]. It only reads the small header (`seq`,
//! `keyframe`), which it needs to start each new viewer on a keyframe. That
//! split is deliberate: Phase 10 encrypts the payload end to end between
//! agent and viewer, and the relay keeps working unchanged.

use serde::{Deserialize, Serialize};

/// One display attached to the agent's desktop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// Stable within one monitor list; used to select a monitor.
    pub id: u32,
    /// OS name, e.g. `\\.\DISPLAY1`.
    pub name: String,
    /// Position on the virtual desktop.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// A unit of video on a media stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFrame {
    /// Increments per frame within one stream.
    pub seq: u64,
    /// Decoding can start here (IDR with SPS/PPS).
    pub keyframe: bool,
    /// Encoded [`VideoPayload`]. Opaque to the server.
    pub payload: Vec<u8>,
}

/// What the viewer decodes. Only agent and viewer ever see this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoPayload {
    pub monitor: u32,
    /// Capture time, microseconds since the stream started.
    pub pts_us: u64,
    pub width: u32,
    pub height: u32,
    /// One H.264 access unit, Annex B, Constrained Baseline profile.
    pub h264: Vec<u8>,
}

impl VideoPayload {
    pub fn to_frame(&self, seq: u64, keyframe: bool) -> MediaFrame {
        MediaFrame {
            seq,
            keyframe,
            payload: postcard::to_stdvec(self).expect("payload serialises"),
        }
    }
}

impl MediaFrame {
    /// Decode the payload. Only the viewer does this.
    pub fn video(&self) -> Result<VideoPayload, postcard::Error> {
        postcard::from_bytes(&self.payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{read_frame, write_frame};

    fn payload(n: u8) -> VideoPayload {
        VideoPayload {
            monitor: 1,
            pts_us: 33_333 * u64::from(n),
            width: 2560,
            height: 1440,
            h264: vec![0, 0, 0, 1, 0x65, n],
        }
    }

    #[tokio::test]
    async fn media_frames_round_trip_through_framing() {
        let frames: Vec<MediaFrame> = (0..3)
            .map(|n| payload(n).to_frame(n.into(), n == 0))
            .collect();
        let mut buf = Vec::new();
        for f in &frames {
            write_frame(&mut buf, f).await.unwrap();
        }
        let mut reader = buf.as_slice();
        for f in &frames {
            let got: MediaFrame = read_frame(&mut reader).await.unwrap().unwrap();
            assert_eq!(&got, f);
            assert_eq!(got.video().unwrap(), payload(got.seq as u8));
        }
    }

    #[test]
    fn a_large_keyframe_fits_in_one_frame() {
        let big = VideoPayload {
            h264: vec![7; 4 * 1024 * 1024],
            ..payload(0)
        };
        let frame = big.to_frame(0, true);
        assert!(frame.payload.len() < crate::MAX_FRAME_LEN as usize);
    }

    #[test]
    fn server_visible_header_is_separate_from_payload() {
        // The relay needs seq and keyframe without decoding the payload.
        let frame = MediaFrame {
            seq: 9,
            keyframe: true,
            payload: b"ciphertext in phase 10".to_vec(),
        };
        let bytes = postcard::to_stdvec(&frame).unwrap();
        let back: MediaFrame = postcard::from_bytes(&bytes).unwrap();
        assert!(back.keyframe && back.seq == 9);
        assert!(back.video().is_err());
    }
}
