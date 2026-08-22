pub mod decode;
pub mod mic;
pub mod preview;
pub mod ptt;
pub mod resample;

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};

pub struct InputDeviceSelection {
    pub device: cpal::Device,
    pub name: String,
    pub used_fallback: bool,
}

/// Names of input devices currently exposed by the active audio host.
pub fn input_device_names() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let mut names = host
        .input_devices()?
        .filter_map(|device| device.name().ok())
        .collect::<Vec<_>>();
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup();
    Ok(names)
}

pub fn default_input_device_name() -> Option<String> {
    cpal::default_host()
        .default_input_device()
        .and_then(|device| device.name().ok())
}

/// Resolve a saved device name, falling back to the OS default if it is no longer available.
pub fn select_input_device(preferred: Option<&str>) -> Result<InputDeviceSelection> {
    let host = cpal::default_host();
    if let Some(preferred) = preferred.filter(|name| !name.trim().is_empty()) {
        match host.input_devices() {
            Ok(mut devices) => {
                if let Some((device, name)) = devices.find_map(|device| {
                    let name = device.name().ok()?;
                    (name == preferred).then_some((device, name))
                }) {
                    return Ok(InputDeviceSelection {
                        device,
                        name,
                        used_fallback: false,
                    });
                }
            }
            Err(error) => tracing::warn!(
                "Could not enumerate input devices while resolving {:?}: {error}",
                preferred
            ),
        }
        tracing::warn!(
            "Configured input device {:?} is unavailable; using the system default",
            preferred
        );
    }

    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("No input device found"))?;
    let name = device
        .name()
        .unwrap_or_else(|_| "<unknown input device>".to_owned());
    Ok(InputDeviceSelection {
        device,
        name,
        used_fallback: preferred.is_some(),
    })
}

/// Print available audio input devices
pub fn print_devices() -> Result<()> {
    let host = cpal::default_host();

    println!("Audio host: {:?}", host.id());

    println!("\n--- Input Devices ---");
    let devices = host.input_devices()?;
    for (i, device) in devices.enumerate() {
        let name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
        let default_cfg = device.default_input_config();
        match default_cfg {
            Ok(cfg) => println!(
                "[{}] {} ({}Hz, {}ch, {:?})",
                i,
                name,
                cfg.sample_rate().0,
                cfg.channels(),
                cfg.sample_format()
            ),
            Err(_) => println!("[{}] {}", i, name),
        }
    }

    if let Some(dev) = host.default_input_device() {
        println!(
            "\nDefault input: {}",
            dev.name().unwrap_or_else(|_| "<unknown>".to_string())
        );
    }

    Ok(())
}
