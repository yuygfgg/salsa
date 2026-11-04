//! CoreAudio output discovery through the GStreamer device monitor.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use gst::prelude::*;
use gstreamer as gst;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AudioDevice {
    /// One-based position in the displayed list.
    pub index: usize,
    pub name: String,
    pub unique_id: Option<String>,
    pub is_default: bool,
    pub transport: Option<String>,
    pub max_channels: Option<u32>,
}

impl AudioDevice {
    fn physical_selector(&self) -> Result<String> {
        if self.is_salsa_virtual() {
            bail!("the Salsa carrier device cannot be used as the physical output");
        }
        self.selector()
            .map(str::to_string)
            .context("device has no selector")
    }

    /// The value accepted by `osxaudiosink unique-id` or `--device`.
    pub fn selector(&self) -> Option<&str> {
        self.unique_id.as_deref().or(Some(self.name.as_str()))
    }

    pub fn is_salsa_virtual(&self) -> bool {
        self.unique_id
            .as_deref()
            .is_some_and(|id| id.starts_with("salsa-atmos"))
            || self.name.contains("Salsa Atmos")
    }

    pub fn description(&self) -> String {
        let mut text = self.name.clone();
        if self.is_default {
            text.push_str(" (system default)");
        }
        if let Some(transport) = &self.transport {
            text.push_str(&format!(" [{transport}]"));
        }
        if let Some(channels) = self.max_channels {
            text.push_str(&format!(" up to {channels} ch"));
        }
        text
    }
}

/// List devices that can be used as a physical output. Salsa's own virtual
/// carrier device is hidden unless `include_virtual` is set.
pub fn list_output_devices(include_virtual: bool) -> Result<Vec<AudioDevice>> {
    gst::init().context("GStreamer init failed")?;
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Audio/Sink"), Some(&gst::Caps::new_any()));
    monitor
        .start()
        .context("cannot start the GStreamer device monitor")?;

    let mut devices = Vec::new();
    for device in monitor.devices() {
        let properties = device.properties();
        let unique_id = properties
            .as_ref()
            .and_then(|p| p.get::<String>("unique-id").ok());
        let transport = properties
            .as_ref()
            .and_then(|p| p.get::<String>("transport").ok());
        let is_default = properties
            .as_ref()
            .and_then(|p| p.get::<bool>("is-default").ok())
            .unwrap_or(false);
        let device = AudioDevice {
            index: 0,
            name: device.display_name().to_string(),
            unique_id,
            is_default,
            transport,
            max_channels: max_channels(device.caps()),
        };
        devices.push(device);
    }
    monitor.stop();
    Ok(prepare_devices(devices, include_virtual))
}

fn prepare_devices(mut devices: Vec<AudioDevice>, include_virtual: bool) -> Vec<AudioDevice> {
    devices.retain(|device| {
        device.selector().is_some() && (include_virtual || !device.is_salsa_virtual())
    });
    devices.sort_by(|left, right| {
        right
            .is_default
            .cmp(&left.is_default)
            .then_with(|| left.name.cmp(&right.name))
    });
    for (index, device) in devices.iter_mut().enumerate() {
        device.index = index + 1;
    }
    devices
}

fn max_channels(caps: Option<gst::Caps>) -> Option<u32> {
    let caps = caps?;
    let mut maximum = None;
    for index in 0..caps.size() {
        if let Some(structure) = caps.structure(index)
            && let Ok(channels) = structure.get::<i32>("channels")
        {
            maximum = Some(maximum.unwrap_or(0).max(channels.max(0) as u32));
        }
    }
    maximum
}

/// Resolve a list index, unique ID, or (case-insensitive) device name to a
/// value suitable for `osxaudiosink unique-id`.
pub fn resolve_selector(query: &str) -> Result<String> {
    let devices = list_output_devices(false)?;
    resolve_from(query, &devices)
}

fn resolve_from(query: &str, devices: &[AudioDevice]) -> Result<String> {
    let query = query.trim();
    if query.is_empty() {
        bail!("no output device was selected");
    }
    if query.eq_ignore_ascii_case("system") {
        return Ok("system".to_string());
    }
    if query.eq_ignore_ascii_case("default") {
        if let Some(device) = devices.iter().find(|device| device.is_default) {
            return device.physical_selector();
        }
        bail!("no system-default output device was found");
    }
    if let Ok(index) = query.parse::<usize>()
        && let Some(device) = devices.iter().find(|device| device.index == index)
    {
        return device.physical_selector();
    }
    if let Some(device) = devices
        .iter()
        .find(|device| device.unique_id.as_deref() == Some(query))
    {
        return device.physical_selector();
    }
    let needle = query.to_lowercase();
    let exact: Vec<_> = devices
        .iter()
        .filter(|device| device.name.to_lowercase() == needle)
        .collect();
    if let [device] = exact.as_slice() {
        return device.physical_selector();
    }
    let partial: Vec<_> = devices
        .iter()
        .filter(|device| device.name.to_lowercase().contains(&needle))
        .collect();
    match partial.as_slice() {
        [device] => device.physical_selector(),
        [] => {
            let names = devices
                .iter()
                .map(|device| format!("  {}  {}", device.index, device.description()))
                .collect::<Vec<_>>()
                .join("\n");
            bail!("no output device matches '{query}':\n{names}")
        }
        _ => {
            let names = partial
                .iter()
                .map(|device| format!("  {}  {}", device.index, device.description()))
                .collect::<Vec<_>>()
                .join("\n");
            bail!("'{query}' matches several output devices:\n{names}");
        }
    }
}

