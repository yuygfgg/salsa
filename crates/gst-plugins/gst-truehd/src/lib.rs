//! GStreamer plugin registering `salsatruehdparse` and `salsatruehdatmosdec`.

mod decoder;
mod parse;

use gstreamer as gst;
use gstreamer_audio as gst_audio;
use gstreamer_base as gst_base;

salsa_gst::plugin_elements! {
    caps = "audio/x-true-hd";

    TrueHdParse(parse::TrueHdParse)
        extends gst_base::BaseParse = "salsatruehdparse" as TRUEHD_PARSE;
    TrueHdAtmosDec(decoder::TrueHdAtmosDec)
        extends gst_audio::AudioDecoder = "salsatruehdatmosdec" as TRUEHD_DEC;
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
    use gst::glib;
    use gst::glib::prelude::*;

    #[test]
    fn builds_the_element() {
        let _ = gst::init();
        let parse: TrueHdParse = glib::Object::new();
        assert!(parse.is::<gst_base::BaseParse>());
        let decode: TrueHdAtmosDec = glib::Object::new();
        assert!(decode.is::<gst_audio::AudioDecoder>());
    }
}
