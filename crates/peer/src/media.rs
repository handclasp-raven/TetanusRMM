//! Video sealed once for every viewer.
//!
//! The agent encodes one stream and the relay fans it out, so frames
//! cannot be sealed per viewer. Instead the agent seals each frame's
//! payload once under a *media key* and gives that key to each viewer over
//! its own sealed session (`Control::MediaKey`). The relay still sees each
//! frame's `seq` and `keyframe` header (it needs them to start viewers on a
//! keyframe); the seal authenticates them, so it cannot alter them.
//!
//! Sealed payload layout: `epoch (u32 BE) | counter (u64 BE) | ciphertext
//! | tag`, ChaCha20-Poly1305 with nonce `0^32 | counter` and associated
//! data `seq (u64 BE) | keyframe (u8)`. The counter is per key and never
//! repeats.
//!
//! Keys change ([`MediaSealer::rotate`]) whenever a viewer leaves, so what
//! follows is unreadable to it. A viewer keeps the last few keys, since
//! frames and the key announcement can arrive in either order.

use std::collections::VecDeque;

use protocol::e2e::{MediaKey, KEY_LEN};
use protocol::media::MediaFrame;
use ring::aead::{Aad, LessSafeKey, UnboundKey, CHACHA20_POLY1305};
use ring::rand::{SecureRandom, SystemRandom};

use crate::noise::nonce;

const HEADER: usize = 4 + 8;

fn aad(seq: u64, keyframe: bool) -> [u8; 9] {
    let mut aad = [0u8; 9];
    aad[..8].copy_from_slice(&seq.to_be_bytes());
    aad[8] = u8::from(keyframe);
    aad
}

fn random_key() -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    SystemRandom::new()
        .fill(&mut key)
        .expect("the system random source works");
    key
}

/// The agent's side: seals frames under the current key.
pub struct MediaSealer {
    current: MediaKey,
    key: LessSafeKey,
    counter: u64,
}

impl Default for MediaSealer {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaSealer {
    /// A fresh random key at epoch 0.
    pub fn new() -> Self {
        Self::with_key(MediaKey {
            epoch: 0,
            key: random_key(),
        })
    }

    fn with_key(current: MediaKey) -> Self {
        Self {
            key: LessSafeKey::new(
                UnboundKey::new(&CHACHA20_POLY1305, &current.key).expect("32-byte key"),
            ),
            current,
            counter: 0,
        }
    }

    /// The key to give viewers.
    pub fn key(&self) -> &MediaKey {
        &self.current
    }

    /// Switch to a new random key (the next epoch); returns it.
    pub fn rotate(&mut self) -> &MediaKey {
        *self = Self::with_key(MediaKey {
            epoch: self.current.epoch.wrapping_add(1),
            key: random_key(),
        });
        &self.current
    }

    /// Seal a frame whose payload is the plaintext `VideoPayload`.
    pub fn seal(&mut self, frame: MediaFrame) -> MediaFrame {
        let counter = self.counter;
        self.counter += 1;
        let mut payload = Vec::with_capacity(HEADER + frame.payload.len() + 16);
        payload.extend_from_slice(&self.current.epoch.to_be_bytes());
        payload.extend_from_slice(&counter.to_be_bytes());
        let mut body = frame.payload;
        self.key
            .seal_in_place_append_tag(
                nonce(counter),
                Aad::from(aad(frame.seq, frame.keyframe)),
                &mut body,
            )
            .expect("sealing cannot fail below the length limit");
        payload.extend_from_slice(&body);
        MediaFrame {
            seq: frame.seq,
            keyframe: frame.keyframe,
            payload,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
    /// Sealed under a key this viewer does not (yet) have.
    #[error("frame is sealed under unknown key epoch {0}")]
    UnknownEpoch(u32),
    #[error("frame failed authentication")]
    Unauthentic,
}

/// Keys a viewer keeps.
const KEPT_KEYS: usize = 3;

/// The viewer's side: opens frames with the keys it has been given.
#[derive(Default)]
pub struct MediaOpener {
    keys: VecDeque<(u32, LessSafeKey)>,
}

impl MediaOpener {
    pub fn add(&mut self, key: &MediaKey) {
        if self.keys.iter().any(|(epoch, _)| *epoch == key.epoch) {
            return;
        }
        if self.keys.len() == KEPT_KEYS {
            self.keys.pop_front();
        }
        self.keys.push_back((
            key.epoch,
            LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &key.key).expect("32-byte key")),
        ));
    }

