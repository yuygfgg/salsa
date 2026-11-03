#![forbid(unsafe_code)]
#![doc = "Format-independent object-to-speaker renderer."]

mod bed;
mod layout;
mod layouts;
mod renderer;
mod scene;
mod trim;

pub use scene::render_scene;
mod speaker;

pub use bed::{bed_channel_q15, layout_bed_anchor};
pub use layout::{
    AxisGroup, MAX_CHANNELS, MAX_GROUPS, MAX_POINTS, RegionGeometry, SpeakerLayout, SpeakerPoint,
    layout_by_channel_count, layout_by_name, layout_by_out_ch_config, layout_by_speaker_bitfield,
    layouts,
};
pub use renderer::{BLOCK_SAMPLES, RenderError, SpeakerMetadata, SpeakerRenderer};
pub use speaker::SpeakerChannel;
pub use trim::{
    ControlError, SpeakerControls, TrimMode, controls_from_calibration, trim_config_index,
};
