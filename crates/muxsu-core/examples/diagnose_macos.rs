//! Diagnostic tool for macOS DDC/CI detection issues.
//! Run with: cargo run -p muxsu-core --example diagnose_macos
//! Not part of the shipped app; safe to delete after diagnosis.

#[cfg(target_os = "macos")]
fn main() {
    use ddc::Ddc;
    use ddc_macos::Monitor;

    println!("== raw ddc-macos::Monitor::enumerate() ==");
    match Monitor::enumerate() {
        Ok(monitors) => {
            println!("found {} monitor(s) at the IOKit level\n", monitors.len());
            for (i, mut monitor) in monitors.into_iter().enumerate() {
                let handle = monitor.handle();
                println!("--- monitor #{i} ---");
                println!("description     : {}", monitor.description());
                println!("is_online()     : {}", handle.is_online());
                println!("is_builtin()    : {}", handle.is_builtin());
                println!("vendor_number   : {:#x}", handle.vendor_number());
                println!("model_number    : {:#x}", handle.model_number());
                println!("serial_number   : {:?}", monitor.serial_number());
                match monitor.edid() {
                    Some(edid) => println!("edid            : {} bytes", edid.len()),
                    None => println!("edid            : <none>"),
                }

                let mut caps_result = monitor.capabilities_string();
                for _ in 0..8 {
                    if caps_result.is_ok() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    caps_result = monitor.capabilities_string();
                }
                match caps_result {
                    Ok(caps) => println!(
                        "capabilities_string: OK ({} bytes)\n{}",
                        caps.len(),
                        String::from_utf8_lossy(&caps)
                    ),
                    Err(error) => println!("capabilities_string: ERROR -> {error}"),
                }

                match monitor.get_vcp_feature(0x60) {
                    Ok(value) => println!("get_vcp_feature(0x60): OK -> {:#x}", value.value()),
                    Err(error) => println!("get_vcp_feature(0x60): ERROR -> {error}"),
                }

                // Extra probes for a specific display. Reads are harmless;
                // writes only run when DIAG_INPUT_VALUES (comma-separated hex,
                // e.g. "11,12,10") is set, and stop at the first value that
                // changes the monitor's state or kills the DDC channel.
                let probe_target = std::env::var("DIAG_PROBE_MONITOR").unwrap_or_default();
                if !probe_target.is_empty() && monitor.description().contains(&probe_target) {
                    println!("\n-- read probe on {} --", monitor.description());
                    for code in [0x60_u8, 0xFD, 0xDC, 0xD6, 0xAA, 0xC8, 0xC9, 0xDF] {
                        match monitor.get_vcp_feature(code) {
                            Ok(value) => println!(
                                "  get_vcp_feature({code:#04x}): OK -> value={:#x} max={:#x}",
                                value.value(),
                                value.maximum()
                            ),
                            Err(error) => {
                                println!("  get_vcp_feature({code:#04x}): ERROR -> {error}")
                            }
                        }
                    }

                    let candidates = std::env::var("DIAG_INPUT_VALUES")
                        .ok()
                        .map(|raw| {
                            raw.split(',')
                                .filter_map(|token| u16::from_str_radix(token.trim(), 16).ok())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let baseline = monitor
                        .get_vcp_feature(0x60)
                        .ok()
                        .map(|value| value.value());
                    'candidates: for candidate in candidates {
                        println!("\n-- write probe: set_vcp_feature(0x60, {candidate:#04x}) --");
                        match monitor.set_vcp_feature(0x60, candidate) {
                            Ok(()) => println!("  set: OK"),
                            Err(error) => println!("  set: ERROR -> {error}"),
                        }
                        let start = std::time::Instant::now();
                        for _ in 0..10 {
                            std::thread::sleep(std::time::Duration::from_millis(150));
                            match monitor.get_vcp_feature(0x60) {
                                Ok(value) => {
                                    println!(
                                        "  t+{:>5}ms  read 0x60 -> {:#x}",
                                        start.elapsed().as_millis(),
                                        value.value()
                                    );
                                    if Some(value.value()) != baseline {
                                        println!("  >>> state changed from {baseline:?}; stopping");
                                        break 'candidates;
                                    }
                                }
                                Err(error) => {
                                    println!(
                                        "  t+{:>5}ms  read 0x60 -> ERROR {error}",
                                        start.elapsed().as_millis()
                                    );
                                    println!(
                                        "  >>> DDC channel lost (likely switched away); stopping"
                                    );
                                    break 'candidates;
                                }
                            }
                        }
                    }
                }
                println!();
            }
        }
        Err(error) => println!("Monitor::enumerate() failed: {error}"),
    }

    println!("== muxsu_core::macos::MacOsMonitorController (filtered) ==");
    use muxsu_core::{macos::MacOsMonitorController, MonitorControl};
    let controller = MacOsMonitorController::new();
    match controller.enumerate() {
        Ok(descriptors) => {
            println!(
                "filtered enumerate() returned {} monitor(s)",
                descriptors.len()
            );
            for descriptor in descriptors {
                println!("{descriptor:#?}");
                if std::env::var("DIAG_WRITE_INPUT_PROBE").is_ok()
                    && descriptor.name.contains("MPG")
                {
                    println!(
                        "\n-- MacOsMonitorController::write_input probe on {} --",
                        descriptor.name
                    );
                    match controller
                        .write_input(&descriptor.id, muxsu_core::DisplayInput::new(0x0F).unwrap())
                    {
                        Ok(()) => println!("write_input(0x0F): OK (verified switch took effect)"),
                        Err(error) => println!("write_input(0x0F): ERROR -> {error}"),
                    }
                }
            }
        }
        Err(error) => println!("filtered enumerate() failed: {error}"),
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("this diagnostic tool only runs on macOS");
}
