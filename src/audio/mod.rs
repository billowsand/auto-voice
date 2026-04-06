pub mod decode;
pub mod mic;
pub mod ptt;
pub mod resample;

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};

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
