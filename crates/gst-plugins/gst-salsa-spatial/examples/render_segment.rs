//! Render a pipeline to a float WAV, with an optional flushing seek.
//!
//! Usage: render_segment <pipeline-without-sink> <output.wav> [start_s stop_s]
//!        [--pcm-override objects.f32] [--warp-mode 0..3]
//! The output caps are fixed at 48 kHz, interleaved F32LE. The renderer selects
//! the channel count and channel order.
//! A PCM override replaces samples at `identity name=object_pcm`. It preserves
//! buffer timing and metadata. The file must contain exactly the same number
//! of interleaved float samples as that identity receives.
//! A warp override uses the same identity to select a common render policy.

use std::error::Error;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, Once};

use gst::prelude::*;
use gstreamer as gst;

struct Pipeline(gst::Pipeline);

impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

fn clock(value: &str) -> Result<gst::ClockTime, Box<dyn Error>> {
    let seconds: f64 = value.parse()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err("time must be finite and nonnegative".into());
    }
    Ok(gst::ClockTime::from_nseconds((seconds * 1e9).round() as u64))
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut input = std::env::args().skip(1);
    let mut args = Vec::new();
    let mut pcm_override = None;
    let mut warp_mode = None;
    while let Some(argument) = input.next() {
        match argument.as_str() {
            "--pcm-override" => pcm_override = Some(input.next().ok_or("missing PCM path")?),
            "--warp-mode" => {
                let mode: u8 = input.next().ok_or("missing warp mode")?.parse()?;
                if mode > 3 {
                    return Err("warp mode must be 0..3".into());
                }
                warp_mode = Some(mode);
            }
            _ => args.push(argument),
        }
    }
    if args.len() != 2 && args.len() != 4 {
        return Err("usage: render_segment <pipeline> <output.wav> [start_s stop_s]".into());
    }
    if pcm_override.is_some() && args.len() == 4 {
        return Err("PCM override requires an unseeked pipeline".into());
    }
    salsa_gst_registry::init()?;
    let pipeline = gst::parse::launch(&format!(
        "{} ! audio/x-raw,format=F32LE,rate=48000,layout=interleaved \
         ! fakesink name=output sync=false signal-handoffs=true",
        args[0]
    ))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "expected a pipeline")?;
    let guard = Pipeline(pipeline);
    let pipeline = &guard.0;
    let replacement = if let Some(path) = pcm_override {
        let file = std::fs::File::open(path)?;
        let expected = file.metadata()?.len();
        let state = Arc::new(Mutex::new((file, 0u64)));
        Some((state, expected))
    } else {
        None
    };
    if replacement.is_some() || warp_mode.is_some() {
        let received = replacement.as_ref().map(|(state, _)| Arc::clone(state));
        let element = pipeline
            .by_name("object_pcm")
            .ok_or("render overrides require identity name=object_pcm")?;
        let weak = element.downgrade();
        let logged_calibration = Once::new();
        element
            .static_pad("src")
            .ok_or("object_pcm has no source pad")?
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_mut() else {
                    return gst::PadProbeReturn::Ok;
                };
                let result = (|| -> Result<(), Box<dyn Error>> {
                    let buffer = buffer.make_mut();
                    if let Some(received) = &received {
                        let mut map = buffer.map_writable()?;
                        let mut state = received.lock().unwrap();
                        state.0.read_exact(map.as_mut_slice())?;
                        state.1 += map.len() as u64;
                    }
                    let mut snapshots = objectaudio_meta::iter_spatial(buffer);
                    if let Some(snapshot) = snapshots.first() {
                        logged_calibration.call_once(|| {
                            println!("Input calibration: {:?}", snapshot.payload.calibration);
                        });
                    }
                    if let Some(mode) = warp_mode {
                        objectaudio_meta::remove_all(buffer);
                        for snapshot in &mut snapshots {
                            snapshot
                                .payload
                                .calibration
                                .get_or_insert_default()
                                .warp_mode = mode;
                            for event in &mut snapshot.payload.calibration_events {
                                event.calibration.warp_mode = mode;
                            }
                            objectaudio_meta::add_spatial(buffer, &snapshot.payload, snapshot.bpf)?;
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    if let Some(element) = weak.upgrade() {
                        element.post_error_message(gst::error_msg!(
                            gst::StreamError::Decode,
                            ["Cannot apply render override: {error}"]
                        ));
                    }
                    return gst::PadProbeReturn::Drop;
                }
                gst::PadProbeReturn::Ok
            });
    }
    let samples = Arc::new(Mutex::new(Vec::<u8>::new()));
    let received = Arc::clone(&samples);
    pipeline
        .by_name("output")
        .unwrap()
        .connect("handoff", false, move |values| {
            let buffer = values[1].get::<gst::Buffer>().expect("handoff buffer");
            let map = buffer.map_readable().expect("readable audio");
            received.lock().unwrap().extend_from_slice(map.as_slice());
            None
        });
    pipeline.set_state(gst::State::Paused)?;
    let (result, _, _) = pipeline.state(gst::ClockTime::from_seconds(30));
    result?;
    if args.len() == 4 {
        let start = clock(&args[2])?;
        let stop = clock(&args[3])?;
        if stop <= start {
            return Err("stop must follow start".into());
        }
        pipeline.seek(
            1.0,
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set,
            start,
            gst::SeekType::Set,
            stop,
        )?;
    }
    pipeline.set_state(gst::State::Playing)?;
    let bus = pipeline.bus().unwrap();
    loop {
        let message = bus
            .timed_pop(gst::ClockTime::from_seconds(30))
            .ok_or("pipeline produced no message for 30 seconds")?;
        match message.view() {
            gst::MessageView::Eos(_) => break,
            gst::MessageView::Error(error) => {
                return Err(format!("{}: {:?}", error.error(), error.debug()).into());
            }
            _ => {}
        }
    }
    let caps = pipeline
        .by_name("output")
        .unwrap()
        .static_pad("sink")
        .unwrap()
        .current_caps()
        .ok_or("output has no negotiated caps")?;
    let channels = u16::try_from(
        caps.structure(0)
            .ok_or("empty output caps")?
            .get::<i32>("channels")?,
    )?;
    let block_align = channels.checked_mul(4).ok_or("too many output channels")?;
    if block_align == 0 {
        return Err("output has no channels".into());
    }
    pipeline.set_state(gst::State::Null)?;
    if let Some((state, expected)) = replacement {
        let consumed = state.lock().unwrap().1;
        if consumed != expected {
            return Err(format!("PCM override consumed {consumed} of {expected} bytes").into());
        }
    }
    let data = samples.lock().unwrap();
    let size = u32::try_from(data.len())?;
    if size == 0 || size % u32::from(block_align) != 0 {
        return Err("output is empty or has an incomplete sample frame".into());
    }
    let mut file = std::fs::File::create(&args[1])?;
    file.write_all(b"RIFF")?;
    file.write_all(
        &size
            .checked_add(36)
            .ok_or("WAV exceeds RIFF size limit")?
            .to_le_bytes(),
    )?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&3u16.to_le_bytes())?;
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&48000u32.to_le_bytes())?;
    file.write_all(&(48000 * u32::from(block_align)).to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&32u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&size.to_le_bytes())?;
    file.write_all(&data)?;
    println!("{} frames; {caps}", size / u32::from(block_align));
    Ok(())
}