    pub fn has_keys(&self) -> bool {
        !self.keys.is_empty()
    }

    /// The frame with its payload decrypted.
    pub fn open(&self, frame: &MediaFrame) -> Result<MediaFrame, OpenError> {
        let header = frame.payload.get(..HEADER).ok_or(OpenError::Unauthentic)?;
        let epoch = u32::from_be_bytes(header[..4].try_into().expect("4 bytes"));
        let counter = u64::from_be_bytes(header[4..].try_into().expect("8 bytes"));
        let key = self
            .keys
            .iter()
            .find(|(e, _)| *e == epoch)
            .map(|(_, k)| k)
            .ok_or(OpenError::UnknownEpoch(epoch))?;
        let mut body = frame.payload[HEADER..].to_vec();
        let plain_len = key
            .open_in_place(
                nonce(counter),
                Aad::from(aad(frame.seq, frame.keyframe)),
                &mut body,
            )
            .map_err(|_| OpenError::Unauthentic)?
            .len();
        body.truncate(plain_len);
        Ok(MediaFrame {
            seq: frame.seq,
            keyframe: frame.keyframe,
            payload: body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::media::VideoPayload;

    fn plain(seq: u64) -> MediaFrame {
        VideoPayload {
            monitor: 0,
            pts_us: seq * 33_333,
            width: 64,
            height: 48,
            h264: vec![0, 0, 0, 1, 0x65, seq as u8, 0xAA, 0xBB],
        }
        .to_frame(seq, seq == 0)
    }

    #[test]
    fn frames_round_trip_and_hide_their_content() {
        let mut sealer = MediaSealer::new();
        let mut opener = MediaOpener::default();
        opener.add(sealer.key());
        for seq in 0..3 {
            let sealed = sealer.seal(plain(seq));
            assert_eq!((sealed.seq, sealed.keyframe), (seq, seq == 0));
            let original = plain(seq).video().unwrap();
            assert_ne!(
                sealed.video().ok(),
                Some(original),
                "the relay cannot decode it"
            );
            assert!(
                !sealed.payload[HEADER..]
                    .windows(4)
                    .any(|w| w == [0, 0, 0, 1]),
                "no H.264 start code visible"
            );
            assert_eq!(opener.open(&sealed).unwrap(), plain(seq));
        }
    }

    #[test]
    fn the_header_the_relay_sees_cannot_be_altered() {
        let mut sealer = MediaSealer::new();
        let mut opener = MediaOpener::default();
        opener.add(sealer.key());
        let sealed = sealer.seal(plain(5));
        let mut flipped = sealed.clone();
        flipped.keyframe = !flipped.keyframe;
        assert_eq!(opener.open(&flipped), Err(OpenError::Unauthentic));
        let mut renumbered = sealed;
        renumbered.seq = 6;
        assert_eq!(opener.open(&renumbered), Err(OpenError::Unauthentic));
    }

    #[test]
    fn a_rotated_key_locks_out_viewers_that_did_not_get_it() {
        let mut sealer = MediaSealer::new();
        let (mut stays, mut leaves) = (MediaOpener::default(), MediaOpener::default());
        stays.add(sealer.key());
        leaves.add(sealer.key());
        let before = sealer.seal(plain(1));
        let next = sealer.rotate().clone();
        assert_eq!(next.epoch, 1);
        let after = sealer.seal(plain(2));

        assert_eq!(stays.open(&after), Err(OpenError::UnknownEpoch(1)));
        stays.add(&next);
        assert_eq!(stays.open(&after).unwrap(), plain(2));
        // Frames of the previous epoch still in flight still open.
        assert_eq!(stays.open(&before).unwrap(), plain(1));
        assert_eq!(leaves.open(&after), Err(OpenError::UnknownEpoch(1)));
    }

    #[test]
    fn viewers_keep_only_recent_keys() {
        let mut sealer = MediaSealer::new();
        let mut opener = MediaOpener::default();
        let first = sealer.seal(plain(0));
        opener.add(sealer.key());
        for _ in 0..KEPT_KEYS {
            opener.add(&sealer.rotate().clone());
        }
        assert_eq!(opener.open(&first), Err(OpenError::UnknownEpoch(0)));
        assert!(opener.open(&sealer.seal(plain(1))).is_ok());
    }
}
