//! GStreamer plugin registering `salsaoar` and `salsahrtfrender`.

mod adapt;
mod hrtf;
mod oar;

use gst::glib;
use gst::glib::prelude::*;
use gstreamer as gst;
use gstreamer_base as gst_base;

gst::glib::wrapper! {
    pub struct Oar(ObjectSubclass<oar::Oar>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

gst::glib::wrapper! {
    pub struct HrtfRender(ObjectSubclass<hrtf::HrtfRender>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

gst::glib::wrapper! {
    pub struct ObjectAudioAdapt(ObjectSubclass<adapt::ObjectAudioAdapt>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "salsaoar",
        gst::Rank::PRIMARY,
        Oar::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "salsahrtfrender",
        gst::Rank::PRIMARY,
        HrtfRender::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "salsaobjectaudioadapt",
        gst::Rank::PRIMARY,
        ObjectAudioAdapt::static_type(),
    )?;
    Ok(())
}

gst::plugin_define!(
    salsaobjectaudio,
    "salsa object audio renderer",
    plugin_init,
    "0.1.0",
    "MPL",
    "gst-objectaudio",
    "gst-objectaudio",
    "https://example.invalid/salsa",
    "2026-10-31"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_renderer() {
        let _ = gst::init();
        let oar: Oar = glib::Object::new();
        assert!(oar.is::<gst_base::BaseTransform>());
        let hrtf: HrtfRender = glib::Object::new();
        assert!(hrtf.is::<gst_base::BaseTransform>());
    }
}
