//! Minimal H264 SPS (sequence parameter set) parser.
//!
//! Only what MP4 muxing needs: the coded picture dimensions. The full SPS
//! grammar (ISO/IEC 14496-10 §7.3.2.1.1) is walked just far enough to reach
//! `pic_width_in_mbs_minus1` / `pic_height_in_map_units_minus1` and the
//! cropping flags, including the high-profile prefix (scaling-matrix skip).
//!
//! No allocation-heavy constructs; a parse failure returns `None` and the
//! caller falls back to a default resolution (players honour the SPS inside
//! `avcC` for the real geometry — the tkhd dimensions are display hints).

/// Decoded picture geometry from an SPS NAL (NAL header included).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpsGeometry {
    pub width: u16,
    pub height: u16,
}

/// Bit reader over the RBSP payload (after emulation-prevention unescape).
struct BitReader<'a> {
    bits: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bits: &'a [u8]) -> Self {
        Self { bits, pos: 0 }
    }

    fn read_bit(&mut self) -> Option<u32> {
        let byte = *self.bits.get(self.pos >> 3)?;
        let bit = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Some(bit as u32)
    }

    fn read_bits(&mut self, n: u32) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            value = (value << 1) | self.read_bit()?;
        }
        Some(value)
    }

    /// Unsigned Exp-Golomb.
    fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.read_bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        // Leading one already consumed; read `zeros` suffix bits.
        if zeros == 0 {
            return Some(0);
        }
        self.read_bits(zeros).map(|suffix| (1u32 << zeros) - 1 + suffix)
    }

    /// Signed Exp-Golomb (unused offsets parse-and-skip).
    fn read_se(&mut self) -> Option<i32> {
        let ue = self.read_ue()?;
        Some(if ue % 2 == 0 {
            -((ue / 2) as i32)
        } else {
            ((ue + 1) / 2) as i32
        })
    }
}

