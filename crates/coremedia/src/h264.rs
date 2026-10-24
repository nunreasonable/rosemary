//! What a format description needs from H.264 parameter sets: the picture
//! size an SPS codes, and the `avcC` record that carries the sets.
//!
//! The SPS syntax is ITU-T H.264 section 7.3.2.1.1; the record is
//! `AVCDecoderConfigurationRecord`, ISO/IEC 14496-15 section 5.3.3.1.

/// `nal_unit_type` values.
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_SPS_EXTENSION: u8 = 13;

/// The `nal_unit_type` of a NAL unit, from its header byte.
pub fn nal_type(unit: &[u8]) -> Option<u8> {
    unit.first().map(|header| header & 0x1f)
}

/// The fields of an SPS a format description reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u32,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    /// The decoded picture, in luma samples.
    pub coded_width: u32,
    pub coded_height: u32,
    /// The frame cropping rectangle, in luma samples from each edge.
    pub crop_left: u32,
    pub crop_right: u32,
    pub crop_top: u32,
    pub crop_bottom: u32,
}

impl Sps {
    /// The picture after cropping.
    pub fn width(&self) -> u32 {
        self.coded_width - self.crop_left - self.crop_right
    }

    pub fn height(&self) -> u32 {
        self.coded_height - self.crop_top - self.crop_bottom
    }
}

/// The profiles whose SPS carries `chroma_format_idc` and the bit depths, and
/// whose `avcC` record carries them again after the PPSs.
fn has_chroma_info(profile_idc: u8) -> bool {
    matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    )
}

/// An RBSP bit reader: the NAL unit's payload with its emulation prevention
/// bytes (the `03` of `00 00 03`) taken out.
struct Bits {
    bytes: Vec<u8>,
    at: usize,
}

impl Bits {
    fn new(unit: &[u8]) -> Bits {
        let mut bytes = Vec::with_capacity(unit.len());
        let mut zeros = 0;
        for &byte in unit {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            bytes.push(byte);
        }
        Bits { bytes, at: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.bytes.get(self.at / 8)?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(u32::from(bit))
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        (0..count).try_fold(0, |value, _| Some(value << 1 | self.bit()?))
    }

    /// `ue(v)`, Exp-Golomb.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        Some(((1u64 << zeros) - 1 + u64::from(rest)).try_into().ok()?)
    }

    /// `se(v)`.
    fn se(&mut self) -> Option<i64> {
        let code = i64::from(self.ue()?);
        Some(match code % 2 {
            1 => (code + 1) / 2,
            _ => -(code / 2),
        })
    }

    /// `scaling_list()`, read only to be stepped over.
    fn skip_scaling_list(&mut self, size: u32) -> Option<()> {
        let (mut last, mut next) = (8i64, 8i64);
        for _ in 0..size {
            if next != 0 {
                next = (last + self.se()? + 256) % 256;
            }
            last = if next == 0 { last } else { next };
        }
        Some(())
    }
}

