//! H.264 samples as a decoder takes them.
//!
//! A sample buffer holds an access unit the MP4 way (`avcC`): each NAL unit
//! behind a big-endian length of 1, 2 or 4 bytes, and the parameter sets kept
//! apart in the format description. Decoders take Annex B: each NAL unit
//! behind a start code, the parameter sets in the stream. A NAL unit's bytes
//! never hold a start code (emulation prevention sees to it), so swapping
//! one framing for the other is exact.

use crate::backend::AccessUnit;

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// `nal_unit_type` of an IDR picture's slices.
const NAL_IDR: u8 = 5;

/// A sample whose lengths do not add up to its size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BadSample;

/// Append `sample`'s NAL units to `out` in Annex B form. `Ok(true)` when one
/// of them is an IDR slice.
pub(crate) fn avcc_to_annex_b(
    sample: &[u8],
    length_size: usize,
    out: &mut Vec<u8>,
) -> Result<bool, BadSample> {
    if !matches!(length_size, 1 | 2 | 4) || sample.is_empty() {
        return Err(BadSample);
    }
    let mut idr = false;
    let mut rest = sample;
    while !rest.is_empty() {
        let (length, after) = rest.split_at_checked(length_size).ok_or(BadSample)?;
        let length = length
            .iter()
            .fold(0usize, |n, &byte| n << 8 | usize::from(byte));
        let (unit, after) = after.split_at_checked(length).ok_or(BadSample)?;
        rest = after;
        // Some muxers pad with empty units; they carry nothing.
        let Some(&header) = unit.first() else {
            continue;
        };
        idr |= header & 0x1f == NAL_IDR;
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(unit);
    }
    Ok(idr)
}

/// The NAL units of an Annex B stream, without their start codes.
pub(crate) fn split_annex_b(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut at = 0;
    while at + 3 <= stream.len() {
        if stream[at..at + 3] == [0, 0, 1] {
            starts.push(at + 3);
            at += 3;
        } else {
            at += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (i, &start) in starts.iter().enumerate() {
        let mut end = starts.get(i + 1).map_or(stream.len(), |&next| next - 3);
        // The zero before a four-byte start code belongs to it, not to the
        // unit before.
        while end > start && stream[end - 1] == 0 && starts.get(i + 1).is_some() {
            end -= 1;
        }
        units.push(&stream[start..end]);
    }
    units
}

/// A session's samples turned into access units, with the parameter sets
/// put in front of the first one, and the first after each flush.
pub(crate) struct Converter {
    length_size: usize,
    parameter_sets: Vec<Vec<u8>>,
    needs_parameter_sets: bool,
}

impl Converter {
    pub fn new(length_size: u8, parameter_sets: Vec<Vec<u8>>) -> Converter {
        Converter {
            length_size: usize::from(length_size),
            parameter_sets,
            needs_parameter_sets: true,
        }
    }

    pub fn convert(&mut self, sample: &[u8]) -> Result<AccessUnit, BadSample> {
        let mut annex_b = Vec::with_capacity(sample.len() + 64);
        if self.needs_parameter_sets {
            for set in &self.parameter_sets {
                annex_b.extend_from_slice(&START_CODE);
                annex_b.extend_from_slice(set);
            }
        }
        let idr = avcc_to_annex_b(sample, self.length_size, &mut annex_b)?;
        self.needs_parameter_sets = false;
        Ok(AccessUnit { annex_b, idr })
    }

    /// The decoder was flushed and has forgotten the parameter sets.
    pub fn flushed(&mut self) {
        self.needs_parameter_sets = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLICE: &[u8] = &[0x41, 0x9a, 0x26];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x01];

    fn avcc(units: &[&[u8]], length_size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in units {
            let length = unit.len().to_be_bytes();
            out.extend_from_slice(&length[length.len() - length_size..]);
            out.extend_from_slice(unit);
        }
        out
    }

    #[test]
    fn avcc_with_length_sizes_1_2_4_becomes_annex_b() {
        let mut want = Vec::new();
        for unit in [SLICE, IDR] {
            want.extend_from_slice(&START_CODE);
            want.extend_from_slice(unit);
        }
        for length_size in [1, 2, 4] {
            let mut out = Vec::new();
            let sample = avcc(&[SLICE, IDR], length_size);
            assert_eq!(avcc_to_annex_b(&sample, length_size, &mut out), Ok(true));
            assert_eq!(out, want, "length size {length_size}");
        }
        let mut out = Vec::new();
        assert_eq!(
            avcc_to_annex_b(&avcc(&[SLICE], 4), 3, &mut out),
            Err(BadSample)
        );
    }

    #[test]
    fn truncated_length_is_bad_data() {
        let sample = avcc(&[SLICE, IDR], 4);
        for cut in [1, 3, 5, sample.len() - 1] {
            let mut out = Vec::new();
            assert_eq!(
                avcc_to_annex_b(&sample[..cut], 4, &mut out),
                Err(BadSample),
                "cut at {cut}"
            );
        }
        assert_eq!(avcc_to_annex_b(&[], 4, &mut Vec::new()), Err(BadSample));
        let mut converter = Converter::new(4, vec![vec![0x67, 1], vec![0x68, 2]]);
        assert!(converter.convert(&sample[..5]).is_err());
        assert!(
            converter
                .convert(&sample)
                .unwrap()
                .annex_b
                .starts_with(&[0, 0, 0, 1, 0x67]),
            "a bad sample does not use up the parameter sets"
        );
    }

    #[test]
    fn parameter_sets_are_prepended_after_open_and_flush() {
        let sps = vec![0x67, 0x64, 0x00];
        let pps = vec![0x68, 0xea];
        let mut converter = Converter::new(4, vec![sps.clone(), pps.clone()]);
        let sample = avcc(&[IDR], 4);
        let mut with_sets = Vec::new();
        for unit in [&sps[..], &pps[..], IDR] {
            with_sets.extend_from_slice(&START_CODE);
            with_sets.extend_from_slice(unit);
        }
        let without_sets = [&START_CODE[..], IDR].concat();
        assert_eq!(converter.convert(&sample).unwrap().annex_b, with_sets);
        assert_eq!(converter.convert(&sample).unwrap().annex_b, without_sets);
        converter.flushed();
        assert_eq!(converter.convert(&sample).unwrap().annex_b, with_sets);
        assert_eq!(converter.convert(&sample).unwrap().annex_b, without_sets);
    }

    #[test]
    fn idr_is_detected() {
        let mut converter = Converter::new(2, vec![]);
        assert!(converter.convert(&avcc(&[IDR], 2)).unwrap().idr);
        assert!(!converter.convert(&avcc(&[SLICE], 2)).unwrap().idr);
        assert!(converter.convert(&avcc(&[SLICE, IDR], 2)).unwrap().idr);
    }

    #[test]
    fn the_probe_clip_splits_into_its_eight_units() {
        let units = split_annex_b(crate::backend::PROBE_CLIP);
        let sizes: Vec<usize> = units.iter().map(|u| u.len()).collect();
        assert_eq!(sizes, [24, 6, 51, 13, 7, 46, 21, 7]);
        let types: Vec<u8> = units.iter().map(|u| u[0] & 0x1f).collect();
        assert_eq!(types, [7, 8, 5, 1, 1, 1, 1, 1]);
    }
}