/// Remove emulation-prevention bytes (`00 00 03` → `00 00`) from a NAL body.
fn unescape_rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zero_run = 0usize;
    for &byte in &nal[1.min(nal.len())..] {
        // A 0x03 is dropped only when preceded by two zero bytes.
        if zero_run >= 2 && byte == 0x03 {
            zero_run = 0;
            continue;
        }
        zero_run = if byte == 0 { zero_run + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// High profiles whose SPS carries the chroma/bit-depth/scaling prefix.
fn has_chroma_prefix(profile_idc: u32) -> bool {
    matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    )
}

fn skip_scaling_list(reader: &mut BitReader, size: usize) -> Option<()> {
    for _ in 0..size {
        if reader.read_bit()? == 1 {
            // Scaling list present: `size` delta-coded values (§7.3.2.1.1.1.1).
            let mut last_scale = 8i32;
            let mut next_scale = 8i32;
            for _ in 0..size {
                if next_scale != 0 {
                    let delta = reader.read_se()?;
                    next_scale = if last_scale == 0 && delta < 0 {
                        0
                    } else {
                        (last_scale + delta + 256) % 256
                    };
                    if next_scale != 0 {
                        last_scale = next_scale;
                    }
                }
            }
        }
    }
    Some(())
}

/// Parse width/height from an SPS NAL (first byte = NAL header).
pub fn parse_sps_geometry(nal: &[u8]) -> Option<SpsGeometry> {
    if nal.len() < 4 || nal[0] & 0x1F != 7 {
        return None;
    }
    let rbsp = unescape_rbsp(nal);
    let mut reader = BitReader::new(&rbsp);

    let profile_idc = reader.read_bits(8)?;
    let _constraints = reader.read_bits(8)?;
    let _level_idc = reader.read_bits(8)?;
    let _seq_parameter_set_id = reader.read_ue()?;

    if has_chroma_prefix(profile_idc) {
        let chroma_format_idc = reader.read_ue()?;
        if chroma_format_idc == 3 {
            let _separate_colour_plane_flag = reader.read_bit()?;
        }
        let _bit_depth_luma_minus8 = reader.read_ue()?;
        let _bit_depth_chroma_minus8 = reader.read_ue()?;
        let _qpprime_y_zero_transform_bypass = reader.read_bit()?;
        let seq_scaling_matrix_present = reader.read_bit()?;
        if seq_scaling_matrix_present == 1 {
            let lists = if chroma_format_idc != 3 { 8 } else { 12 };
            for index in 0..lists {
                let size = if index < 6 { 16 } else { 64 };
                skip_scaling_list(&mut reader, size)?;
            }
        }
    }

    let _log2_max_frame_num_minus4 = reader.read_ue()?;
    let pic_order_cnt_type = reader.read_ue()?;
    match pic_order_cnt_type {
        0 => {
            let _log2_max_pic_order_cnt_lsb_minus4 = reader.read_ue()?;
        }
        1 => {
            let _delta_pic_order_always_zero_flag = reader.read_bit()?;
            let _offset_for_non_ref_pic = reader.read_se()?;
            let _offset_for_top_to_bottom_field = reader.read_se()?;
            let cycles = reader.read_ue()?;
            if cycles > 255 {
                return None;
            }
            for _ in 0..cycles {
                let _offset_for_ref_frame = reader.read_se()?;
            }
        }
        _ => {}
    }

    let _max_num_ref_frames = reader.read_ue()?;
    let _gaps_in_frame_num_allowed = reader.read_bit()?;
    let pic_width_in_mbs_minus1 = reader.read_ue()?;
    let pic_height_in_map_units_minus1 = reader.read_ue()?;
    let frame_mbs_only_flag = reader.read_bit()?;
    if frame_mbs_only_flag == 0 {
        let _mb_adaptive_frame_field_flag = reader.read_bit()?;
    }
    let _direct_8x8_inference_flag = reader.read_bit()?;
    let frame_cropping_flag = reader.read_bit()?;
    let (crop_left, crop_right, crop_top, crop_bottom) = if frame_cropping_flag == 1 {
        (
            reader.read_ue()?,
            reader.read_ue()?,
            reader.read_ue()?,
            reader.read_ue()?,
        )
    } else {
        (0, 0, 0, 0)
    };

    // 4:2:0 crop units (2 luma samples per crop unit horizontally, and
    // vertically when not frame-mbs-only). Inputs beyond u16 geometry are
    // rejected rather than wrapped.
    let width_mbs = pic_width_in_mbs_minus1.checked_add(1)?.checked_mul(16)?;
    let height_map = pic_height_in_map_units_minus1.checked_add(1)?;
    let height_units = height_map.checked_mul(16)?;
    let height_mbs = height_units.checked_mul(if frame_mbs_only_flag == 1 {
        1
    } else {
        2
    })?;
    if width_mbs > u16::MAX as u32 || height_mbs > u16::MAX as u32 {
        return None;
    }
    let width = width_mbs as u16
        - (crop_left as u16) * 2
        - (crop_right as u16) * 2;
    let height = height_mbs as u16
        - (crop_top as u16) * 2 * (if frame_mbs_only_flag == 1 { 1 } else { 2 })
        - (crop_bottom as u16) * 2 * (if frame_mbs_only_flag == 1 { 1 } else { 2 });
    if width == 0 || height == 0 {
        return None;
    }
    Some(SpsGeometry { width, height })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NAL header (type 7, nal_ref_idc 3) + SPS body for a baseline stream.
    fn sps_nal(profile: u32, width_mbs: u32, height_mbs: u32, crop: bool) -> Vec<u8> {
        let mut nal = vec![0x67]; // (3<<5)|7
        nal.extend(super::fixtures::sps_body(profile, width_mbs, height_mbs, crop));
        nal
    }

    #[test]
    fn baseline_640x480() {
        let nal = sps_nal(66, 40, 30, false); // 40*16 x 30*16
        let geo = parse_sps_geometry(&nal).expect("parses");
        assert_eq!(geo.width, 640);
        assert_eq!(geo.height, 480);
    }

    #[test]
    fn baseline_with_crop() {
        // (64*16 - 16) x (36*16 - 8)
        let nal = sps_nal(66, 64, 36, true);
        let geo = parse_sps_geometry(&nal).expect("parses");
        assert_eq!(geo.width, 1008);
        assert_eq!(geo.height, 568);
    }

    #[test]
    fn high_profile_1280x720() {
        let nal = sps_nal(100, 80, 45, false); // 80*16 x 45*16
        let geo = parse_sps_geometry(&nal).expect("parses");
        assert_eq!(geo.width, 1280);
        assert_eq!(geo.height, 720);
    }

    #[test]
    fn rejects_non_sps_and_truncated() {
        assert!(parse_sps_geometry(&[0x41]).is_none());
        // Header + profile bytes only: the width/height fields cannot fit.
        let mut nal = sps_nal(66, 40, 30, false);
        nal.truncate(4);
        assert!(parse_sps_geometry(&nal).is_none());
    }

    #[test]
    fn unescape_drops_emulation_prevention() {
        // 00 00 03 xx → 00 00 xx, header byte preserved.
        let nal = vec![0x67, 0x00, 0x00, 0x03, 0xAB];
        let rbsp = unescape_rbsp(&nal);
        assert_eq!(rbsp, vec![0x00, 0x00, 0xAB]);
    }
}

/// Bit-exact SPS builders shared with the muxer tests (`cfg(test)` only —
/// never compiled into the release build).
#[cfg(test)]
pub(crate) mod fixtures {
    struct BitWriter {
        bytes: Vec<u8>,
        pos: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                pos: 0,
            }
        }

        fn put_bit(&mut self, bit: u32) {
            if self.pos % 8 == 0 {
                self.bytes.push(0);
            }
            if bit != 0 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 1 << (7 - (self.pos & 7));
            }
            self.pos += 1;
        }

        fn put_bits(&mut self, value: u32, n: u32) {
            for shift in (0..n).rev() {
                self.put_bit((value >> shift) & 1);
            }
        }

        fn put_ue(&mut self, mut value: u32) {
            value += 1;
            let mut bits = 0u32;
            let mut v = value;
            while v > 1 {
                bits += 1;
                v >>= 1;
            }
            for _ in 0..bits {
                self.put_bit(0);
            }
            for shift in (0..=bits).rev() {
                self.put_bit((value >> shift) & 1);
            }
        }

        fn finish(mut self) -> Vec<u8> {
            // RBSP trailing bits: one `1`, zeros to byte alignment.
            self.put_bit(1);
            while self.pos % 8 != 0 {
                self.put_bit(0);
            }
            self.bytes
        }
    }

    /// SPS RBSP body (no NAL header) for a synthetic stream.
    pub(crate) fn sps_body(profile: u32, width_mbs: u32, height_mbs: u32, crop: bool) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put_bits(profile, 8);
        w.put_bits(0, 8); // constraint flags
        w.put_bits(30, 8); // level
        w.put_ue(0); // sps id
        if super::has_chroma_prefix(profile) {
            w.put_ue(1); // chroma_format_idc = 4:2:0
            w.put_ue(0); // bit depth luma
            w.put_ue(0); // bit depth chroma
            w.put_bit(0); // qpprime
            w.put_bit(0); // scaling matrix absent
        }
        w.put_ue(0); // log2_max_frame_num_minus4
        w.put_ue(0); // pic_order_cnt_type = 0
        w.put_ue(0); // log2_max_pic_order_cnt_lsb_minus4
        w.put_ue(1); // max_num_ref_frames
        w.put_bit(0); // gaps allowed
        w.put_ue(width_mbs - 1);
        w.put_ue(height_mbs - 1);
        w.put_bit(1); // frame_mbs_only_flag
        w.put_bit(1); // direct_8x8
        if crop {
            // Crop 8 luma samples left/right and 4 top/bottom at 4:2:0:
            // crop units are 2 horizontally and 2 vertically for profile 66.
            w.put_bit(1);
            w.put_ue(4); // left
            w.put_ue(4); // right
            w.put_ue(2); // top
            w.put_ue(2); // bottom
        } else {
            w.put_bit(0);
        }
        w.finish()
    }

    /// Complete SPS NAL (header byte included) — muxer test fixture.
    pub(crate) fn baseline_sps(profile: u32, width_mbs: u32, height_mbs: u32, crop: bool) -> Vec<u8> {
        let mut nal = vec![0x67]; // (3<<5)|7
        nal.extend(sps_body(profile, width_mbs, height_mbs, crop));
        nal
    }
}
