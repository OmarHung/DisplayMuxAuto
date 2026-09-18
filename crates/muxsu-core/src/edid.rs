use crate::{MonitorResolution, ResolutionSource, SinkInterface};

const EDID_BLOCK_SIZE: usize = 128;
const EDID_HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];

pub(crate) fn is_valid(edid: &[u8]) -> bool {
    if edid.len() < EDID_BLOCK_SIZE || edid[..8] != EDID_HEADER {
        return false;
    }

    let block_count = usize::from(edid[126]) + 1;
    let Some(required_len) = block_count.checked_mul(EDID_BLOCK_SIZE) else {
        return false;
    };
    required_len <= edid.len()
        && edid[..required_len]
            .chunks_exact(EDID_BLOCK_SIZE)
            .all(|block| block.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)) == 0)
}

pub(crate) fn preferred_resolution(
    edid: Option<&[u8]>,
    core_graphics_mode: Option<MonitorResolution>,
) -> (Option<MonitorResolution>, Option<ResolutionSource>) {
    if let Some(resolution) = edid
        .filter(|value| is_valid(value))
        .and_then(max_resolution)
    {
        return (Some(resolution), Some(ResolutionSource::Edid));
    }

    (
        core_graphics_mode,
        core_graphics_mode.map(|_| ResolutionSource::CoreGraphicsDisplayMode),
    )
}

fn max_resolution(edid: &[u8]) -> Option<MonitorResolution> {
    let mut maximum = None;
    for offset in [54, 72, 90, 108] {
        consider_detailed_timing(edid, offset, &mut maximum);
    }

    for block in edid[EDID_BLOCK_SIZE..].chunks_exact(EDID_BLOCK_SIZE) {
        if block[0] != 0x02 {
            continue;
        }
        let detailed_timing_start = usize::from(block[2]);
        if !(4..=109).contains(&detailed_timing_start) {
            continue;
        }
        for offset in (detailed_timing_start..127).step_by(18) {
            consider_detailed_timing(block, offset, &mut maximum);
        }
    }
    maximum
}

fn consider_detailed_timing(block: &[u8], offset: usize, maximum: &mut Option<MonitorResolution>) {
    let Some(timing) = block.get(offset..offset + 18) else {
        return;
    };
    if timing[0] == 0 && timing[1] == 0 {
        return;
    }

    let width = u32::from(timing[2]) | (u32::from(timing[4] & 0xf0) << 4);
    let height = u32::from(timing[5]) | (u32::from(timing[7] & 0xf0) << 4);
    if width == 0 || height == 0 {
        return;
    }
    let candidate = MonitorResolution::new(width, height);
    let candidate_key = (u64::from(width) * u64::from(height), width);
    let current_key = maximum
        .map(|current| {
            (
                u64::from(current.width) * u64::from(current.height),
                current.width,
            )
        })
        .unwrap_or_default();
    if candidate_key > current_key {
        *maximum = Some(candidate);
    }
}

/// The input interface the monitor declares in this EDID. `None` when the
/// EDID is structurally invalid.
pub(crate) fn sink_interface(edid: &[u8]) -> Option<SinkInterface> {
    if !is_valid(edid) {
        return None;
    }
    let video_input = edid[VIDEO_INPUT_OFFSET];
    if video_input & DIGITAL_INPUT_FLAG == 0 {
        return Some(SinkInterface::Vga);
    }
    // EDID 1.4 added an explicit interface field; 1.3 left those bits reserved.
    let declared = (edid[EDID_REVISION_OFFSET] >= 4).then_some(video_input & 0x0f);
    if declared == Some(INTERFACE_DISPLAYPORT) {
        return Some(SinkInterface::DisplayPort);
    }
    if declares_hdmi_vendor_block(edid) {
        return Some(SinkInterface::Hdmi);
    }
    Some(match declared {
        Some(INTERFACE_DVI) => SinkInterface::Dvi,
        Some(INTERFACE_HDMI_A | INTERFACE_HDMI_B) => SinkInterface::Hdmi,
        _ => SinkInterface::UnknownDigital,
    })
}

const EDID_REVISION_OFFSET: usize = 19;
const VIDEO_INPUT_OFFSET: usize = 20;
const DIGITAL_INPUT_FLAG: u8 = 0x80;
const INTERFACE_DVI: u8 = 0x01;
const INTERFACE_HDMI_A: u8 = 0x02;
const INTERFACE_HDMI_B: u8 = 0x03;
const INTERFACE_DISPLAYPORT: u8 = 0x05;
const CTA_EXTENSION_TAG: u8 = 0x02;
const CTA_VENDOR_SPECIFIC_TAG: u8 = 3;
/// IEEE OUIs, stored little-endian in the data block.
const HDMI_LICENSING_OUI: [u8; 3] = [0x03, 0x0c, 0x00];
const HDMI_FORUM_OUI: [u8; 3] = [0xd8, 0x5d, 0xc4];

