//! GStreamer playback lifecycle.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::pipeline::{
    Codec, DeviceInfo, OutputConfig, PipelineError, PipelineParts, build_pipeline,
};

#[derive(Debug, thiserror::Error)]
pub enum PlayerError {
    #[error("{0}")]
    Pipeline(#[from] PipelineError),
    #[error(
        "No object audio found (no JOC/OAMD metadata). This is not an Atmos stream; open it in a standard player."
    )]
    NoObjectAudio,
    #[error("playback did not preroll in time; the file may not contain JOC/OAMD object audio")]
    PrerollTimeout,
    #[error("playback failed: {0}")]
    Playback(String),
    #[error("open cancelled")]
    Cancelled,
}

pub struct Player {
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    parser: gst::Element,
    decoder: gst::Element,
    volume: gst::Element,
    codec: Codec,
}

impl Player {
    pub fn open(
        path: &Path,
        config: &OutputConfig,
        devices: &[DeviceInfo],
        start: gst::ClockTime,
        playing: bool,
        cancel: &Arc<AtomicBool>,
        require_object_audio: bool,
    ) -> Result<Self, PlayerError> {
        let parts = build_pipeline(path, config, devices)?;
        let PipelineParts {
            pipeline,
            parser,
            decoder,
            volume,
            codec,
        } = parts;
        let bus = pipeline
            .bus()
            .ok_or_else(|| PlayerError::Playback("pipeline has no bus".to_string()))?;

        if pipeline.set_state(gst::State::Paused).is_err() {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(PlayerError::Playback("cannot enter PAUSED".to_string()));
        }
        wait_for_preroll(&pipeline, &bus, cancel)?;

        if require_object_audio && !has_object_audio(&parser) {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(PlayerError::NoObjectAudio);
        }

        if start > gst::ClockTime::ZERO {
            if let Err(error) =
                pipeline.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, start)
            {
                let _ = pipeline.set_state(gst::State::Null);
                return Err(PlayerError::Playback(error.to_string()));
            }
            wait_for_preroll(&pipeline, &bus, cancel)?;
        }

        if playing && pipeline.set_state(gst::State::Playing).is_err() {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(PlayerError::Playback("cannot start playback".to_string()));
        }

        Ok(Self {
            pipeline,
            bus,
            parser,
            decoder,
            volume,
            codec,
        })
    }

    pub fn decoder(&self) -> &gst::Element {
        &self.decoder
    }

    /// Whether the parser has seen JOC/OAMD metadata. `atmos` is set by both
    /// `salsaeac3jocparse` and `salsatruehdparse`.
    pub fn object_audio(&self) -> bool {
        has_object_audio(&self.parser)
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn bus(&self) -> &gst::Bus {
        &self.bus
    }

    pub fn set_playing(&self) -> Result<(), PlayerError> {
        self.pipeline
            .set_state(gst::State::Playing)
            .map(|_| ())
            .map_err(|_| PlayerError::Playback("cannot start playback".to_string()))
    }

    pub fn set_paused(&self) -> Result<(), PlayerError> {
        self.pipeline
            .set_state(gst::State::Paused)
            .map(|_| ())
            .map_err(|_| PlayerError::Playback("cannot pause playback".to_string()))
    }

    pub fn seek(&self, position: gst::ClockTime) -> Result<(), PlayerError> {
        self.pipeline
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, position)
            .map_err(|error| PlayerError::Playback(error.to_string()))
    }

    pub fn position(&self) -> gst::ClockTime {
        self.pipeline
            .query_position::<gst::ClockTime>()
            .unwrap_or(gst::ClockTime::ZERO)
    }

    pub fn duration(&self) -> Option<gst::ClockTime> {
        self.pipeline.query_duration::<gst::ClockTime>()
    }

    pub fn set_volume(&self, value: f64) {
        self.volume.set_property("volume", value.clamp(0.0, 1.5));
    }

    pub fn state(&self) -> gst::State {
        self.pipeline.current_state()
    }

    pub fn close(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn has_object_audio(parser: &gst::Element) -> bool {
    parser
        .static_pad("src")
        .and_then(|pad| pad.current_caps())
        .and_then(|caps| {
            caps.structure(0)
                .and_then(|structure| structure.get::<bool>("atmos").ok())
        })
        == Some(true)
}

fn wait_for_preroll(
    pipeline: &gst::Pipeline,
    bus: &gst::Bus,
    cancel: &Arc<AtomicBool>,
) -> Result<(), PlayerError> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(PlayerError::Cancelled);
        }
        let (result, current, _) = pipeline.state(gst::ClockTime::from_mseconds(50));
        match result {
            Ok(gst::StateChangeSuccess::Success) => return Ok(()),
            Ok(gst::StateChangeSuccess::Async) | Ok(gst::StateChangeSuccess::NoPreroll) => {
                let _ = current;
            }
            Err(_) => {
                let _ = pipeline.set_state(gst::State::Null);
                return Err(PlayerError::Playback(
                    take_bus_error(bus).unwrap_or_else(|| "state change failed".to_string()),
                ));
            }
        }
        if Instant::now() > deadline {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(PlayerError::PrerollTimeout);
        }
    }
}

fn take_bus_error(bus: &gst::Bus) -> Option<String> {
    while let Some(message) = bus.pop_filtered(&[gst::MessageType::Error]) {
        if let gst::MessageView::Error(error) = message.view() {
            let debug = error
                .debug()
                .map(|value| value.to_string())
                .unwrap_or_default();
            return Some(if debug.is_empty() {
                error.error().to_string()
            } else {
                format!("{} ({debug})", error.error())
            });
        }
    }
    None
}
