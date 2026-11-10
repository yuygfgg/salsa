//! E-AC-3 JOC decode stage: libavcodec core PCM + the JOC kernel -> `SpatialFrame`.

use eac3joc::{
    BedPlan, Eac3JocFrame, FRAME_SAMPLES, JocParams, MAX_OBJECTS, compute_bed_plan_for_config,
    num_channels_for_config,
};
use objectaudio_ir::{IrError, ObjectState, PcmBuffer, SpatialDecoder, SpatialFrame};
use objectaudio_oamd::BedPolicy;
use thiserror::Error;

use crate::kernel::{JocKernel, KernelError, OUTPUT_CHANNELS};
use eac3joc_core_decoder::{CoreDecoder, CoreDecoderError, CorePcm, select_lanes};

#[derive(Debug, Error)]
pub enum DspError {
    #[error("core decode failed: {0}")]
    Core(#[from] CoreDecoderError),

    #[error("JOC kernel failed: {0}")]
    Kernel(#[from] KernelError),

    #[error("IR rejected the decoded frame: {0}")]
    Ir(#[from] IrError),

    #[error("the core decoder produced {actual} samples, the frame declares {expected}")]
    SampleCount { expected: usize, actual: usize },

    #[error("the core decoder produced {actual} channels, the bed plan declares {declared}")]
    ChannelCount { declared: usize, actual: usize },

    #[error("no bed plan is available for this frame")]
    MissingBedPlan,

    #[error("the bed plan has no LFE channel")]
    MissingLfe,
}

/// The E-AC-3 JOC decoder.
pub struct Eac3JocDecoder {
    core: CoreDecoder,
    kernel: Box<JocKernel>,
    last_bed_plan: Option<BedPlan>,
    last_config: u8,
    last_sample_rate: u32,
    last_object_count: usize,
}

impl Eac3JocDecoder {
    /// Build a decoder. The libavcodec E-AC-3 decoder must be available.
    pub fn try_new() -> Result<Self, DspError> {
        Ok(Self {
            core: CoreDecoder::new()?,
            kernel: Box::new(JocKernel::new()),
            last_bed_plan: None,
            last_config: 3,
            last_sample_rate: 48_000,
            last_object_count: MAX_OBJECTS,
        })
    }

    /// Drop the transport/decode state after a flush or seek.
    ///
    /// The stream configuration (`last_bed_plan`, `last_config`,
    /// `last_object_count`) is deliberately kept: the first frame after a
    /// seek may not repeat the JOC payload, and losing the configuration
    /// would change the negotiated channel count or bed mapping mid-stream.
    pub fn reset(&mut self) {
        self.core.reset();
        self.kernel.reset();
    }

    fn decode_core(&mut self, frame: &Eac3JocFrame) -> Result<CorePcm, DspError> {
        // libavcodec only assembles dependent substreams when they arrive in
        // the same packet as their independent substream.  Sending the
        // syncframes separately makes it discard every dependent frame.
        let bytes = frame.core_bytes();
        let pcm = self.core.decode(&bytes)?;
        if pcm.channels == 0 {
            return Err(DspError::SampleCount {
                expected: frame.samples,
                actual: 0,
            });
        }
        if pcm.samples < frame.samples {
            return Err(DspError::SampleCount {
                expected: frame.samples,
                actual: pcm.samples,
            });
        }
        Ok(pcm)
    }

    fn resolve_bed_plan(
        &mut self,
        frame: &Eac3JocFrame,
        params: &JocParams,
    ) -> Result<BedPlan, DspError> {
        if let Some(plan) = frame.bed_plan {
            self.last_bed_plan = Some(plan);
            return Ok(plan);
        }
        if let Some(plan) = self.last_bed_plan {
            return Ok(plan);
        }
        let plan = compute_bed_plan_for_config(params.dmx_config_idx)
            .map_err(|_| DspError::MissingBedPlan)?;
        self.last_bed_plan = Some(plan);
        Ok(plan)
    }
}

impl SpatialDecoder for Eac3JocDecoder {
    type Frame = Eac3JocFrame;
    type Error = DspError;

    fn decode(&mut self, frame: &Self::Frame) -> Result<SpatialFrame, Self::Error> {
        let silent;
        let params: &JocParams = if let Some(joc) = &frame.joc {
            self.last_config = joc.dmx_config_idx;
            joc
        } else {
            let mut params = JocParams::silent();
            params.dmx_config_idx = self.last_config;
            params.n_channels = frame
                .bed_plan
                .or(self.last_bed_plan)
                .map(|plan| plan.core_channels() as u8)
                .unwrap_or_else(|| {
                    num_channels_for_config(u32::from(self.last_config)).unwrap_or(5)
                });
            silent = Box::new(params);
            silent.as_ref()
        };
        let core_channels = usize::from(params.n_channels);
        let object_count = if frame.joc.is_some() {
            let count = usize::from(params.n_objects).min(MAX_OBJECTS);
            self.last_object_count = count;
            count
        } else {
            self.last_object_count.min(MAX_OBJECTS)
        };
        let plan = self.resolve_bed_plan(frame, params)?;
        if frame.samples != FRAME_SAMPLES {
            return Err(DspError::SampleCount {
                expected: FRAME_SAMPLES,
                actual: frame.samples,
            });
        }
        let samples = FRAME_SAMPLES;
        let pcm = self.decode_core(frame)?;
        if pcm.channels != plan.channels {
            return Err(DspError::ChannelCount {
                declared: plan.channels,
                actual: pcm.channels,
            });
        }
        let bed_planes = select_lanes(&pcm, &plan.lanes[..core_channels], samples);
        let mut bed_planar = Vec::with_capacity(core_channels * samples);
        for plane in &bed_planes {
            bed_planar.extend_from_slice(plane);
        }
        let lfe_index = plan.lfe_channel.ok_or(DspError::MissingLfe)?;
        let lfe = pcm
            .planes
            .get(lfe_index)
            .map(|plane| plane[..samples].to_vec());

        let mut output = vec![0.0f32; OUTPUT_CHANNELS * FRAME_SAMPLES];
        self.kernel
            .process(&bed_planar, lfe.as_deref(), params, 1.0, &mut output)?;

        let output_channels = object_count + 1;
        let mut planes = Vec::with_capacity(output_channels);
        for channel in 0..output_channels {
            planes.push(output[channel * FRAME_SAMPLES..(channel + 1) * FRAME_SAMPLES].to_vec());
        }
        let pcm = PcmBuffer::from_planar(planes)?;

        // The OAMD block offset is the only base offset for an E-AC-3 frame.
        // Object ids are rebased to 1..=N by the shared OAMD mapping.
        let mapping = frame
            .oamd
            .as_ref()
            .map(|oamd| oamd.map_to_ir(u32::from(frame.oamd_sample_offset), BedPolicy::Ignore));
        let calibration = mapping
            .as_ref()
            .and_then(|mapping| mapping.calibration.clone());
        let mut events = mapping
            .as_ref()
            .map(|mapping| mapping.events.clone())
            .unwrap_or_default();
        let snapshot = mapping
            .as_ref()
            .map(|mapping| (mapping.snapshot_offset, mapping.snapshot_ramp_samples));
        let mut objects = mapping.map(|mapping| mapping.objects).unwrap_or_default();
        objects.truncate(object_count);
        for id in objects.len() as u32 + 1..=object_count as u32 {
            objects.push(ObjectState::new(id, [0.0, 0.0, 0.0]));
        }
        events.retain(|event| event.state.id as usize <= object_count);

        let sample_rate = if frame.sample_rate != 0 {
            frame.sample_rate
        } else {
            self.last_sample_rate
        };
        if sample_rate != 0 {
            self.last_sample_rate = sample_rate;
        }

        let mut spatial = SpatialFrame::new(sample_rate, FRAME_SAMPLES, objects, events, pcm)?;
        if let Some(calibration) = calibration {
            spatial = spatial.with_calibration(calibration);
        }
        if let Some((offset, ramp)) = snapshot {
            spatial = spatial
                .with_snapshot_ramp(ramp)
                .with_snapshot_offset(i64::from(offset));
        } else {
            spatial.snapshot = objectaudio_ir::Snapshot::Continue;
        }
        Ok(spatial)
    }
}
