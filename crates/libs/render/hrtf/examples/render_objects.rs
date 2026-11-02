//! Offline Rosella render from an interleaved objects16 dump.
//!
//! The OAMD timeline is taken from the E-AC-3 stream so the renderer sees the
//! same updates as the source stream:
//!
//! ```sh
//! cargo run -p objectaudio-hrtf --example render_objects -- \
//!   --ec3 stream.ec3 --objects stream.objects16.f32le --model model.personalized_headphone \
//!   --filterbank 2 --frames 250 --out /tmp/ours.f32le
//! ```

#![allow(clippy::needless_range_loop)]

use std::env;
use std::fs;

use eac3joc::Eac3JocParser;
use objectaudio_hrtf::{
    FRAME_SAMPLES, FilterbankKind, RosellaProfile, RosellaRenderOptions, RosellaRenderer,
    load_personalized_headphone,
};

fn main() {
    let mut ec3 = String::new();
    let mut objects = String::new();
    let mut model_path = String::new();
    let mut out = String::new();
    let mut filterbank = FilterbankKind::Public;
    let mut profile = RosellaProfile::Mid;
    let mut frames = usize::MAX;
    let mut source_channels = 16usize;
    let mut tail_seconds = 0.0f64;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().expect("missing value");
        match arg.as_str() {
            "--ec3" => ec3 = value(),
            "--objects" => objects = value(),
            "--model" => model_path = value(),
            "--out" => out = value(),
            "--filterbank" => filterbank = FilterbankKind::parse(&value()).expect("filterbank"),
            "--profile" => profile = RosellaProfile::parse(&value()).expect("profile"),
            "--frames" => frames = value().parse().expect("frames"),
            "--channels" => source_channels = value().parse().expect("channels"),
            "--tail" => tail_seconds = value().parse().expect("tail"),
            other => panic!("unknown argument {other}"),
        }
    }

    let model = load_personalized_headphone(&model_path).expect("model");
    eprintln!("{}", model.summary());
    let options = RosellaRenderOptions {
        filterbank,
        profile,
        tail_seconds,
        chunk_frames: 1,
        skip_latency: true,
        ..RosellaRenderOptions::default()
    };
    let mut renderer = RosellaRenderer::new(model, options, source_channels).expect("renderer");

    let raw = fs::read(&objects).expect("objects16");
    let frame_bytes = FRAME_SAMPLES * source_channels * 4;
    if !raw.len().is_multiple_of(frame_bytes) {
        panic!("objects16 length is not a whole number of frames");
    }
    let object_frames = raw.len() / frame_bytes;

    let stream = fs::read(&ec3).expect("ec3");
    let mut parser = Eac3JocParser::new();
    let mut parsed = parser.push_bytes(&stream).expect("parse");
    parsed.extend(parser.flush().expect("flush"));

    let count = object_frames.min(parsed.len()).min(frames);
    let mut stereo = Vec::new();
    for index in 0..count {
        let frame = &parsed[index];
        let spatial = frame.oamd.as_ref().map(|oamd| {
            let mut payload =
                oamd.to_spatial_payload(frame.sample_rate, objectaudio_oamd::BedPolicy::Expand);
            payload.rebase(i64::from(frame.oamd_sample_offset));
            payload
        });
        let base = index * frame_bytes;
        let mut planar = vec![0.0f32; FRAME_SAMPLES * source_channels];
        for sample in 0..FRAME_SAMPLES {
            for channel in 0..source_channels {
                let offset = base + (sample * source_channels + channel) * 4;
                planar[channel * FRAME_SAMPLES + sample] =
                    f32::from_le_bytes(raw[offset..offset + 4].try_into().unwrap());
            }
        }
        renderer
            .submit_frame(&planar, spatial.as_ref())
            .expect("submit");
        stereo.extend(renderer.take_output());
    }
    if tail_seconds > 0.0 {
        let flush = renderer.finish_capacity(tail_seconds);
        stereo.extend(renderer.finish(flush).expect("finish"));
    }
    let mut bytes = Vec::with_capacity(stereo.len() * 4);
    for value in stereo {
        bytes.extend_from_slice(&(value as f32).to_le_bytes());
    }
    fs::write(&out, bytes).expect("write");
    eprintln!("rendered {count} frames -> {out}");
}
