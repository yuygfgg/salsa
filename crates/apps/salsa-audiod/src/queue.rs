//! A single-producer appsrc queue with a byte limit and cancellable waits.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::AppSrc;

pub(crate) struct BoundedAppSrc {
    source: AppSrc,
    pipeline: gst::Pipeline,
    stop: Arc<AtomicBool>,
    max_bytes: u64,
}

impl BoundedAppSrc {
    pub(crate) fn new(
        source: AppSrc,
        pipeline: &gst::Pipeline,
        stop: Arc<AtomicBool>,
        max_bytes: u64,
    ) -> Self {
        assert!(max_bytes > 0);
        source.set_max_bytes(max_bytes);
        // GStreamer's blocking push cannot observe our cancellation flag.
        // Reserve space here instead; &mut self enforces one producer.
        source.set_block(false);
        Self {
            source,
            pipeline: pipeline.clone(),
            stop,
            max_bytes,
        }
    }

    pub(crate) fn source(&self) -> &AppSrc {
        &self.source
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.stop.load(Ordering::Relaxed) {
            bail!("audio operation cancelled");
        }
        if let Some(message) = self.pipeline.bus().and_then(|bus| {
            bus.timed_pop_filtered(gst::ClockTime::ZERO, &[gst::MessageType::Error])
        }) && let gst::MessageView::Error(error) = message.view()
        {
            bail!(
                "audio pipeline error: {} ({})",
                error.error(),
                error.debug().unwrap_or_default()
            );
        }
        Ok(())
    }

    pub(crate) fn push(&mut self, buffer: gst::Buffer) -> Result<()> {
        let bytes = buffer.size() as u64;
        if bytes > self.max_bytes {
            bail!(
                "audio buffer ({bytes} bytes) exceeds queue limit ({} bytes)",
                self.max_bytes
            );
        }
        loop {
            self.check()?;
            if self.source.current_level_bytes() <= self.max_bytes - bytes {
                return self
                    .source
                    .push_buffer(buffer)
                    .map(|_| ())
                    .context("appsrc rejected audio");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub(crate) fn end_of_stream(&self) -> Result<()> {
        self.check()?;
        self.source
            .end_of_stream()
            .map(|_| ())
            .context("appsrc rejected EOS")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn full_queue_waits_without_growing_and_cancels() {
        gst::init().unwrap();
        let source = AppSrc::builder().build();
        let pipeline = gst::Pipeline::new();
        pipeline.add(&source).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let mut queue = BoundedAppSrc::new(source.clone(), &pipeline, stop.clone(), 4096);
        queue.push(gst::Buffer::with_size(4096).unwrap()).unwrap();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            tx.send(queue.push(gst::Buffer::with_size(4096).unwrap()))
                .unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        assert_eq!(source.current_level_bytes(), 4096);
        stop.store(true, Ordering::Relaxed);
        let error = rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        worker.join().unwrap();
    }

    #[test]
    fn oversized_buffer_is_rejected_without_enqueueing() {
        gst::init().unwrap();
        let source = AppSrc::builder().build();
        let mut queue =
            BoundedAppSrc::new(source.clone(), &gst::Pipeline::new(), Arc::default(), 4096);
        assert!(queue.push(gst::Buffer::with_size(8192).unwrap()).is_err());
        assert_eq!(source.current_level_bytes(), 0);
    }
}
