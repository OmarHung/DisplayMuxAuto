//! Reads how each external display is physically attached on macOS.
//!
//! Every display transport IOKit exposes (`IOPortTransportStateDisplayPort`)
//! hangs off the physical connector it runs through (`Port-HDMI@1`,
//! `Port-USB-C@2`, optionally via a Thunderbolt `CIO` tunnel) and carries the
//! raw EDID the display sent over that path. Pairing the two gives both ends
//! of the connection without touching DDC/CI.

use std::ffi::{c_char, CStr};

use core_foundation::{
    array::CFArray,
    base::{CFType, TCFType},
    data::CFData,
    string::CFString,
};
use io_kit_sys::{
    kIOMasterPortDefault, keys::kIOServicePlane, types::io_object_t, IOIteratorNext,
    IOObjectRelease, IORegistryEntryCreateCFProperty, IORegistryEntryGetLocationInPlane,
    IORegistryEntryGetName, IORegistryEntryGetParentEntry, IOServiceGetMatchingServices,
    IOServiceMatching,
};

use crate::{edid, HostOutput, MonitorConnection, MonitorFingerprint};

const DISPLAY_TRANSPORT_CLASS: &CStr = c"IOPortTransportStateDisplayPort";
const THUNDERBOLT_TUNNEL_NODE: &str = "CIO";
const PORT_NODE_PREFIX: &str = "Port-";
/// Transport -> (optional CIO tunnel) -> port; a little slack for firmware
/// that inserts an extra node.
const MAX_PARENT_HOPS: usize = 4;
/// `io_name_t` is a fixed 128-byte C string.
const IO_NAME_LENGTH: usize = 128;
const KERN_SUCCESS: i32 = 0;

/// One live display link found in the registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayLink {
    pub edid: Vec<u8>,
    pub port_name: String,
    pub via_thunderbolt: bool,
    pub port_transports: Vec<String>,
}

impl DisplayLink {
    pub(crate) fn connection(&self) -> MonitorConnection {
        MonitorConnection::classify(
            host_output(&self.port_name, self.via_thunderbolt),
            Some(self.port_name.clone()),
            edid::sink_interface(&self.edid),
            shares_usb_data(&self.port_transports),
        )
    }
}

fn host_output(port_name: &str, via_thunderbolt: bool) -> Option<HostOutput> {
    let kind = port_name
        .strip_prefix(PORT_NODE_PREFIX)?
        .split('@')
        .next()
        .unwrap_or_default();
    match kind {
        "HDMI" => Some(HostOutput::Hdmi),
        "USB-C" if via_thunderbolt => Some(HostOutput::Thunderbolt),
        "USB-C" => Some(HostOutput::UsbC),
        "DisplayPort" | "DP" => Some(HostOutput::DisplayPort),
        _ => None,
    }
}

/// USB 3 data on the same connector means a hub, dock or hub-equipped USB-C
/// monitor sits on the other end. USB 2 alone is not counted: a plain
/// USB-C video cable may still negotiate it.
fn shares_usb_data(port_transports: &[String]) -> bool {
    port_transports.iter().any(|transport| transport == "USB3")
}

/// The connection of the display whose EDID identity is exactly
/// `fingerprint`. `None` when no link matches or several do (identical
/// monitors without serial numbers), so a connection is never misattributed.
pub(crate) fn connection_for(
    links: &[DisplayLink],
    fingerprint: &MonitorFingerprint,
    identity: impl Fn(&[u8]) -> Option<MonitorFingerprint>,
) -> Option<MonitorConnection> {
    let mut matches = links.iter().filter(|link| {
        identity(&link.edid).is_some_and(|candidate| candidate.matches_exactly(fingerprint))
    });
    let first = matches.next()?;
    matches.next().is_none().then(|| first.connection())
}

