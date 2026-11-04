// SPDX-License-Identifier: MPL-2.0
//! In-process FFmpeg LFE low-pass / alignment filter.

use std::sync::{Once, OnceLock};

use crate::bail;
use crate::error::{Context, Result};
use ffmpeg::ChannelLayout;
use ffmpeg::filter;
use ffmpeg::format;
use ffmpeg::frame;
use ffmpeg_next as ffmpeg;

/// The reference LFE graph, without its `[0:a]` source label.
const LFE_GRAPH_BODY: &str = "\
channelsplit=channel_layout=5.1[fl][fr][fc][lfe][bl][br];\
[lfe]asplit=2[lfe_dry][lfe_in];\
[lfe_dry]volume=0[silence];\
[lfe_in]firequalizer=\
gain_entry='entry(0,-0.42);entry(80,-0.42);entry(100,-0.42);entry(110,-0.47);\
entry(120,0.78);entry(125,-3.12);entry(130,-13.12);entry(135,-17.12);\
entry(140,-23.12);entry(150,-37.12);entry(160,-44.12);entry(180,-64.12);\
entry(200,-79.12);entry(24000,-89.12)':delay=0.10:accuracy=1:min_phase=1,\
atrim=start_sample=306,asetpts=N/SR/TB[filtered];\
[silence][filtered]amix=inputs=2:duration=first:normalize=0[lfe_final];\
[fl][fr][fc][lfe_final][bl][br]join=inputs=6:channel_layout=5.1:\
map=0.0-FL|1.0-FR|2.0-FC|3.0-LFE|4.0-BL|5.0-BR[out]";

const SAMPLE_FORMAT: format::Sample = format::Sample::F32(format::sample::Type::Packed);

/// Streaming LFE filter over packed f32 5.1 frames.
pub(crate) struct LfeFilter {
    graph: filter::Graph,
    pts: i64,
}

impl LfeFilter {
    pub(crate) fn new() -> Result<Self> {
        static INIT: Once = Once::new();
        static INIT_ERROR: OnceLock<String> = OnceLock::new();
        INIT.call_once(|| {
            if let Err(error) = ffmpeg::init() {
                let _ = INIT_ERROR.set(error.to_string());
            }
        });
        if let Some(error) = INIT_ERROR.get() {
            bail!("initialising FFmpeg failed: {error}");
        }

        let spec = format!(
            "abuffer@in=time_base=1/48000:sample_rate=48000:sample_fmt=flt:channel_layout=5.1,\
             {LFE_GRAPH_BODY};\
             [out]aformat=sample_fmts=flt:channel_layouts=5.1[fmt];\
             [fmt]abuffersink@out"
        );
        let mut graph = filter::Graph::new();
        graph
            .parse(&spec)
            .context("parsing the FFmpeg LFE filter graph")?;
        graph
            .validate()
            .context("configuring the FFmpeg LFE filter graph")?;
        Ok(Self { graph, pts: 0 })
    }

    /// Feed one interleaved 5.1 f32 block and return any filtered samples now available.
    pub(crate) fn push(&mut self, block: &[f32]) -> Result<Vec<f32>> {
        debug_assert!(block.len().is_multiple_of(6));
        let samples = block.len() / 6;
        if samples == 0 {
            return Ok(Vec::new());
        }
        let mut input = frame::Audio::new(SAMPLE_FORMAT, samples, ChannelLayout::_5POINT1_BACK);
        input.set_rate(48_000);
        input.set_pts(Some(self.pts));
        self.pts += samples as i64;
        input.plane_mut::<f32>(0)[..block.len()].copy_from_slice(block);
        self.graph
            .get("abuffer@in")
            .context("FFmpeg LFE graph source missing")?
            .source()
            .add(&input)
            .context("feeding the FFmpeg LFE graph")?;
        self.drain()
    }

    /// Flush the filter and return the remaining samples.
    pub(crate) fn flush(&mut self) -> Result<Vec<f32>> {
        self.graph
            .get("abuffer@in")
            .context("FFmpeg LFE graph source missing")?
            .source()
            .flush()
            .context("flushing the FFmpeg LFE graph")?;
        self.drain()
    }

    fn drain(&mut self) -> Result<Vec<f32>> {
        let mut output = Vec::new();
        let mut frame = frame::Audio::empty();
        loop {
            let result = self
                .graph
                .get("abuffersink@out")
                .context("FFmpeg LFE graph sink missing")?
                .sink()
                .frame(&mut frame);
            if result.is_err() {
                break;
            }
            let samples = frame.samples();
            if samples == 0 {
                continue;
            }
            if frame.format() != SAMPLE_FORMAT {
                bail!(
                    "FFmpeg LFE filter produced {:?}, expected packed f32",
                    frame.format()
                );
            }
            let plane = frame.plane::<f32>(0);
            output.extend_from_slice(&plane[..samples * 6]);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_preserves_the_input_length() {
        let mut filter = LfeFilter::new().unwrap();
        let block = vec![0.0f32; 1536 * 6];
        let mut output = filter.push(&block).unwrap();
        output.extend(filter.flush().unwrap());
        assert_eq!(output.len(), block.len());
    }
}
