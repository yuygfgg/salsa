#![doc = "Salsa virtual IEC 61937 audio service: carrier recovery, existing Salsa decode/render, bounded output."]

pub mod carrier;
pub mod clock;
pub mod config;
pub mod devices;
pub mod ledger;
pub mod output;
pub mod pipeline;
mod queue;
pub mod service;

pub use carrier::{
    CarrierChunk, CarrierInput, FLAG_DISCONTINUITY, FLAG_DROPPED, SHM_MAGIC, SHM_VERSION,
    SLOT_BYTES, SLOT_COUNT,
};
pub use config::FileConfig;
pub use devices::{
    AudioDevice, choose_output_device, choose_output_device_with_default, list_output_devices,
    resolve_selector,
};
pub use ledger::DelayLedger;
pub use output::{AudioInfo, OutputConfig, RenderedPcm};
pub use pipeline::{Codec, DecodeRenderPipeline, PipelineConfig, RenderMode};
pub use service::{
    CarrierSource, ComparisonReport, LifecycleReport, RenderOutcome, RenderReport, ServiceConfig,
    compare, render, render_direct,
};

/// Initialize GStreamer and the Salsa elements.
pub fn init() -> anyhow::Result<()> {
    pipeline::init()
}