/// Every display transport in the registry that currently carries an EDID.
/// Failures are logged and yield an empty list: connection details are
/// advisory and must never block monitor enumeration.
pub(crate) fn display_links() -> Vec<DisplayLink> {
    let mut iterator: io_object_t = 0;
    // SAFETY: IOServiceMatching returns a +1 CFDictionary (or null) that
    // IOServiceGetMatchingServices always consumes; `iterator` is a valid
    // out-pointer.
    let status = unsafe {
        IOServiceGetMatchingServices(
            kIOMasterPortDefault,
            IOServiceMatching(DISPLAY_TRANSPORT_CLASS.as_ptr()),
            &mut iterator,
        )
    };
    if status != KERN_SUCCESS {
        tracing::warn!(status, "unable to enumerate macOS display transports");
        return Vec::new();
    }
    let iterator = RegistryObject(iterator);
    std::iter::from_fn(|| {
        // SAFETY: `iterator` is a live io_iterator_t owned by this function.
        let entry = unsafe { IOIteratorNext(iterator.0) };
        (entry != 0).then(|| RegistryObject(entry))
    })
    .filter_map(|transport| read_link(&transport))
    .collect()
}

fn read_link(transport: &RegistryObject) -> Option<DisplayLink> {
    let edid = transport
        .property("EDID")?
        .downcast::<CFData>()?
        .bytes()
        .to_vec();
    let mut via_thunderbolt = false;
    let mut current = transport.parent()?;
    for _ in 0..MAX_PARENT_HOPS {
        let name = current.name()?;
        if name.starts_with(PORT_NODE_PREFIX) {
            let port_transports = current
                .property("TransportsActive")
                .and_then(|value| value.downcast::<CFArray>())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|item| {
                            // SAFETY: items borrowed from a live CFArray are
                            // valid CFTypeRefs; get rule retains them.
                            let item = unsafe { CFType::wrap_under_get_rule(*item) };
                            item.downcast::<CFString>().map(|text| text.to_string())
                        })
                        .collect()
                })
                .unwrap_or_default();
            return Some(DisplayLink {
                edid,
                port_name: name,
                via_thunderbolt,
                port_transports,
            });
        }
        via_thunderbolt |= name == THUNDERBOLT_TUNNEL_NODE;
        current = current.parent()?;
    }
    None
}

/// Owns one IOKit object reference and releases it on drop.
struct RegistryObject(io_object_t);

impl RegistryObject {
    fn parent(&self) -> Option<Self> {
        let mut parent: io_object_t = 0;
        // SAFETY: `self.0` is a live registry entry; the returned parent is
        // retained on success and owned by the new RegistryObject.
        let status = unsafe { IORegistryEntryGetParentEntry(self.0, kIOServicePlane, &mut parent) };
        (status == KERN_SUCCESS && parent != 0).then(|| Self(parent))
    }

    fn name(&self) -> Option<String> {
        let mut buffer = [0 as c_char; IO_NAME_LENGTH];
        // SAFETY: `buffer` is exactly io_name_t sized; IOKit NUL-terminates it.
        let status = unsafe { IORegistryEntryGetName(self.0, buffer.as_mut_ptr()) };
        if status != KERN_SUCCESS {
            return None;
        }
        // SAFETY: IORegistryEntryGetName wrote a NUL-terminated string into
        // the buffer on success.
        let name = unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        Some(match self.location() {
            Some(location) => format!("{name}@{location}"),
            None => name,
        })
    }

    /// The `@…` suffix ioreg shows, e.g. `2` for `Port-USB-C@2`.
    fn location(&self) -> Option<String> {
        let mut buffer = [0 as c_char; IO_NAME_LENGTH];
        // SAFETY: `buffer` is io_name_t sized and `kIOServicePlane` is a
        // static NUL-terminated plane name.
        let status = unsafe {
            IORegistryEntryGetLocationInPlane(self.0, kIOServicePlane, buffer.as_mut_ptr())
        };
        if status != KERN_SUCCESS {
            return None;
        }
        // SAFETY: NUL-terminated by IOKit on success.
        let location = unsafe { CStr::from_ptr(buffer.as_ptr()) };
        Some(location.to_string_lossy().into_owned()).filter(|value| !value.is_empty())
    }

