//! GStreamer plugin registering `salsaspatialparse` and `salsaspatialdec`.

mod backend;
mod decoder;
mod framing;
mod parse;

use gstreamer as gst;
use gstreamer_audio as gst_audio;
use gstreamer_base as gst_base;

salsa_gst::plugin_elements! {
    caps = "audio/x-salsa-spatial";

    SalsaSpatialParse(parse::SalsaSpatialParse)
        extends gst_base::BaseParse = "salsaspatialparse" as SALSA_SPATIAL_PARSE;
    SalsaSpatialDec(decoder::SalsaSpatialDec)
        extends gst_audio::AudioDecoder = "salsaspatialdec" as SALSA_SPATIAL_DEC;
}

gst::plugin_define!(
    salsaspatial,
    "salsa-spatial parse and decode elements",
    plugin_init,
    "0.1.0",
    "MPL",
    "gst-salsa-spatial",
    "gst-salsa-spatial",
    "https://example.invalid/salsa",
    "2026-11-07"
);

#[cfg(test)]
mod tests {
    use super::*;
    use gst::glib;
    use gst::glib::prelude::*;
    use gst::prelude::*;

    #[test]
    fn builds_both_elements() {
        let _ = gst::init();
        let parse: SalsaSpatialParse = glib::Object::new();
        assert!(parse.is::<gst_base::BaseParse>());
        let decode: SalsaSpatialDec = glib::Object::new();
        assert!(decode.is::<gst_audio::AudioDecoder>());
    }

    #[test]
    fn parser_advertises_salsa_spatial_caps() {
        let _ = gst::init();
        let parse: SalsaSpatialParse = glib::Object::new();

        let sink = parse.pad_template("sink").expect("sink template");
        let sink_caps = sink.caps();
        let wanted = gst::Caps::builder("audio/x-salsa-spatial")
            .field("rate", 48_000)
            .field("channels", 8)
            .field("object-count", 6)
            .build();
        assert!(
            sink_caps.can_intersect(&wanted),
            "sink template does not accept {wanted}"
        );

        let src = parse.pad_template("src").expect("src template");
        let src_caps = src.caps();
        let structure = src_caps.structure(0).expect("src structure");
        assert_eq!(structure.name(), "audio/x-salsa-spatial");
        assert_eq!(structure.get::<bool>("framed"), Ok(true));
        assert_eq!(structure.get::<&str>("alignment"), Ok("frame"));
        assert!(src_caps.can_intersect(&wanted));
    }
}
