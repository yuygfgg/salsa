//! Rate-limited progress on stderr. Stdout remains a JSON report.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use salsa_spatial_encoder::{EncodeProgress, EncodeStage};

const SAMPLE_RATE: f64 = 48_000.0;

pub struct Progress {
    enabled: bool,
    terminal: bool,
    stage: Option<EncodeStage>,
    started: Instant,
    last_update: Instant,
    line_open: bool,
}

impl Progress {
    pub fn new(enabled: bool) -> Self {
        let now = Instant::now();
        Self {
            enabled,
            terminal: std::io::stderr().is_terminal() && log::max_level() < log::LevelFilter::Info,
            stage: None,
            started: now,
            last_update: now,
            line_open: false,
        }
    }

    pub fn update(&mut self, progress: EncodeProgress) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let changed = self.stage != Some(progress.stage);
        if changed {
            self.finish();
            self.stage = Some(progress.stage);
            self.started = now;
        }
        let complete = progress.completed_samples >= progress.total_samples;
        let interval = if self.terminal {
            Duration::from_millis(250)
        } else {
            Duration::from_secs(5)
        };
        if !changed && !complete && now.duration_since(self.last_update) < interval {
            return;
        }
        self.last_update = now;
        let line = format_progress(progress, now.duration_since(self.started));
        let mut stderr = std::io::stderr().lock();
        if self.terminal {
            let _ = write!(stderr, "\r\x1b[2K{line}");
            self.line_open = true;
            if complete {
                let _ = writeln!(stderr);
                self.line_open = false;
            }
        } else {
            let _ = writeln!(stderr, "{line}");
        }
        let _ = stderr.flush();
    }

    /// End an unfinished terminal line before an error or the final report.
    pub fn finish(&mut self) {
        if self.line_open {
            let _ = writeln!(std::io::stderr().lock());
            self.line_open = false;
        }
    }
}

fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.0).ceil() as u64;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn format_progress(progress: EncodeProgress, elapsed: Duration) -> String {
    let stage = match progress.stage {
        EncodeStage::Headroom => "Headroom",
        EncodeStage::Encoding => "Encoding",
    };
    let completed = progress.completed_samples.min(progress.total_samples);
    let fraction = if progress.total_samples == 0 {
        1.0
    } else {
        completed as f64 / progress.total_samples as f64
    };
    let seconds = elapsed.as_secs_f64();
    let speed = if seconds > 0.0 {
        completed as f64 / SAMPLE_RATE / seconds
    } else {
        0.0
    };
    let eta = if completed == progress.total_samples {
        clock(0.0)
    } else if completed > 0 && seconds > 0.0 {
        clock(seconds * (progress.total_samples - completed) as f64 / completed as f64)
    } else {
        "--:--:--".to_string()
    };
    format!(
        "{stage}: {:5.1}% | {:.1}/{:.1} s | {speed:.2}x | elapsed {} | ETA {eta}",
        fraction * 100.0,
        completed as f64 / SAMPLE_RATE,
        progress.total_samples as f64 / SAMPLE_RATE,
        clock(seconds),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_stage_speed_and_remaining_time() {
        let update = EncodeProgress {
            stage: EncodeStage::Encoding,
            completed_samples: 96_000,
            total_samples: 480_000,
        };
        assert_eq!(
            format_progress(update, Duration::from_secs(4)),
            "Encoding:  20.0% | 2.0/10.0 s | 0.50x | elapsed 00:00:04 | ETA 00:00:16"
        );
    }

    #[test]
    fn start_and_empty_interval_have_finite_progress() {
        let mut update = EncodeProgress {
            stage: EncodeStage::Headroom,
            completed_samples: 0,
            total_samples: 48_000,
        };
        assert_eq!(
            format_progress(update, Duration::ZERO),
            "Headroom:   0.0% | 0.0/1.0 s | 0.00x | elapsed 00:00:00 | ETA --:--:--"
        );
        update.total_samples = 0;
        assert_eq!(
            format_progress(update, Duration::ZERO),
            "Headroom: 100.0% | 0.0/0.0 s | 0.00x | elapsed 00:00:00 | ETA 00:00:00"
        );
    }
}
