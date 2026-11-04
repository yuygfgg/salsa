//! GStreamer plugin registering `salsatruehdparse` and `salsatruehdatmosdec`.

mod decoder;
mod parse;

use gst::glib;
use gst::glib::prelude::*;
use gstreamer as gst;
use gstreamer_audio as gst_audio;
use gstreamer_base as gst_base;
use salsa_codec::elements;

gst::glib::wrapper! {
    pub struct TrueHdParse(ObjectSubclass<parse::TrueHdParse>)
        @extends gst_base::BaseParse, gst::Element, gst::Object;
}

gst::glib::wrapper! {
    pub struct TrueHdAtmosDec(ObjectSubclass<decoder::TrueHdAtmosDec>)
        @extends gst_audio::AudioDecoder, gst::Element, gst::Object;
}

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        elements::TRUEHD_PARSE,
        gst::Rank::PRIMARY,
        TrueHdParse::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        elements::TRUEHD_DEC,
        gst::Rank::PRIMARY,
        TrueHdAtmosDec::static_type(),
    )?;
    Ok(())
}

gst::plugin_define!(
    salsatruehd,
    "TrueHD Atmos parse and decode elements",
    plugin_init,
    "0.1.0",
    "MPL",
    "gst-truehd",
    "gst-truehd",
    "https://example.invalid/salsa",
    "2026-11-01"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_element() {
        let _ = gst::init();
        let parse: TrueHdParse = glib::Object::new();
        assert!(parse.is::<gst_base::BaseParse>());
        let decode: TrueHdAtmosDec = glib::Object::new();
        assert!(decode.is::<gst_audio::AudioDecoder>());
    }
}
