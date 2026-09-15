//! Physical connection between a host and a monitor, inferred from two
//! independent sources: the host's output connector (IOKit / WMI) and the
//! input interface the monitor itself declares in the EDID it hands this
//! host. Everything here is a read-only hint; it never authorizes a switch.

use serde::{Deserialize, Serialize};

use crate::DisplayInput;

/// The monitor-side input interface, as declared by the EDID the host reads.
/// Many monitors expose a different EDID per input, so this usually tells
/// which *kind* of input (not which numbered port) this host is plugged into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SinkInterface {
    Hdmi,
    DisplayPort,
    Dvi,
    Vga,
    /// Digital, but the EDID does not say which interface.
    UnknownDigital,
}

/// The host-side connector the video signal leaves from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HostOutput {
    Hdmi,
    DisplayPort,
    /// DisplayPort Alt Mode on a USB-C connector.
    UsbC,
    /// DisplayPort tunnelled over Thunderbolt / USB4.
    Thunderbolt,
    Dvi,
    Vga,
    /// Rendered by software and sent over USB or the network (DisplayLink,
    /// Miracast, virtual displays); there is no DDC/CI channel at all.
    Indirect,
}

/// How likely DDC/CI is to reach the monitor over this connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DdcRisk {
    /// Same signal family end to end; DDC/CI normally passes.
    Low,
    /// The signal is converted (e.g. USB-C/DP to HDMI); many converter chips
    /// forward video but drop DDC/CI.
    Elevated,
    /// No physical DDC/CI channel exists on this path.
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorConnection {
    pub host_output: Option<HostOutput>,
    /// Platform connector label for diagnostics, e.g. `Port-USB-C@2`.
    #[serde(default)]
    pub host_port: Option<String>,
    pub sink_interface: Option<SinkInterface>,
    /// USB data is active on the same connector as the video signal: a hub,
    /// dock, or a USB-C monitor with a built-in USB hub.
    #[serde(default)]
    pub shares_usb_data: bool,
    #[serde(default)]
    pub signal_conversion: bool,
    pub ddc_risk: Option<DdcRisk>,
}

