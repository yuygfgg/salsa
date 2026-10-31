//! Caps-level checks for `salsaobjectaudioadapt` and Dolby meta input.

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstsalsaobjectaudio::{HrtfRender, Oar, ObjectAudioAdapt};

fn init() {
    let _ = gst::init();
    let _ = gst::Element::register(
        None,
        "salsaobjectaudioadapt",
        gst::Rank::PRIMARY,
        ObjectAudioAdapt::static_type(),
    );
    let _ = gst::Element::register(None, "salsaoar", gst::Rank::PRIMARY, Oar::static_type());
    let _ = gst::Element::register(
        None,
        "salsahrtfrender",
        gst::Rank::PRIMARY,
        HrtfRender::static_type(),
    );
}

fn structure(format: &str, features: Option<&str>) -> gst::Caps {
    let caps = match features {
        Some(features) => format!(
            "audio/x-raw({features}), format=(string){format}, \
             layout=(string)interleaved, channels=(int)16, rate=(int)48000, \
             channel-mask=(bitmask)0"
        ),
        None => format!(
            "audio/x-raw, format=(string){format}, layout=(string)interleaved, \
             channels=(int)16, rate=(int)48000, channel-mask=(bitmask)0"
        ),
    };
    caps.parse().unwrap()
}

#[test]
fn adapt_sink_accepts_salsa_and_dolby_metas() {
    init();
    let adapt: ObjectAudioAdapt = glib::Object::new();
    let sink = adapt.static_pad("sink").expect("sink").query_caps(None);
    for features in [
        Some("memory:SystemMemory, meta:ObjectAudioMeta"),
        Some("meta:ObjectAudioMeta"),
        Some("memory:SystemMemory, meta:DlbObjectAudioMeta"),
        Some("meta:DlbObjectAudioMeta"),
        None,
    ] {
        let given = structure("F32LE", features);
        assert!(
            !sink.intersect(&given).is_empty(),
            "sink must accept {features:?}"
        );
    }
}

#[test]
fn adapt_src_advertises_the_dolby_feature() {
    init();
    let adapt: ObjectAudioAdapt = glib::Object::new();
    let src = adapt.static_pad("src").expect("src").query_caps(None);
    let features = src.features(0).expect("features");
    assert!(features.contains("meta:DlbObjectAudioMeta"));
    assert!(!features.contains("meta:ObjectAudioMeta"));
}

#[test]
fn adapt_properties_round_trip() {
    init();
    let adapt: ObjectAudioAdapt = glib::Object::new();
    assert!(!adapt.property::<bool>("remove"));
    assert!(!adapt.property::<bool>("float"));
    adapt.set_property("remove", true);
    adapt.set_property("float", true);
    assert!(adapt.property::<bool>("remove"));
    assert!(adapt.property::<bool>("float"));
}

#[test]
fn renderer_sinks_require_the_salsa_adapter_for_dolby_meta() {
    init();
    let dolby = structure("F32LE", Some("meta:DlbObjectAudioMeta"));

    let oar: Oar = glib::Object::new();
    let oar_sink = oar.static_pad("sink").expect("sink").query_caps(None);
    assert!(
        oar_sink.intersect(&dolby).is_empty(),
        "salsaoar must not consume Dolby meta directly; insert salsaobjectaudioadapt"
    );

    let hrtf: HrtfRender = glib::Object::new();
    let hrtf_sink = hrtf.static_pad("sink").expect("sink").query_caps(None);
    assert!(
        hrtf_sink.intersect(&dolby).is_empty(),
        "salsahrtfrender must not consume Dolby meta directly; insert salsaobjectaudioadapt"
    );
}

#[test]
fn the_three_object_audio_elements_are_registered() {
    init();
    for name in ["salsaobjectaudioadapt", "salsaoar", "salsahrtfrender"] {
        assert!(
            gst::ElementFactory::find(name).is_some(),
            "{name} is not registered"
        );
    }
}
