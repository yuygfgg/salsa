//! GStreamer plugin registering `salsaeac3jocparse` and `salsaeac3jocdec`.

mod decoder;
mod parse;

use gstreamer as gst;
use gstreamer_audio as gst_audio;
use gstreamer_base as gst_base;

salsa_gst::plugin_elements! {
    caps = "audio/x-eac3";

    Eac3JocParse(parse::Eac3JocParse)
        extends gst_base::BaseParse = "salsaeac3jocparse" as EAC3_PARSE;
    Eac3JocDec(decoder::Eac3JocDec)
        extends gst_audio::AudioDecoder = "salsaeac3jocdec" as EAC3_DEC;
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
    use gst::glib;
    use gst::glib::prelude::*;

    #[test]
    fn builds_both_elements() {
        let _ = gst::init();
        let parse: Eac3JocParse = glib::Object::new();
        assert!(parse.is::<gst_base::BaseParse>());
        let decode: Eac3JocDec = glib::Object::new();
        assert!(decode.is::<gst_audio::AudioDecoder>());
    }
}
