use gstreamer as gst;

/// Caps feature that marks a raw buffer as carrying [`crate::ObjectAudioMeta`].
pub const CAPS_FEATURE: &str = "meta:ObjectAudioMeta";

/// Caps feature used by Dolby's `DlbObjectAudioMeta`.
///
/// The core renderers do not advertise it; only the Dolby side of
/// `salsaobjectaudioadapt` does.
pub const DLB_CAPS_FEATURE: &str = "meta:DlbObjectAudioMeta";

/// CapsFeature set containing only [`CAPS_FEATURE`].
pub fn caps_features() -> gst::CapsFeatures {
    gst::CapsFeatures::new([CAPS_FEATURE])
}

/// Feature sets a renderer sink should advertise.
///
/// Salsa writes `(memory:SystemMemory, meta:ObjectAudioMeta)` from its
/// decoders, while some hand-built buffers only carry `(meta:ObjectAudioMeta)`.
/// Both spellings are accepted. The Dolby feature and plain raw are
/// intentionally absent: a Dolby stream must go through
/// `salsaobjectaudioadapt`, and renderers require a `SpatialPayload`.
pub fn sink_feature_sets() -> Vec<Option<gst::CapsFeatures>> {
    vec![
        Some(gst::CapsFeatures::new([CAPS_FEATURE])),
        Some(gst::CapsFeatures::new([
            "memory:SystemMemory",
            CAPS_FEATURE,
        ])),
    ]
}

/// Whether any structure in `caps` carries [`CAPS_FEATURE`].
pub fn has_feature(caps: &gst::CapsRef) -> bool {
    (0..caps.size()).any(|index| {
        caps.features(index)
            .map(|features| features.contains(CAPS_FEATURE))
            .unwrap_or(false)
    })
}

/// Add [`CAPS_FEATURE`] to every structure in `caps`.
pub fn ensure_feature(caps: &mut gst::CapsRef) {
    for index in 0..caps.size() {
        if let Some(features) = caps.features_mut(index) {
            features.add(CAPS_FEATURE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_and_detects_the_feature() {
        let _ = gst::init();
        let mut caps = gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .build();
        assert!(!has_feature(&caps));
        ensure_feature(caps.get_mut().unwrap());
        assert!(has_feature(&caps));
        assert!(caps.features(0).unwrap().contains("meta:ObjectAudioMeta"));
    }

    #[test]
    fn renderer_feature_sets_do_not_advertise_dolby() {
        let _ = gst::init();
        let features = sink_feature_sets();
        assert!(
            features
                .iter()
                .flatten()
                .any(|features| features.contains(CAPS_FEATURE))
        );
        assert!(
            !features
                .iter()
                .flatten()
                .any(|features| features.contains(DLB_CAPS_FEATURE))
        );
        assert!(features.iter().all(Option::is_some));
    }
}
