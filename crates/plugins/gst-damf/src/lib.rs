//! GStreamer plugin registering `salsadamfsrc`.

mod source;

use gst::glib;
use gst::glib::prelude::*;
use gstreamer as gst;
use gstreamer_base as gst_base;

gst::glib::wrapper! {
    pub struct DamfSrc(ObjectSubclass<source::DamfSrc>)
        @extends gst_base::BaseSrc, gst::Element, gst::Object;
}

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "salsadamfsrc",
        gst::Rank::PRIMARY,
        DamfSrc::static_type(),
    )?;
    Ok(())
}

gst::plugin_define!(
    salsadamf,
    "DAMF source",
    plugin_init,
    "0.1.0",
    "MPL",
    "gst-damf",
    "gst-damf",
    "https://example.invalid/salsa",
    "2026-11-02"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_element() {
        let _ = gst::init();
        let source: DamfSrc = glib::Object::new();
        assert!(source.is::<gst_base::BaseSrc>());
    }
}
