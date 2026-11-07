//! In-process registration checks live in their own test binary so the
//! plugin's unit tests can construct subclass types without racing GType
//! registration.

use gstreamer as gst;

use gstsalsaspatial::{CAPS, SALSA_SPATIAL_DEC, SALSA_SPATIAL_PARSE};

#[test]
fn registry_finds_both_factories() {
    salsa_gst_registry::init().expect("register salsa elements");
    assert!(gst::ElementFactory::find(SALSA_SPATIAL_PARSE).is_some());
    assert!(gst::ElementFactory::find(SALSA_SPATIAL_DEC).is_some());
    assert_eq!(
        salsa_gst_registry::codec_elements(salsa_codec::Codec::SalsaSpatial).caps,
        CAPS
    );
    assert_eq!(CAPS, "audio/x-salsa-spatial");
}
