//! Register every Salsa GStreamer element in-process and map codecs to the
//! elements that handle them.
//!
//! Applications and tests used to open-code the same `Element::register`
//! sequence (and the GUI additionally loaded the plugin binaries from a
//! hand-maintained list). The plugin crates own their factory names; this
//! registry is the single place that assembles them.

use gst::glib;
use gstreamer as gst;
use salsa_codec::Codec;

/// GStreamer element factory names registered by the Salsa plugins.
pub mod elements {
    pub use gstsalsadamf::{DAMF_SINK, DAMF_SRC};
    pub use gstsalsaeac3joc::{EAC3_DEC, EAC3_PARSE};
    pub use gstsalsaobjectaudio::{HRTF_RENDER, OAR, OBJECT_AUDIO_ADAPT};
    pub use gstsalsaspatial::{SALSA_SPATIAL_DEC, SALSA_SPATIAL_PARSE};
    pub use gstsalsatruehd::{TRUEHD_DEC, TRUEHD_PARSE};
}

/// GStreamer transport description for a compressed codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecElements {
    pub caps: &'static str,
    pub parser: &'static str,
    pub decoder: &'static str,
}

/// Return the caps, parser and decoder that handle `codec`.
pub const fn codec_elements(codec: Codec) -> CodecElements {
    match codec {
        Codec::Eac3 => CodecElements {
            caps: gstsalsaeac3joc::CAPS,
            parser: elements::EAC3_PARSE,
            decoder: elements::EAC3_DEC,
        },
        Codec::TrueHd => CodecElements {
            caps: gstsalsatruehd::CAPS,
            parser: elements::TRUEHD_PARSE,
            decoder: elements::TRUEHD_DEC,
        },
        Codec::SalsaSpatial => CodecElements {
            caps: gstsalsaspatial::CAPS,
            parser: elements::SALSA_SPATIAL_PARSE,
            decoder: elements::SALSA_SPATIAL_DEC,
        },
    }
}

/// Register all Salsa elements without a plugin.
pub fn register_all() {
    gstsalsadamf::register_all();
    gstsalsaeac3joc::register_all();
    gstsalsaobjectaudio::register_all();
    gstsalsaspatial::register_all();
    gstsalsatruehd::register_all();
}

/// Initialize GStreamer, the object-audio meta API and every Salsa element.
pub fn init() -> Result<(), glib::Error> {
    objectaudio_meta::init()?;
    register_all();
    Ok(())
}