impl MonitorConnection {
    pub fn classify(
        host_output: Option<HostOutput>,
        host_port: Option<String>,
        sink_interface: Option<SinkInterface>,
        shares_usb_data: bool,
    ) -> Self {
        let signal_conversion = match (host_output, sink_interface) {
            (Some(host), Some(sink)) => is_conversion(host, sink),
            _ => false,
        };
        let ddc_risk = match (host_output, sink_interface) {
            (Some(HostOutput::Indirect), _) => Some(DdcRisk::Unsupported),
            (_, Some(SinkInterface::UnknownDigital)) | (None, _) | (_, None) => None,
            _ if signal_conversion => Some(DdcRisk::Elevated),
            _ => Some(DdcRisk::Low),
        };
        Self {
            host_output,
            host_port,
            sink_interface,
            shares_usb_data,
            signal_conversion,
            ddc_risk,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SignalFamily {
    DisplayPort,
    /// HDMI and DVI both use TMDS and pass DDC through passive adapters.
    Tmds,
    Analog,
}

fn host_family(host: HostOutput) -> Option<SignalFamily> {
    match host {
        HostOutput::DisplayPort | HostOutput::UsbC | HostOutput::Thunderbolt => {
            Some(SignalFamily::DisplayPort)
        }
        HostOutput::Hdmi | HostOutput::Dvi => Some(SignalFamily::Tmds),
        HostOutput::Vga => Some(SignalFamily::Analog),
        HostOutput::Indirect => None,
    }
}

fn sink_family(sink: SinkInterface) -> Option<SignalFamily> {
    match sink {
        SinkInterface::DisplayPort => Some(SignalFamily::DisplayPort),
        SinkInterface::Hdmi | SinkInterface::Dvi => Some(SignalFamily::Tmds),
        SinkInterface::Vga => Some(SignalFamily::Analog),
        SinkInterface::UnknownDigital => None,
    }
}

fn is_conversion(host: HostOutput, sink: SinkInterface) -> bool {
    matches!(
        (host_family(host), sink_family(sink)),
        (Some(from), Some(to)) if from != to
    )
}

/// Standard MCCS VCP 0x60 codes per interface. USB-C has no standard code.
fn standard_codes(sink: SinkInterface) -> &'static [u32] {
    match sink {
        SinkInterface::Vga => &[0x01, 0x02],
        SinkInterface::Dvi => &[0x03, 0x04],
        SinkInterface::DisplayPort => &[0x0f, 0x10],
        SinkInterface::Hdmi => &[0x11, 0x12],
        SinkInterface::UnknownDigital => &[],
    }
}

const STANDARD_VIDEO_INPUT_CODES: [u32; 8] = [0x01, 0x02, 0x03, 0x04, 0x0f, 0x10, 0x11, 0x12];

/// Whether a VCP 0x60 value is a standard MCCS input of the given interface.
/// `None` when either side is too vague to judge (vendor-specific values,
/// USB-C inputs without a standard code, or an unknown digital sink).
pub fn input_matches_sink(sink: SinkInterface, input: DisplayInput) -> Option<bool> {
    if sink == SinkInterface::UnknownDigital || !STANDARD_VIDEO_INPUT_CODES.contains(&input.value())
    {
        return None;
    }
    Some(standard_codes(sink).contains(&input.value()))
}

/// The inputs from `available` that are of the given interface kind.
pub fn candidate_inputs(sink: SinkInterface, available: &[DisplayInput]) -> Vec<DisplayInput> {
    available
        .iter()
        .copied()
        .filter(|input| standard_codes(sink).contains(&input.value()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(value: u32) -> DisplayInput {
        DisplayInput::new(value).unwrap()
    }

    #[test]
    fn usb_c_into_an_hdmi_input_is_a_conversion_with_elevated_ddc_risk() {
        let connection = MonitorConnection::classify(
            Some(HostOutput::UsbC),
            Some("Port-USB-C@3".to_owned()),
            Some(SinkInterface::Hdmi),
            false,
        );
        assert!(connection.signal_conversion);
        assert_eq!(connection.ddc_risk, Some(DdcRisk::Elevated));
    }

    #[test]
    fn thunderbolt_into_a_displayport_input_is_direct_with_low_ddc_risk() {
        let connection = MonitorConnection::classify(
            Some(HostOutput::Thunderbolt),
            None,
            Some(SinkInterface::DisplayPort),
            false,
        );
        assert!(!connection.signal_conversion);
        assert_eq!(connection.ddc_risk, Some(DdcRisk::Low));
    }

    #[test]
    fn hdmi_and_dvi_share_tmds_signalling_and_are_not_a_conversion() {
        let connection = MonitorConnection::classify(
            Some(HostOutput::Hdmi),
            None,
            Some(SinkInterface::Dvi),
            false,
        );
        assert!(!connection.signal_conversion);
        assert_eq!(connection.ddc_risk, Some(DdcRisk::Low));
    }

    #[test]
    fn digital_output_into_a_vga_input_is_a_conversion() {
        let connection = MonitorConnection::classify(
            Some(HostOutput::Hdmi),
            None,
            Some(SinkInterface::Vga),
            false,
        );
        assert!(connection.signal_conversion);
        assert_eq!(connection.ddc_risk, Some(DdcRisk::Elevated));
    }

    #[test]
    fn indirect_displays_never_have_a_ddc_channel() {
        let connection = MonitorConnection::classify(
            Some(HostOutput::Indirect),
            None,
            Some(SinkInterface::Hdmi),
            true,
        );
        assert_eq!(connection.ddc_risk, Some(DdcRisk::Unsupported));
    }

    #[test]
    fn usb_data_on_a_displayport_connection_alone_is_not_treated_as_risky() {
        // A USB-C monitor with a built-in hub looks exactly like this, and
        // DDC/CI normally works on it.
        let connection = MonitorConnection::classify(
            Some(HostOutput::UsbC),
            None,
            Some(SinkInterface::DisplayPort),
            true,
        );
        assert!(connection.shares_usb_data);
        assert!(!connection.signal_conversion);
        assert_eq!(connection.ddc_risk, Some(DdcRisk::Low));
    }

    #[test]
    fn unknown_side_leaves_conversion_and_risk_undetermined() {
        for (host, sink) in [
            (None, Some(SinkInterface::Hdmi)),
            (Some(HostOutput::UsbC), None),
            (Some(HostOutput::UsbC), Some(SinkInterface::UnknownDigital)),
        ] {
            let connection = MonitorConnection::classify(host, None, sink, false);
            assert!(!connection.signal_conversion, "{host:?} -> {sink:?}");
            assert_eq!(connection.ddc_risk, None, "{host:?} -> {sink:?}");
        }
    }

    #[test]
    fn standard_input_codes_are_matched_against_the_sink_interface() {
        assert_eq!(
            input_matches_sink(SinkInterface::Hdmi, input(0x11)),
            Some(true)
        );
        assert_eq!(
            input_matches_sink(SinkInterface::Hdmi, input(0x0f)),
            Some(false)
        );
        assert_eq!(
            input_matches_sink(SinkInterface::DisplayPort, input(0x10)),
            Some(true)
        );
        assert_eq!(
            input_matches_sink(SinkInterface::Vga, input(0x12)),
            Some(false)
        );
    }

    #[test]
    fn vendor_specific_codes_and_unknown_sinks_cannot_be_judged() {
        // 0x1b is a common vendor USB-C code, not an MCCS standard input.
        assert_eq!(
            input_matches_sink(SinkInterface::DisplayPort, input(0x1b)),
            None
        );
        assert_eq!(
            input_matches_sink(SinkInterface::UnknownDigital, input(0x11)),
            None
        );
    }

    #[test]
    fn serializes_in_the_camel_case_shape_the_settings_page_reads() {
        let connection = MonitorConnection::classify(
            Some(HostOutput::UsbC),
            Some("Port-USB-C@2".to_owned()),
            Some(SinkInterface::UnknownDigital),
            true,
        );
        assert_eq!(
            serde_json::to_value(&connection).unwrap(),
            serde_json::json!({
                "hostOutput": "usbC",
                "hostPort": "Port-USB-C@2",
                "sinkInterface": "unknownDigital",
                "sharesUsbData": true,
                "signalConversion": false,
                "ddcRisk": null,
            })
        );
    }

    #[test]
    fn candidate_inputs_keep_only_ports_of_the_same_kind() {
        let available = [0x01, 0x0f, 0x11, 0x12, 0x1b].map(input);
        assert_eq!(
            candidate_inputs(SinkInterface::Hdmi, &available),
            vec![input(0x11), input(0x12)]
        );
        assert_eq!(
            candidate_inputs(SinkInterface::DisplayPort, &available),
            vec![input(0x0f)]
        );
        assert!(candidate_inputs(SinkInterface::UnknownDigital, &available).is_empty());
    }
}