/// Interactive selection. The list and prompt go to stderr; only the resolved
/// selector is written to stdout.
pub fn choose_output_device() -> Result<String> {
    choose_output_device_with_default(None)
}

/// Like [`choose_output_device`], but marks `current` as the default choice.
pub fn choose_output_device_with_default(current: Option<&str>) -> Result<String> {
    let devices = list_output_devices(false)?;
    if devices.is_empty() {
        bail!("no physical output devices were found");
    }
    let follow_system = current.is_none_or(|value| value.is_empty() || value == "system");
    let current_index = current.and_then(|current| {
        devices
            .iter()
            .find(|device| {
                device.selector() == Some(current)
                    || device.unique_id.as_deref() == Some(current)
                    || device.name == current
            })
            .map(|device| device.index)
    });
    let fallback = devices.iter().find(|device| device.is_default).or_else(|| {
        devices
            .iter()
            .find(|device| device.transport.as_deref() != Some("virt"))
    });
    let default_index = if follow_system {
        0
    } else {
        current_index
            .or_else(|| fallback.map(|device| device.index))
            .unwrap_or(devices[0].index)
    };

    let mut stderr = std::io::stderr();
    writeln!(stderr, "Available physical outputs:")?;
    writeln!(
        stderr,
        "   0. Follow the macOS system output{}",
        if follow_system { "  <- current" } else { "" }
    )?;
    for device in &devices {
        let marker = if Some(device.index) == current_index && !follow_system {
            "  <- current"
        } else if Some(device.index) == Some(default_index) {
            "  <- default"
        } else {
            ""
        };
        writeln!(
            stderr,
            "  {:>2}. {}{marker}",
            device.index,
            device.description(),
        )?;
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "no terminal is attached; pass --device <number|name|unique-id|system> (see `salsa-audiod devices`)"
        );
    }
    write!(
        stderr,
        "Choose an output [0-{}] (Enter = {}): ",
        devices.len(),
        default_index
    )?;
    stderr.flush()?;

    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let query = line.trim();
    let query = if query.is_empty() {
        default_index.to_string()
    } else {
        query.to_string()
    };
    if query == "0" || query.eq_ignore_ascii_case("system") {
        eprintln!("Selected: follow the macOS system output");
        return Ok("system".to_string());
    }
    let selected = resolve_from(&query, &devices)
        .with_context(|| format!("cannot use selection '{query}'"))?;
    let device = devices
        .iter()
        .find(|device| device.selector() == Some(selected.as_str()))
        .context("selected device disappeared")?;
    eprintln!("Selected: {} ({selected})", device.name);
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(index: usize, name: &str, id: &str, is_default: bool) -> AudioDevice {
        AudioDevice {
            index,
            name: name.to_string(),
            unique_id: Some(id.to_string()),
            is_default,
            transport: None,
            max_channels: None,
        }
    }

    #[test]
    fn resolves_index_id_and_unique_name() {
        let devices = [
            device(1, "MacBook Pro Speakers", "BuiltInSpeakerDevice", true),
            device(2, "USB DAC", "USB_DAC_UID", false),
        ];
        assert_eq!(resolve_from("1", &devices).unwrap(), "BuiltInSpeakerDevice");
        assert_eq!(
            resolve_from("USB_DAC_UID", &devices).unwrap(),
            "USB_DAC_UID"
        );
        assert_eq!(resolve_from("usb dac", &devices).unwrap(), "USB_DAC_UID");
    }

    #[test]
    fn detects_salsa_devices_and_ambiguous_names() {
        let salsa = device(1, "Salsa Atmos E-AC-3", "salsa-atmos-eac3", false);
        assert!(salsa.is_salsa_virtual());

        let devices = [
            device(1, "Output A", "a", false),
            device(2, "Output B", "b", false),
        ];
        let error = resolve_from("Output", &devices).unwrap_err().to_string();
        assert!(error.contains("several"), "{error}");
    }

    #[test]
    fn displayed_indices_exclude_virtual_outputs() {
        let devices = prepare_devices(
            vec![
                device(0, "Built-in speakers", "speakers", true),
                device(0, "Salsa Atmos", "salsa-atmos-eac3", false),
                device(0, "USB DAC", "usb", false),
            ],
            false,
        );
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[1].index, 2);
        assert_eq!(devices[1].name, "USB DAC");
        assert_eq!(resolve_from("2", &devices).unwrap(), "usb");
        assert!(resolve_from("salsa-atmos-eac3", &devices).is_err());
    }

    #[test]
    fn every_explicit_selector_rejects_a_virtual_device() {
        let devices = [device(1, "Salsa Atmos", "salsa-atmos-eac3", true)];
        for query in ["1", "default", "Salsa Atmos", "Salsa", "salsa-atmos-eac3"] {
            assert!(
                resolve_from(query, &devices)
                    .unwrap_err()
                    .to_string()
                    .contains("cannot be used")
            );
        }
    }
}