/// Parse an SPS NAL unit, header byte included. `None` for anything that is
/// not a well-formed one.
pub fn parse_sps(unit: &[u8]) -> Option<Sps> {
    if nal_type(unit)? != NAL_SPS {
        return None;
    }
    let mut bits = Bits::new(unit.get(1..)?);
    let profile_idc = bits.bits(8)? as u8;
    let constraint_flags = bits.bits(8)? as u8;
    let level_idc = bits.bits(8)? as u8;
    let _seq_parameter_set_id = bits.ue()?;
    let (mut chroma_format_idc, mut separate_colour_plane) = (1, false);
    let (mut bit_depth_luma_minus8, mut bit_depth_chroma_minus8) = (0, 0);
    if has_chroma_info(profile_idc) {
        chroma_format_idc = bits.ue()?;
        if chroma_format_idc > 3 {
            return None;
        }
        if chroma_format_idc == 3 {
            separate_colour_plane = bits.bit()? == 1;
        }
        bit_depth_luma_minus8 = bits.ue()?;
        bit_depth_chroma_minus8 = bits.ue()?;
        let _qpprime_y_zero_transform_bypass = bits.bit()?;
        if bits.bit()? == 1 {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if bits.bit()? == 1 {
                    bits.skip_scaling_list(if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let _log2_max_frame_num_minus4 = bits.ue()?;
    match bits.ue()? {
        0 => {
            let _log2_max_pic_order_cnt_lsb_minus4 = bits.ue()?;
        }
        1 => {
            let _delta_pic_order_always_zero = bits.bit()?;
            let _offset_for_non_ref_pic = bits.se()?;
            let _offset_for_top_to_bottom_field = bits.se()?;
            for _ in 0..bits.ue()? {
                let _offset_for_ref_frame = bits.se()?;
            }
        }
        2 => {}
        _ => return None,
    }
    let _max_num_ref_frames = bits.ue()?;
    let _gaps_in_frame_num_value_allowed = bits.bit()?;
    let width_in_mbs = bits.ue()?.checked_add(1)?;
    let height_in_map_units = bits.ue()?.checked_add(1)?;
    let frame_mbs_only = bits.bit()?;
    if frame_mbs_only == 0 {
        let _mb_adaptive_frame_field = bits.bit()?;
    }
    let _direct_8x8_inference = bits.bit()?;
    let coded_width = width_in_mbs.checked_mul(16)?;
    let coded_height = height_in_map_units
        .checked_mul(16)?
        .checked_mul(2 - frame_mbs_only)?;
    let (mut crop_left, mut crop_right, mut crop_top, mut crop_bottom) = (0, 0, 0, 0);
    if bits.bit()? == 1 {
        // Equations 7-19 to 7-22: the offsets count chroma samples, or luma
        // ones where there is no chroma array.
        let chroma_array_type = if separate_colour_plane {
            0
        } else {
            chroma_format_idc
        };
        let (unit_x, unit_y) = match chroma_array_type {
            0 => (1, 2 - frame_mbs_only),
            1 => (2, 2 * (2 - frame_mbs_only)),
            2 => (2, 2 - frame_mbs_only),
            _ => (1, 2 - frame_mbs_only),
        };
        crop_left = bits.ue()?.checked_mul(unit_x)?;
        crop_right = bits.ue()?.checked_mul(unit_x)?;
        crop_top = bits.ue()?.checked_mul(unit_y)?;
        crop_bottom = bits.ue()?.checked_mul(unit_y)?;
    }
    let sps = Sps {
        profile_idc,
        constraint_flags,
        level_idc,
        chroma_format_idc,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        coded_width,
        coded_height,
        crop_left,
        crop_right,
        crop_top,
        crop_bottom,
    };
    let fits = crop_left.checked_add(crop_right)? < coded_width
        && crop_top.checked_add(crop_bottom)? < coded_height;
    fits.then_some(sps)
}

/// The parameter sets an `avcC` record holds, in its order: the SPSs, the
/// PPSs, then any SPS extensions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub length_size: u8,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
    pub sps_extensions: Vec<Vec<u8>>,
}

impl Record {
    /// The `AVCDecoderConfigurationRecord` bytes. `sps` describes the first SPS.
    pub fn encode(&self, sps: &Sps) -> Vec<u8> {
        let mut out = vec![
            1,
            sps.profile_idc,
            sps.constraint_flags,
            sps.level_idc,
            0xfc | (self.length_size - 1),
            0xe0 | self.sps.len() as u8,
        ];
        let put = |out: &mut Vec<u8>, set: &[u8]| {
            out.extend_from_slice(&(set.len() as u16).to_be_bytes());
            out.extend_from_slice(set);
        };
        for set in &self.sps {
            put(&mut out, set);
        }
        out.push(self.pps.len() as u8);
        for set in &self.pps {
            put(&mut out, set);
        }
        if has_chroma_info(sps.profile_idc) {
            out.push(0xfc | sps.chroma_format_idc as u8);
            out.push(0xf8 | sps.bit_depth_luma_minus8 as u8);
            out.push(0xf8 | sps.bit_depth_chroma_minus8 as u8);
            out.push(self.sps_extensions.len() as u8);
            for set in &self.sps_extensions {
                put(&mut out, set);
            }
        }
        out
    }

    /// Read an `AVCDecoderConfigurationRecord`.
    pub fn decode(bytes: &[u8]) -> Option<Record> {
        let (length_size, ranges, counts) = walk(bytes)?;
        let mut sets = ranges.into_iter().map(|range| bytes[range].to_vec());
        Some(Record {
            length_size,
            sps: sets.by_ref().take(counts[0]).collect(),
            pps: sets.by_ref().take(counts[1]).collect(),
            sps_extensions: sets.collect(),
        })
    }

    /// The sets in the order a parameter-set index counts them.
    pub fn sets(&self) -> impl Iterator<Item = &Vec<u8>> {
        self.sps.iter().chain(&self.pps).chain(&self.sps_extensions)
    }
}

/// Where each parameter set of an `AVCDecoderConfigurationRecord` is, in
/// index order, and the NAL unit length size it states.
pub fn locate(bytes: &[u8]) -> Option<(u8, Vec<std::ops::Range<usize>>)> {
    walk(bytes).map(|(length_size, ranges, _)| (length_size, ranges))
}

/// [`locate`], with how many of the ranges are SPSs and PPSs. The trailing
/// high-profile fields are optional: a record may end before them.
fn walk(bytes: &[u8]) -> Option<(u8, Vec<std::ops::Range<usize>>, [usize; 2])> {
    if *bytes.first()? != 1 {
        return None;
    }
    let length_size = (bytes.get(4)? & 3) + 1;
    let sps_count = usize::from(bytes.get(5)? & 0x1f);
    let mut at = 6;
    let mut ranges = Vec::new();
    let mut take = |at: &mut usize, count: usize| -> Option<()> {
        for _ in 0..count {
            let length = usize::from(u16::from_be_bytes(
                bytes.get(*at..*at + 2)?.try_into().ok()?,
            ));
            let range = *at + 2..*at + 2 + length;
            bytes.get(range.clone())?;
            ranges.push(range);
            *at += 2 + length;
        }
        Some(())
    };
    take(&mut at, sps_count)?;
    let pps_count = usize::from(*bytes.get(at)?);
    at += 1;
    take(&mut at, pps_count)?;
    if let Some(&extensions) = bytes.get(at + 3) {
        at += 4;
        take(&mut at, usize::from(extensions))?;
    }
    Some((length_size, ranges, [sps_count, pps_count]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Chromium's 480p probe stream: High profile, 848x480 in 53x30
    /// macroblocks, no cropping.
    const SPS_480P: &[u8] = &[
        0x67, 0x64, 0x00, 0x1e, 0xac, 0xd9, 0x80, 0xd4, 0x3d, 0xa1, 0x00, 0x00, 0x03, 0x00, 0x01,
        0x00, 0x00, 0x03, 0x00, 0x30, 0x8f, 0x16, 0x2d, 0x9a,
    ];
    const PPS_480P: &[u8] = &[0x68, 0xe9, 0x7b, 0xcb];

    #[test]
    fn a_high_profile_sps_gives_its_picture_size() {
        let sps = parse_sps(SPS_480P).expect("a valid SPS");
        assert_eq!((sps.profile_idc, sps.level_idc), (100, 30));
        assert_eq!(sps.chroma_format_idc, 1);
        assert_eq!((sps.width(), sps.height()), (848, 480));
    }

    /// x264's Baseline 1080p SPS: coded as 1920x1088, with eight rows cropped
    /// off the bottom - the usual way 1080 lines go in 16-line macroblocks.
    #[test]
    fn cropping_gives_the_displayed_size() {
        let sps = parse_sps(&[
            0x67, 0x42, 0xc0, 0x28, 0xd9, 0x00, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00,
            0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0x08, 0x3c, 0x60, 0xc9, 0x20,
        ])
        .expect("a valid SPS");
        assert_eq!((sps.coded_width, sps.coded_height), (1920, 1088));
        assert_eq!(sps.crop_bottom, 8);
        assert_eq!((sps.width(), sps.height()), (1920, 1080));
    }

    /// x264's High 4:2:2 SPS for 1278x718: the crop is in units of two
    /// columns and one row, which is what 4:2:2's chroma subsampling makes it.
    #[test]
    fn cropping_is_counted_in_chroma_samples() {
        let sps = parse_sps(&[
            0x67, 0x7a, 0x00, 0x1f, 0xbc, 0xd9, 0x40, 0x50, 0x05, 0xbe, 0xaf, 0x01, 0x10, 0x00,
            0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x00, 0x20, 0xf1, 0x83, 0x19, 0x60,
        ])
        .expect("a valid SPS");
        assert_eq!((sps.profile_idc, sps.chroma_format_idc), (122, 2));
        assert_eq!((sps.coded_width, sps.coded_height), (1280, 720));
        assert_eq!((sps.width(), sps.height()), (1278, 718));
    }

    #[test]
    fn a_pps_or_a_truncated_sps_is_not_an_sps() {
        assert_eq!(parse_sps(PPS_480P), None);
        assert_eq!(parse_sps(&SPS_480P[..6]), None);
        assert_eq!(parse_sps(&[]), None);
    }

    #[test]
    fn a_record_round_trips_its_sets_in_index_order() {
        let sps = parse_sps(SPS_480P).unwrap();
        let record = Record {
            length_size: 4,
            sps: vec![SPS_480P.to_vec()],
            pps: vec![PPS_480P.to_vec()],
            sps_extensions: vec![],
        };
        let bytes = record.encode(&sps);
        assert_eq!(&bytes[..6], &[1, 0x64, 0x00, 0x1e, 0xff, 0xe1]);
        assert_eq!(Record::decode(&bytes), Some(record.clone()));
        let sets: Vec<_> = record.sets().collect();
        assert_eq!(sets, [&SPS_480P.to_vec(), &PPS_480P.to_vec()]);
    }
}
