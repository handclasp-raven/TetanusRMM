//! H.264 Annex B helpers.
//!
//! A viewer can only start decoding at a keyframe (IDR) that is preceded by
//! the sequence and picture parameter sets (SPS/PPS). Encoders do not always
//! repeat SPS/PPS before every IDR, and viewers join mid-stream, so the agent
//! remembers the latest SPS/PPS and prepends them to any keyframe that lacks
//! them.

pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// Split an Annex B stream into NAL units (without start codes).
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let mut end = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
            // A 4-byte start code leaves a trailing zero on the previous unit.
            while end > start && n + 1 < starts.len() && data[end - 1] == 0 {
                end -= 1;
            }
            &data[start..end]
        })
        .filter(|nal| !nal.is_empty())
        .collect()
}

pub fn nal_type(nal: &[u8]) -> u8 {
    nal[0] & 0x1f
}

/// Whether the access unit contains an IDR slice.
pub fn is_keyframe(data: &[u8]) -> bool {
    nal_units(data).iter().any(|n| nal_type(n) == NAL_IDR)
}

/// Remembers the latest SPS/PPS and makes every keyframe self-contained.
#[derive(Debug, Default)]
pub struct ParamSets {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParamSets {
    /// Record any SPS/PPS in `frame`; if it is a keyframe without them,
    /// return it with the remembered ones prepended.
    pub fn fix_up(&mut self, frame: Vec<u8>) -> Vec<u8> {
        let units = nal_units(&frame);
        let mut has_sps = false;
        let mut has_pps = false;
        let mut idr = false;
        for nal in &units {
            match nal_type(nal) {
                NAL_SPS => {
                    has_sps = true;
                    self.sps = Some(nal.to_vec());
                }
                NAL_PPS => {
                    has_pps = true;
                    self.pps = Some(nal.to_vec());
                }
                NAL_IDR => idr = true,
                _ => {}
            }
        }
        if !idr || (has_sps && has_pps) {
            return frame;
        }
        let mut out = Vec::with_capacity(frame.len() + 64);
        for set in [&self.sps, &self.pps].into_iter().flatten() {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(set);
        }
        out.extend_from_slice(&frame);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x42, 0xc0, 0x1f];
    const PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];
    const IDR: &[u8] = &[0x65, 0x88, 0x84];
    const P: &[u8] = &[0x41, 0x9a, 0x02];

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| [&[0u8, 0, 0, 1][..], n].concat())
            .collect()
    }

    #[test]
    fn splits_with_three_and_four_byte_start_codes() {
        let mut data = vec![0, 0, 0, 1];
        data.extend_from_slice(SPS);
        data.extend_from_slice(&[0, 0, 1]);
        data.extend_from_slice(PPS);
        data.extend_from_slice(&[0, 0, 0, 1]);
        data.extend_from_slice(IDR);
        assert_eq!(nal_units(&data), vec![SPS, PPS, IDR]);
        assert!(is_keyframe(&data));
        assert!(!is_keyframe(&annexb(&[P])));
        assert!(nal_units(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn keyframes_without_parameter_sets_get_them_prepended() {
        let mut sets = ParamSets::default();
        let first = annexb(&[SPS, PPS, IDR]);
        assert_eq!(sets.fix_up(first.clone()), first);
        assert_eq!(sets.fix_up(annexb(&[P])), annexb(&[P]));
        // A later IDR without SPS/PPS becomes decodable on its own.
        assert_eq!(sets.fix_up(annexb(&[IDR])), annexb(&[SPS, PPS, IDR]));
    }

    #[test]
    fn newer_parameter_sets_replace_old_ones() {
        let mut sets = ParamSets::default();
        let sps2 = &[0x67, 0x42, 0xc0, 0x28][..];
        sets.fix_up(annexb(&[SPS, PPS, IDR]));
        sets.fix_up(annexb(&[sps2, PPS, IDR]));
        assert_eq!(sets.fix_up(annexb(&[IDR])), annexb(&[sps2, PPS, IDR]));
    }
}
