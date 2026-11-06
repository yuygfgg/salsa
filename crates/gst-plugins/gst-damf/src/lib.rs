//! GStreamer plugin registering `salsadamfsrc` and `salsadamfsink`.

mod sink;
mod source;

use gstreamer as gst;
use gstreamer_base as gst_base;

salsa_gst::plugin_elements! {
    DamfSrc(source::DamfSrc) extends gst_base::BaseSrc = "salsadamfsrc" as DAMF_SRC;
    DamfSink(sink::DamfSink) extends gst_base::BaseSink = "salsadamfsink" as DAMF_SINK;
}

gst::plugin_define!(
    salsadamf,
    "DAMF source and sink",
    plugin_init,
    "0.1.0",
    "MPL",
    "gst-damf",
    "gst-damf",
    "https://example.invalid/salsa",
    "2026-11-03"
);

#[cfg(test)]
mod tests {
    use super::*;
    use gst::glib;
    use gst::glib::prelude::*;

    #[test]
    fn builds_the_elements() {
        let _ = gst::init();
        let source: DamfSrc = glib::Object::new();
        assert!(source.is::<gst_base::BaseSrc>());
        let sink: DamfSink = glib::Object::new();
        assert!(sink.is::<gst_base::BaseSink>());
    }
}
