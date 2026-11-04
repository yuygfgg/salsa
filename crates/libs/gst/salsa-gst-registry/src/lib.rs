//! Register every Salsa GStreamer element in-process.
//!
//! Applications and tests used to open-code the same `Element::register`
//! sequence (and the GUI additionally loaded the plugin binaries from a
//! hand-maintained list). One registry keeps the element names and types in a
//! single place.

use gst::glib;
use gstreamer as gst;
use salsa_codec::elements;

/// Register all Salsa elements without a plugin.
pub fn register_all() {
    salsa_gst::register_element::<gstsalsaeac3joc::Eac3JocParse>(elements::EAC3_PARSE);
    salsa_gst::register_element::<gstsalsaeac3joc::Eac3JocDec>(elements::EAC3_DEC);
    salsa_gst::register_element::<gstsalsatruehd::TrueHdParse>(elements::TRUEHD_PARSE);
    salsa_gst::register_element::<gstsalsatruehd::TrueHdAtmosDec>(elements::TRUEHD_DEC);
    salsa_gst::register_element::<gstsalsaobjectaudio::Oar>(elements::OAR);
    salsa_gst::register_element::<gstsalsaobjectaudio::HrtfRender>(elements::HRTF_RENDER);
    salsa_gst::register_element::<gstsalsaobjectaudio::ObjectAudioAdapt>(
        elements::OBJECT_AUDIO_ADAPT,
    );
    salsa_gst::register_element::<gstsalsadamf::DamfSrc>(elements::DAMF_SRC);
    salsa_gst::register_element::<gstsalsadamf::DamfSink>(elements::DAMF_SINK);
}

/// Initialize GStreamer, the object-audio meta API and every Salsa element.
pub fn init() -> Result<(), glib::Error> {
    objectaudio_meta::init()?;
    register_all();
    Ok(())
}
