//! GStreamer plugin registering `salsadamfsrc` and `salsadamfsink`.

mod sink;
mod source;

use gst::glib;
use gst::glib::prelude::*;
use gstreamer as gst;
use gstreamer_base as gst_base;
use salsa_codec::elements;

gst::glib::wrapper! {
    pub struct DamfSrc(ObjectSubclass<source::DamfSrc>)
        @extends gst_base::BaseSrc, gst::Element, gst::Object;
}

gst::glib::wrapper! {
    pub struct DamfSink(ObjectSubclass<sink::DamfSink>)
        @extends gst_base::BaseSink, gst::Element, gst::Object;
}

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        elements::DAMF_SRC,
        gst::Rank::PRIMARY,
        DamfSrc::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        elements::DAMF_SINK,
        gst::Rank::PRIMARY,
        DamfSink::static_type(),
    )?;
    Ok(())
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

    #[test]
    fn builds_the_elements() {
        let _ = gst::init();
        let source: DamfSrc = glib::Object::new();
        assert!(source.is::<gst_base::BaseSrc>());
        let sink: DamfSink = glib::Object::new();
        assert!(sink.is::<gst_base::BaseSink>());
    }
}
