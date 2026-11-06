//! GStreamer plugin registering `salsaoar` and `salsahrtfrender`.

mod adapt;
mod hrtf;
mod oar;

use gstreamer as gst;
use gstreamer_base as gst_base;

salsa_gst::plugin_elements! {
    Oar(oar::Oar) extends gst_base::BaseTransform = "salsaoar" as OAR;
    HrtfRender(hrtf::HrtfRender)
        extends gst_base::BaseTransform = "salsahrtfrender" as HRTF_RENDER;
    ObjectAudioAdapt(adapt::ObjectAudioAdapt)
        extends gst_base::BaseTransform = "salsaobjectaudioadapt" as OBJECT_AUDIO_ADAPT;
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
    use gst::glib;
    use gst::glib::prelude::*;

    #[test]
    fn builds_the_renderer() {
        let _ = gst::init();
        let oar: Oar = glib::Object::new();
        assert!(oar.is::<gst_base::BaseTransform>());
        let hrtf: HrtfRender = glib::Object::new();
        assert!(hrtf.is::<gst_base::BaseTransform>());
    }
}