/// Whether any CTA-861 extension carries an HDMI Licensing or HDMI Forum
/// vendor-specific data block; only HDMI sinks include those.
fn declares_hdmi_vendor_block(edid: &[u8]) -> bool {
    edid[EDID_BLOCK_SIZE..]
        .chunks_exact(EDID_BLOCK_SIZE)
        .filter(|block| block[0] == CTA_EXTENSION_TAG)
        .any(|block| {
            // Data blocks occupy bytes 4..d, where d is the DTD offset.
            let end = usize::from(block[2]).clamp(4, EDID_BLOCK_SIZE - 1);
            let mut offset = 4;
            while offset < end {
                let header = block[offset];
                let length = usize::from(header & 0x1f);
                let payload = block.get(offset + 1..offset + 1 + length).unwrap_or(&[]);
                if header >> 5 == CTA_VENDOR_SPECIFIC_TAG
                    && payload.len() >= 3
                    && [HDMI_LICENSING_OUI, HDMI_FORUM_OUI]
                        .contains(&[payload[0], payload[1], payload[2]])
                {
                    return true;
                }
                offset += 1 + length;
            }
            false
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from an AOC 24B2HM2 behind a USB-C hub (EDID 1.3 + CTA-861
    /// with an HDMI 1.x VSDB).
    const AOC_24B2HM2_HDMI: &str = "00ffffffffffff0005e30224e80000000122010380361f782a9945a755509a26125054bfef00d1c0b30095008180814081c001010101023a801871382d40582c4500173a2100001e000000ff0031474151314841303030323332000000fc0032344232484d320a2020202020000000fd0030641e7318000a20202020202001cb020329f14b101f051404130312021101230907078301000067030c001000003c681a000001013064e6605980a07038144030203500173a2100001e2a4480a07038274030203500173a2100001a011d007251d01e206e285500173a2100001e8c0ad08a20e02d10103e9600173a210000180000000000000000000000000000f3";
    /// Captured from an AOC 24B2W1 on a USB-C to HDMI cable.
    const AOC_24B2W1_HDMI: &str = "00ffffffffffff0005e30224450b0000331f010380351e782a0ca5a5554ea0260e5054bfef00d1c0b30095008180814081c001010101023a801871382d40582c45000f282100001e2a4480a070382740302035000f282100001a000000fc003234423257310a202020202020000000fd00304b1e5512000a202020202020013102031ef14b101f051404130312021101230f07078301000065030c001000023a801871382d40582c45000f282100001e011d007251d01e206e2855000f282100001e8c0ad08a20e02d10103e96000f28210000188c0ad090204031200c4055000f2821000018000000000000000000000000000000000000000000000000001f";

    fn decode_hex(value: &str) -> Vec<u8> {
        (0..value.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&value[index..index + 2], 16).unwrap())
            .collect()
    }

    fn fix_checksum(block: &mut [u8]) {
        block[127] = 0_u8.wrapping_sub(
            block[..127]
                .iter()
                .fold(0_u8, |sum, byte| sum.wrapping_add(*byte)),
        );
    }

    #[test]
    fn real_hdmi_edids_with_an_hdmi_vsdb_are_hdmi_sinks() {
        for captured in [AOC_24B2HM2_HDMI, AOC_24B2W1_HDMI] {
            let edid = decode_hex(captured);
            assert!(is_valid(&edid));
            assert_eq!(sink_interface(&edid), Some(SinkInterface::Hdmi));
        }
    }

    #[test]
    fn edid_1_4_interface_field_identifies_displayport() {
        let mut edid = edid_with_timings(&[(2560, 1440)]);
        edid[18] = 1;
        edid[19] = 4;
        edid[20] = 0x80 | 0x05;
        fix_checksum(&mut edid);
        assert_eq!(sink_interface(&edid), Some(SinkInterface::DisplayPort));
    }

    #[test]
    fn edid_1_4_interface_field_identifies_dvi_and_hdmi() {
        for (field, expected) in [
            (0x01, SinkInterface::Dvi),
            (0x02, SinkInterface::Hdmi),
            (0x03, SinkInterface::Hdmi),
        ] {
            let mut edid = edid_with_timings(&[(1920, 1080)]);
            edid[18] = 1;
            edid[19] = 4;
            edid[20] = 0x80 | field;
            fix_checksum(&mut edid);
            assert_eq!(sink_interface(&edid), Some(expected));
        }
    }

    #[test]
    fn analog_edid_is_a_vga_sink() {
        let mut edid = edid_with_timings(&[(1920, 1080)]);
        edid[18] = 1;
        edid[19] = 3;
        edid[20] = 0x0e;
        fix_checksum(&mut edid);
        assert_eq!(sink_interface(&edid), Some(SinkInterface::Vga));
    }

    #[test]
    fn digital_edid_1_3_without_hdmi_blocks_is_unknown_digital() {
        let mut edid = edid_with_timings(&[(1920, 1080)]);
        edid[18] = 1;
        edid[19] = 3;
        edid[20] = 0x80;
        fix_checksum(&mut edid);
        assert_eq!(sink_interface(&edid), Some(SinkInterface::UnknownDigital));
    }

    #[test]
    fn the_hdmi_forum_vsdb_alone_also_marks_an_hdmi_sink() {
        let mut edid = decode_hex(AOC_24B2W1_HDMI);
        // Rewrite the HDMI 1.x OUI (03 0c 00) into the HDMI Forum OUI (d8 5d c4).
        let block = &mut edid[128..256];
        let position = block
            .windows(3)
            .position(|window| window == [0x03, 0x0c, 0x00])
            .unwrap();
        block[position..position + 3].copy_from_slice(&[0xd8, 0x5d, 0xc4]);
        fix_checksum(block);
        assert_eq!(sink_interface(&edid), Some(SinkInterface::Hdmi));
    }

    #[test]
    fn invalid_edid_has_no_sink_interface() {
        let mut edid = decode_hex(AOC_24B2HM2_HDMI);
        edid[20] ^= 0x01;
        assert_eq!(sink_interface(&edid), None);
    }

    fn edid_with_timings(timings: &[(u32, u32)]) -> [u8; EDID_BLOCK_SIZE] {
        let mut edid = [0_u8; EDID_BLOCK_SIZE];
        edid[..8].copy_from_slice(&EDID_HEADER);
        for (index, (width, height)) in timings.iter().take(4).enumerate() {
            let offset = 54 + index * 18;
            edid[offset] = 1;
            edid[offset + 2] = *width as u8;
            edid[offset + 4] = ((*width >> 8) as u8) << 4;
            edid[offset + 5] = *height as u8;
            edid[offset + 7] = ((*height >> 8) as u8) << 4;
        }
        edid[127] = 0_u8.wrapping_sub(
            edid[..127]
                .iter()
                .fold(0_u8, |sum, byte| sum.wrapping_add(*byte)),
        );
        edid
    }

    #[test]
    fn valid_edid_uses_the_largest_detailed_timing() {
        for expected in [(3440, 1440), (2560, 1080), (2560, 1440), (3840, 2160)] {
            let edid = edid_with_timings(&[(1920, 1080), expected]);
            let (resolution, source) =
                preferred_resolution(Some(&edid), Some(MonitorResolution::new(1280, 720)));
            assert_eq!(
                resolution,
                Some(MonitorResolution::new(expected.0, expected.1))
            );
            assert_eq!(source, Some(ResolutionSource::Edid));
        }
    }

    #[test]
    fn malformed_or_missing_edid_uses_core_graphics_physical_pixels() {
        let fallback = Some(MonitorResolution::new(3440, 1440));
        for edid in [None, Some(&[0_u8; 32][..]), Some(&[0_u8; 128][..])] {
            assert_eq!(
                preferred_resolution(edid, fallback),
                (fallback, Some(ResolutionSource::CoreGraphicsDisplayMode))
            );
        }
    }

    #[test]
    fn valid_edid_without_a_timing_uses_core_graphics_physical_pixels() {
        let edid = edid_with_timings(&[]);
        let fallback = Some(MonitorResolution::new(2560, 1440));
        assert_eq!(
            preferred_resolution(Some(&edid), fallback),
            (fallback, Some(ResolutionSource::CoreGraphicsDisplayMode))
        );
    }

    #[test]
    fn bad_checksum_invalidates_an_otherwise_well_formed_edid() {
        let mut edid = edid_with_timings(&[(3840, 2160)]);
        edid[20] = edid[20].wrapping_add(1);
        assert!(!is_valid(&edid));
    }
}
