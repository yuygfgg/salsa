//! Audio output device enumeration shared by the Salsa applications.
//!
//! GStreamer's `DeviceMonitor` is the common source of truth. Apps map the
//! resulting [`DeviceEntry`] into their own display models; they no longer
//! reimplement the monitor, caps parsing or channel-limit logic.

use gst::glib;
use gst::prelude::*;
use gstreamer as gst;

/// A sink device discovered through the GStreamer device monitor.
#[derive(Clone)]
pub struct DeviceEntry {
    pub display_name: String,
    pub unique_id: Option<String>,
    pub is_default: bool,
    pub transport: Option<String>,
    pub max_channels: Option<u32>,
    pub device: gst::Device,
}

/// Enumerate audio sink devices.
pub fn enumerate() -> Result<Vec<DeviceEntry>, glib::BoolError> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Audio/Sink"), Some(&gst::Caps::new_any()));
    monitor.start()?;

    let devices = monitor
        .devices()
        .into_iter()
        .map(|device| {
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
            DeviceEntry {
                display_name: device.display_name().to_string(),
                unique_id,
                is_default,
                transport,
                max_channels: device_max_channels(&device),
                device,
            }
        })
        .collect();
    monitor.stop();
    Ok(devices)
}

/// Enumerate every audio sink, falling back to an empty list on monitor error.
pub fn enumerate_or_default() -> Vec<DeviceEntry> {
    enumerate().unwrap_or_default()
}

/// The highest channel count advertised by a device.
///
/// The device caps come from the device provider and describe the real
/// hardware limit. Only when they are missing do we inspect the sink element's
/// template, which can be more optimistic.
pub fn device_max_channels(device: &gst::Device) -> Option<u32> {
    if let Some(max) = device.caps().as_ref().and_then(max_channels_from_caps) {
        return Some(max);
    }
    device.create_element(None).ok().and_then(|element| {
        let pad = element.static_pad("sink")?;
        max_channels_from_caps(&pad.query_caps(None))
    })
}

/// The highest `channels` value across the caps structures.
pub fn max_channels_from_caps(caps: &gst::Caps) -> Option<u32> {
    let mut maximum: Option<u32> = None;
    for structure in caps.iter() {
        if let Ok(channels) = structure.get::<i32>("channels") {
            maximum = maximum.max(Some(channels.max(0) as u32));
        } else if let Ok(range) = structure.get::<gst::IntRange<i32>>("channels") {
            maximum = maximum.max(Some(range.max().max(0) as u32));
        }
    }
    maximum
}
