use gstreamer as gst;
use gstreamer::glib;
use gstreamer_base::prelude::*;
use gstsalsaobjectaudio::Oar;

#[test]
fn sink_caps_intersect_object_raw() {
    let _ = gst::init();
    let oar: Oar = glib::Object::new();
    let given: gst::Caps = "audio/x-raw(memory:SystemMemory, meta:ObjectAudioMeta), \
        format=(string)F32LE, layout=(string)interleaved, channels=(int)16, \
        rate=(int)48000"
        .parse()
        .unwrap();
    let queried = oar.sink_pad().query_caps(None);
    let intersect = queried.intersect(&given);
    assert!(!intersect.is_empty());
}

#[test]
fn downstream_mask_can_override_the_layout_property() {
    let _ = gst::init();
    let oar: Oar = glib::Object::new();
    oar.set_property("layout", "5.1");
    let filter: gst::Caps = "audio/x-raw, format=(string)F32LE, layout=(string)interleaved, \
        channels=(int)12, channel-mask=(bitmask)0x33c3f"
        .parse()
        .unwrap();
    let queried = oar.src_pad().query_caps(Some(&filter));
    let intersect = queried.intersect(&filter);
    assert!(!intersect.is_empty());
    let structure = intersect.structure(0).unwrap();
    assert_eq!(structure.get::<i32>("channels"), Ok(12));
}

#[test]
fn sink_caps_accept_dynamic_object_counts() {
    let _ = gst::init();
    let oar: Oar = glib::Object::new();
    let queried = oar.sink_pad().query_caps(None);
    for channels in [1, 2, 13, 15, 16, 128, 256, 1024] {
        let given: gst::Caps = format!(
            "audio/x-raw(memory:SystemMemory, meta:ObjectAudioMeta), \
             format=(string)F32LE, layout=(string)interleaved, channels=(int){channels}, \
             rate=(int)48000"
        )
        .parse()
        .unwrap();
        assert!(
            !queried.intersect(&given).is_empty(),
            "salsaoar rejected {channels} channels"
        );
    }
}

#[test]
fn hrtf_sink_caps_accept_dynamic_object_counts() {
    use gstsalsaobjectaudio::HrtfRender;

    let _ = gst::init();
    let hrtf: HrtfRender = glib::Object::new();
    let queried = hrtf.sink_pad().query_caps(None);
    for channels in [1, 2, 13, 15, 16, 128, 256] {
        let given: gst::Caps = format!(
            "audio/x-raw(memory:SystemMemory, meta:ObjectAudioMeta), \
             format=(string)F32LE, layout=(string)interleaved, channels=(int){channels}, \
             rate=(int)48000"
        )
        .parse()
        .unwrap();
        assert!(
            !queried.intersect(&given).is_empty(),
            "salsahrtfrender rejected {channels} channels"
        );
    }
}

#[test]
fn adapter_follows_the_dolby_32_channel_sink_bound() {
    use gstsalsaobjectaudio::ObjectAudioAdapt;

    let _ = gst::init();
    let adapt: ObjectAudioAdapt = glib::Object::new();
    let queried = adapt.sink_pad().query_caps(None);
    for channels in [1, 16, 32] {
        let given: gst::Caps = format!(
            "audio/x-raw(memory:SystemMemory, meta:ObjectAudioMeta), \
             format=(string)F32LE, layout=(string)interleaved, channels=(int){channels}, \
             rate=(int)48000"
        )
        .parse()
        .unwrap();
        assert!(
            !queried.intersect(&given).is_empty(),
            "salsaobjectaudioadapt rejected {channels} channels"
        );
    }
    let too_wide: gst::Caps = "audio/x-raw(memory:SystemMemory, meta:ObjectAudioMeta), \
        format=(string)F32LE, layout=(string)interleaved, channels=(int)33, \
        rate=(int)48000"
        .parse()
        .unwrap();
    assert!(
        queried.intersect(&too_wide).is_empty(),
        "the adapter must not advertise beyond Dolby's dlboar sink caps"
    );
}
