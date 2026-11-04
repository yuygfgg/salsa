//! GStreamer plugin registering `salsaeac3jocparse` and `salsaeac3jocdec`.

mod decoder;
mod parse;

use gst::glib;
use gst::glib::prelude::*;
use gstreamer as gst;
use gstreamer_audio as gst_audio;
use gstreamer_base as gst_base;
use salsa_codec::elements;

gst::glib::wrapper! {
    pub struct Eac3JocParse(ObjectSubclass<parse::Eac3JocParse>)
        @extends gst_base::BaseParse, gst::Element, gst::Object;
}

gst::glib::wrapper! {
    pub struct Eac3JocDec(ObjectSubclass<decoder::Eac3JocDec>)
        @extends gst_audio::AudioDecoder, gst::Element, gst::Object;
}

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        elements::EAC3_PARSE,
        gst::Rank::PRIMARY,
        Eac3JocParse::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        elements::EAC3_DEC,
        gst::Rank::PRIMARY,
        Eac3JocDec::static_type(),
    )?;
    Ok(())
}

gst::plugin_define!(
    salsaeac3joc,
    "E-AC-3 JOC parse and decode elements",
    plugin_init,
    "0.1.0",
    "MPL",
    "gst-eac3joc",
    "gst-eac3joc",
    "https://example.invalid/salsa",
    "2026-10-31"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_both_elements() {
        let _ = gst::init();
        let parse: Eac3JocParse = glib::Object::new();
        assert!(parse.is::<gst_base::BaseParse>());
        let decode: Eac3JocDec = glib::Object::new();
        assert!(decode.is::<gst_audio::AudioDecoder>());
    }
}