    fn property(&self, key: &'static str) -> Option<CFType> {
        let key = CFString::from_static_string(key);
        // SAFETY: `self.0` is a live registry entry and `key` outlives the
        // call; the returned value follows the create rule.
        let value = unsafe {
            IORegistryEntryCreateCFProperty(self.0, key.as_concrete_TypeRef(), std::ptr::null(), 0)
        };
        // SAFETY: non-null result of a Create function, owned by us.
        (!value.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(value) })
    }
}

impl Drop for RegistryObject {
    fn drop(&mut self) {
        // SAFETY: this wrapper holds exactly one reference to the object.
        unsafe {
            IOObjectRelease(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DdcRisk, SinkInterface};

    fn link(port_name: &str, via_thunderbolt: bool, transports: &[&str]) -> DisplayLink {
        DisplayLink {
            edid: Vec::new(),
            port_name: port_name.to_owned(),
            via_thunderbolt,
            port_transports: transports.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    fn fingerprint(serial: &str) -> MonitorFingerprint {
        MonitorFingerprint::new("AOC", "2402", Some(serial))
    }

    #[test]
    fn port_node_names_map_to_host_outputs() {
        assert_eq!(host_output("Port-HDMI@1", false), Some(HostOutput::Hdmi));
        assert_eq!(host_output("Port-USB-C@2", false), Some(HostOutput::UsbC));
        assert_eq!(
            host_output("Port-USB-C@1", true),
            Some(HostOutput::Thunderbolt)
        );
        assert_eq!(host_output("Port-MagSafe 3@1", false), None);
        assert_eq!(host_output("DisplayPort", false), None);
    }

    #[test]
    fn only_usb3_on_the_same_port_counts_as_shared_usb_data() {
        assert!(shares_usb_data(
            &link(
                "Port-USB-C@2",
                false,
                &["CC", "USB3", "USB2", "DisplayPort"]
            )
            .port_transports
        ));
        assert!(!shares_usb_data(
            &link("Port-USB-C@3", false, &["CC", "USB2", "DisplayPort"]).port_transports
        ));
    }

    #[test]
    fn usb_c_hub_into_the_captured_hdmi_monitor_is_an_elevated_risk_conversion() {
        let mut hub = link(
            "Port-USB-C@2",
            false,
            &["CC", "USB3", "USB2", "DisplayPort"],
        );
        hub.edid = vec![0; 128];
        let connection = MonitorConnection::classify(
            host_output(&hub.port_name, hub.via_thunderbolt),
            Some(hub.port_name.clone()),
            Some(SinkInterface::Hdmi),
            shares_usb_data(&hub.port_transports),
        );
        assert_eq!(connection.host_output, Some(HostOutput::UsbC));
        assert!(connection.shares_usb_data);
        assert_eq!(connection.ddc_risk, Some(DdcRisk::Elevated));
    }

    #[test]
    fn connection_is_attributed_only_to_the_exactly_matching_monitor() {
        let mut first = link("Port-USB-C@2", false, &[]);
        first.edid = vec![1];
        let mut second = link("Port-USB-C@3", false, &[]);
        second.edid = vec![2];
        let identity = |edid: &[u8]| Some(fingerprint(if edid == [1] { "232" } else { "2885" }));

        let found = connection_for(&[first, second], &fingerprint("2885"), identity).unwrap();

        assert_eq!(found.host_port.as_deref(), Some("Port-USB-C@3"));
    }

    #[test]
    fn ambiguous_or_unknown_monitors_get_no_connection() {
        let links = [
            link("Port-USB-C@2", false, &[]),
            link("Port-USB-C@3", false, &[]),
        ];
        let same_identity = |_: &[u8]| Some(fingerprint("232"));
        assert_eq!(
            connection_for(&links, &fingerprint("232"), same_identity),
            None
        );
        assert_eq!(
            connection_for(&links, &fingerprint("999"), same_identity),
            None
        );
        assert_eq!(
            connection_for(&links, &fingerprint("232"), |_: &[u8]| None),
            None
        );
    }
}
