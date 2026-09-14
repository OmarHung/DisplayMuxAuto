//! Diagnostic tool for macOS DDC/CI detection issues.
//! Run with: cargo run -p displaymux-core --example diagnose_macos
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

                if monitor.description().contains("MPG") {
                    println!("\n-- write+poll probe on {} --", monitor.description());
                    match monitor.set_vcp_feature(0x60, 0x0F) {
                        Ok(()) => println!("set_vcp_feature(0x60, 0x0F): OK"),
                        Err(error) => println!("set_vcp_feature(0x60, 0x0F): ERROR -> {error}"),
                    }
                    let start = std::time::Instant::now();
                    for _ in 0..20 {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        match monitor.get_vcp_feature(0x60) {
                            Ok(value) => println!(
                                "  t+{:>5}ms  get_vcp_feature(0x60): OK -> {:#x}",
                                start.elapsed().as_millis(),
                                value.value()
                            ),
                            Err(error) => println!(
                                "  t+{:>5}ms  get_vcp_feature(0x60): ERROR -> {error}",
                                start.elapsed().as_millis()
                            ),
                        }
                    }
                }
                println!();
            }
        }
        Err(error) => println!("Monitor::enumerate() failed: {error}"),
    }

    println!("== displaymux_core::macos::MacOsMonitorController (filtered) ==");
    use displaymux_core::{macos::MacOsMonitorController, MonitorControl};
    let controller = MacOsMonitorController::new();
    match controller.enumerate() {
        Ok(descriptors) => {
            println!(
                "filtered enumerate() returned {} monitor(s)",
                descriptors.len()
            );
            for descriptor in descriptors {
                println!("{descriptor:#?}");
                if descriptor.name.contains("MPG") {
                    println!(
                        "\n-- MacOsMonitorController::write_input probe on {} --",
                        descriptor.name
                    );
                    match controller
                        .write_input(&descriptor.id, displaymux_core::DisplayInput::new(0x0F).unwrap())
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
