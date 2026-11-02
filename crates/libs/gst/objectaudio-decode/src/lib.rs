#![forbid(unsafe_code)]
#![doc = "GStreamer caps/negotiation/PCM-push glue shared by the salsa decoders."]

use gstreamer as gst;
use gstreamer_audio as gst_audio;

use gst_audio::prelude::*;
use objectaudio_ir::{SpatialFrame, SpatialPayload};

/// Build the F32LE interleaved src caps a decoder offers.
pub fn object_audio_src_caps(rate: i32, channels: usize, with_feature: bool) -> gst::Caps {
    let mut caps = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("layout", "interleaved")
        .field("channels", channels as i32)
        .field("rate", rate)
        .field("channel-mask", gst::Bitmask(0))
        .build();
    if with_feature {
        objectaudio_meta::ensure_feature(caps.get_mut().unwrap());
    }
    caps
}

/// Output sample rates supported by the E-AC-3 JOC decoder.
pub const EAC3_RATES: &str = "{32000,44100,48000}";

/// Output sample rates supported by the TrueHD Atmos decoder.
pub const TRUEHD_RATES: &str = "{44100,48000,88200,96000,176400,192000}";

/// Src pad-template caps with both the featured and the plain variant.
pub fn object_audio_template_caps(max_channels: usize, rates: &str) -> gst::Caps {
    format!(
        "audio/x-raw(memory:SystemMemory, {}), format=(string)F32LE, \
         layout=(string)interleaved, channels=(int)[1,{max_channels}], \
         rate=(int){rates}, channel-mask=(bitmask)0; \
         audio/x-raw(memory:SystemMemory), format=(string)F32LE, \
         layout=(string)interleaved, channels=(int)[1,{max_channels}], \
         rate=(int){rates}, channel-mask=(bitmask)0",
        objectaudio_meta::CAPS_FEATURE
    )
    .parse()
    .expect("static decoder src caps")
}

/// Whether the downstream peer accepts (and therefore needs) `ObjectAudioMeta`.
pub fn negotiate_attach_meta<D: IsA<gst_audio::AudioDecoder>>(decoder: &D) -> bool {
    let decoder = decoder.upcast_ref::<gst_audio::AudioDecoder>();
    let peer = decoder.src_pad().peer_query_caps(None);
    objectaudio_meta::has_feature(peer.as_ref())
}

/// Pack already-interleaved `f32` samples, attach an optional payload, and
/// finish the frame. `channel_count` must match the interleaved stride.
pub fn push_pcm<D: IsA<gst_audio::AudioDecoder>>(
    decoder: &D,
    interleaved: &[f32],
    channel_count: usize,
    meta: Option<&SpatialPayload>,
    frames: i32,
) -> Result<gst::FlowSuccess, gst::FlowError> {
    let mut bytes = Vec::with_capacity(interleaved.len() * 4);
    for sample in interleaved {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    let mut buffer = gst::Buffer::from_slice(bytes);
    if let Some(payload) = meta {
        let bpf = 4 * channel_count;
        let buffer_ref = buffer.get_mut().expect("fresh buffer is writable");
        objectaudio_meta::add_spatial(buffer_ref, payload, bpf).map_err(|error| {
            gst::error!(gst::CAT_RUST, "cannot attach ObjectAudioMeta: {error}");
            gst::FlowError::Error
        })?;
    }
    let decoder = decoder.upcast_ref::<gst_audio::AudioDecoder>();
    decoder
        .finish_frame(Some(buffer), frames)
        .inspect_err(|error| {
            // A flush can race with an in-flight frame; the caller drops the
            // frame and lets FLUSH_STOP reset the decoder. Do not report it
            // as a decode error.
            if error == &gst::FlowError::Flushing {
                gst::debug!(gst::CAT_RUST, "finish_frame stopped by a flush");
            } else {
                gst::error!(gst::CAT_RUST, "finish_frame failed: {error:?}");
            }
        })
}

/// Interleave `spatial`, attach an optional spatial payload, and finish the frame.
pub fn push_spatial<D: IsA<gst_audio::AudioDecoder>>(
    decoder: &D,
    spatial: &SpatialFrame,
    meta: Option<&SpatialPayload>,
    frames: i32,
) -> Result<gst::FlowSuccess, gst::FlowError> {
    let interleaved = spatial.pcm.interleave();
    push_pcm(
        decoder,
        &interleaved,
        spatial.pcm.channel_count(),
        meta,
        frames,
    )
}
