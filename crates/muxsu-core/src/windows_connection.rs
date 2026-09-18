//! Reads how each monitor is physically attached on Windows.
//!
//! The host side comes from `WmiMonitorConnectionParams.VideoOutputTechnology`
//! (a `DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY` value). The monitor side comes
//! from the raw EDID Windows caches for the monitor's device instance under
//! `HKLM\SYSTEM\CurrentControlSet\Enum\<instance>\Device Parameters\EDID`.
//! Neither touches DDC/CI.

use crate::{edid, HostOutput, MonitorConnection};

/// `DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY` values that name an external
/// connector. Embedded/internal panels are handled as built-in elsewhere.
fn host_output(technology: u32) -> Option<HostOutput> {
    match technology {
        OUTPUT_HD15 => Some(HostOutput::Vga),
        OUTPUT_DVI => Some(HostOutput::Dvi),
        OUTPUT_HDMI => Some(HostOutput::Hdmi),
        // USB-C DisplayPort Alt Mode also reports DISPLAYPORT_EXTERNAL.
        OUTPUT_DISPLAYPORT_EXTERNAL => Some(HostOutput::DisplayPort),
        OUTPUT_DISPLAYPORT_USB_TUNNEL => Some(HostOutput::Thunderbolt),
        OUTPUT_MIRACAST | OUTPUT_INDIRECT_WIRED | OUTPUT_INDIRECT_VIRTUAL => {
            Some(HostOutput::Indirect)
        }
        _ => None,
    }
}

const OUTPUT_HD15: u32 = 0;
const OUTPUT_DVI: u32 = 4;
const OUTPUT_HDMI: u32 = 5;
const OUTPUT_DISPLAYPORT_EXTERNAL: u32 = 10;
const OUTPUT_MIRACAST: u32 = 15;
const OUTPUT_INDIRECT_WIRED: u32 = 16;
const OUTPUT_INDIRECT_VIRTUAL: u32 = 17;
/// Declared right after INDIRECT_VIRTUAL without an explicit value in wingdi.h.
const OUTPUT_DISPLAYPORT_USB_TUNNEL: u32 = 18;

/// Registry subkey (relative to HKLM) holding the cached EDID of a monitor
/// device instance such as `DISPLAY\AUS3554\5&5405411&0&UID4353`.
fn edid_registry_subkey(instance_key: &str) -> String {
    format!(r"SYSTEM\CurrentControlSet\Enum\{instance_key}\Device Parameters")
}

/// The connection for one monitor, or `None` when neither end is known.
pub(crate) fn connection(
    technology: Option<u32>,
    raw_edid: Option<&[u8]>,
) -> Option<MonitorConnection> {
    let host = technology.and_then(host_output);
    let sink = raw_edid.and_then(edid::sink_interface);
    (host.is_some() || sink.is_some()).then(|| MonitorConnection::classify(host, None, sink, false))
}

/// The cached EDID for a monitor device instance. `None` when the key or
/// value is missing; connection details are advisory, so this never fails
/// enumeration.
#[cfg(target_os = "windows")]
pub(crate) fn read_cached_edid(instance_key: &str) -> Option<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_BINARY},
    };

    let subkey = wide_null(&edid_registry_subkey(instance_key));
    let value = wide_null("EDID");
    let mut size = 0_u32;
    // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the
    // call; a null data pointer asks only for the required size.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_BINARY,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS || size == 0 {
        tracing::debug!(instance_key, status, "no cached EDID for monitor instance");
        return None;
    }
    let mut data = vec![0_u8; size as usize];
    // SAFETY: `data` is exactly `size` bytes and `size` reports its capacity.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_BINARY,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS {
        tracing::debug!(instance_key, status, "unable to read cached EDID");
        return None;
    }
    data.truncate(size as usize);
    Some(data)
}

#[cfg(target_os = "windows")]
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DdcRisk, SinkInterface};

    fn hdmi_edid() -> Vec<u8> {
        let mut edid = vec![0_u8; 128];
        edid[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        edid[18] = 1;
        edid[19] = 4;
        edid[20] = 0x80 | 0x02;
        edid[127] = 0_u8.wrapping_sub(
            edid[..127]
                .iter()
                .fold(0_u8, |sum, byte| sum.wrapping_add(*byte)),
        );
        edid
    }

    #[test]
    fn documented_output_technologies_map_to_host_connectors() {
        assert_eq!(host_output(0), Some(HostOutput::Vga));
        assert_eq!(host_output(4), Some(HostOutput::Dvi));
        assert_eq!(host_output(5), Some(HostOutput::Hdmi));
        assert_eq!(host_output(10), Some(HostOutput::DisplayPort));
        assert_eq!(host_output(18), Some(HostOutput::Thunderbolt));
        for indirect in [15, 16, 17] {
            assert_eq!(host_output(indirect), Some(HostOutput::Indirect));
        }
    }

    #[test]
    fn internal_or_unlisted_technologies_have_no_host_connector() {
        for technology in [6, 11, 13, 0x8000_0000, 0xffff_ffff, 1, 9, 12, 14] {
            assert_eq!(host_output(technology), None, "{technology:#x}");
        }
    }

    #[test]
    fn edid_lives_under_the_device_instance_parameters_key() {
        assert_eq!(
            edid_registry_subkey(r"DISPLAY\AUS3554\5&5405411&0&UID4353"),
            r"SYSTEM\CurrentControlSet\Enum\DISPLAY\AUS3554\5&5405411&0&UID4353\Device Parameters"
        );
    }

    #[test]
    fn displayport_output_into_an_hdmi_sink_is_flagged_as_converted() {
        let connection = connection(Some(10), Some(&hdmi_edid())).unwrap();
        assert_eq!(connection.host_output, Some(HostOutput::DisplayPort));
        assert_eq!(connection.sink_interface, Some(SinkInterface::Hdmi));
        assert_eq!(connection.ddc_risk, Some(DdcRisk::Elevated));
        // WMI exposes no USB topology, so this is never claimed.
        assert!(!connection.shares_usb_data);
    }

    #[test]
    fn missing_both_ends_yields_no_connection() {
        assert_eq!(connection(None, None), None);
        assert_eq!(connection(Some(0xffff_ffff), Some(&[0_u8; 16])), None);
        assert!(connection(Some(5), None).is_some());
    }
}
