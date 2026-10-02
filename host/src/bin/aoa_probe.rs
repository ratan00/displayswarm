#[cfg(not(windows))]
use nusb::transfer::{ControlIn, ControlType, Recipient};
#[cfg(not(windows))]
use nusb::MaybeFuture;
#[cfg(not(windows))]
use std::time::Duration;

#[cfg(not(windows))]
const AOA_GET_PROTOCOL: u8 = 51;

/// nusb offers device-level control transfers only off Windows (there a device
/// needs its interface claimed through WinUSB first), so the probe is not built.
#[cfg(windows)]
fn main() {
    eprintln!("aoa_probe is not available on Windows");
}

#[cfg(not(windows))]
fn main() {
    println!("🔍 DisplaySwarm USB AOA Hardware Prober");
    println!("===================================");

    let devices = match nusb::list_devices().wait() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[-] Failed to enumerate USB devices: {}", e);
            return;
        }
    };

    println!("[*] Scanning USB bus for connected Android devices...");
    let mut found = 0;

    for dev in devices {
        let vid = dev.vendor_id();
        let pid = dev.product_id();

        // Check for Google Accessory mode
        if vid == 0x18D1 && (0x2D00..=0x2D05).contains(&pid) {
            println!(
                "🎉 Found ACTIVE Android Open Accessory device: VID=0x{:04x}, PID=0x{:04x}",
                vid, pid
            );
            found += 1;
            continue;
        }

        // Check for Samsung or candidate Android devices
        // Samsung VID is 0x04e8
        if vid == 0x04e8 || vid == 0x18d1 || vid == 0x2717 || vid == 0x0bb4 || vid == 0x12d1 {
            println!(
                "\n📱 Found candidate Android device: Bus {} Device {:03} (VID=0x{:04x}, PID=0x{:04x})",
                dev.bus_id(),
                dev.device_address(),
                vid,
                pid
            );
            found += 1;

            match dev.open().wait() {
                Ok(handle) => {
                    println!("    [+] Successfully opened USB device handle!");

                    // Probe AOA Protocol version (Control transfer: Req 51, Type 0xC0)
                    match handle
                        .control_in(
                            ControlIn {
                                control_type: ControlType::Vendor,
                                recipient: Recipient::Device,
                                request: AOA_GET_PROTOCOL,
                                value: 0,
                                index: 0,
                                length: 2,
                            },
                            Duration::from_millis(1500),
                        )
                        .wait()
                    {
                        Ok(buf) if buf.len() == 2 => {
                            let proto_ver = u16::from_le_bytes([buf[0], buf[1]]);
                            println!("    [+] Android Open Accessory (AOA) Protocol Version: v{}", proto_ver);
                            if proto_ver >= 1 {
                                println!("    ✅ SUCCESS: This Samsung device fully supports native AOA (Plug & Play second monitor without ADB)!");
                            }
                        }
                        Ok(buf) => {
                            println!("    [-] Received unexpected protocol response length: {}", buf.len());
                        }
                        Err(e) => {
                            println!("    [-] AOA Protocol query error: {}", e);
                        }
                    }
                }
                Err(e) => {
                    println!("    [-] Failed to open device: {}", e);
                }
            }
        }
    }

    if found == 0 {
        println!("[-] No candidate Android or AOA devices found on USB bus.");
    }
    println!("\nDone.");
}
