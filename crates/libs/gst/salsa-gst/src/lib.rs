//! Small helpers shared by the Salsa GStreamer plugins and applications.
//!
//! The plugins keep their state behind a `Mutex`. A panic in one streaming
//! thread would otherwise poison the mutex and turn every later property or
//! state callback into a second panic, so the plugins recover the guard instead
//! of unwrapping the lock result.
//!
//! The crate also owns the pieces of GStreamer integration that used to be
//! copied into every plugin and app: element construction/registration,
//! device enumeration, and the `ElementImpl` boilerplate.

use std::sync::{Mutex, MutexGuard};

use gst::glib;
use gst::glib::prelude::*;
use gst::prelude::*;
use gst_audio::prelude::*;
use gstreamer as gst;
use gstreamer_audio as gst_audio;

pub mod device;

/// Lock `mutex`, recovering the guard if a previous holder panicked.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Construct an element by factory name.
pub fn element(factory: &str) -> Result<gst::Element, glib::BoolError> {
    gst::ElementFactory::make(factory).build()
}

/// Register an element without a plugin, ignoring "already registered".
///
/// Applications register the plugin elements explicitly so they do not depend
/// on the host's GStreamer plugin search path.
pub fn register_element<T>(name: &str)
where
    T: StaticType + IsA<gst::Element>,
{
    let _ = gst::Element::register(None, name, gst::Rank::PRIMARY, T::static_type());
}

/// Implement the static `ElementImpl::metadata` body.
///
/// The generated method caches the metadata in a `OnceLock`; call it inside an
/// `ElementImpl` impl. `name`, `klass` and `description` describe the element.
#[macro_export]
macro_rules! element_metadata {
    ($name:literal, $klass:literal, $description:literal) => {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
                std::sync::OnceLock::new();
            Some(METADATA.get_or_init(|| {
                gst::subclass::ElementMetadata::new($name, $klass, $description, "salsa")
            }))
        }
    };
}

/// Push caps from a `BaseParse` subclass, returning whether the event was
/// accepted.
///
/// Call from a `BaseParseImpl` method; the expansion uses `Self`, `gst` and
/// `gst_base`, which are in scope in every Salsa parser.
#[macro_export]
macro_rules! push_parse_caps {
    ($this:expr, $caps:expr) => {{
        let borrowed = $this.obj();
        let object: &<Self as gst::glib::subclass::prelude::ObjectSubclass>::Type = borrowed.as_ref();
        <<Self as gst::glib::subclass::prelude::ObjectSubclass>::Type as gst_base::prelude::BaseParseExtManual>::src_pad(object)
            .push_event(gst::event::Caps::new(&$caps))
    }};
}

/// Implement the static `ElementImpl::metadata` body for an element with the
/// given metadata struct.
#[macro_export]
macro_rules! element_metadata_struct {
    ($value:expr) => {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
                std::sync::OnceLock::new();
            Some(METADATA.get_or_init(|| $value))
        }
    };
}

/// Implement the static `ElementImpl::pad_templates` body.
#[macro_export]
macro_rules! pad_templates {
    ($value:expr) => {
        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: std::sync::OnceLock<Vec<gst::PadTemplate>> =
                std::sync::OnceLock::new();
            TEMPLATES.get_or_init(|| $value)
        }
    };
}

/// Push decoded units, treating a flush race as a normal stop.
///
/// `finish_frame` returns `Flushing` when a seek starts while a frame is in
/// flight. Propagating it makes BaseParse treat the downstream stop as a fatal
/// stream error, so drop the remaining units instead; `FLUSH_STOP` resets the
/// decoder.
pub fn drain_frames<I, F>(frames: I, mut push: F) -> Result<(), gst::FlowError>
where
    I: IntoIterator,
    F: FnMut(I::Item) -> Result<(), gst::FlowError>,
{
    for frame in frames {
        match push(frame) {
            Ok(()) => {}
            Err(gst::FlowError::Flushing) => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Apply the standard `AudioDecoder` configuration used by the Salsa decoders.
pub fn configure_audio_decoder<T>(object: &T)
where
    T: IsA<gst_audio::AudioDecoder> + gst_audio::prelude::AudioDecoderExtManual,
{
    object.set_needs_format(true);
    object.set_estimate_rate(true);
    object.set_use_default_pad_acceptcaps(true);
    let sink_pad = object.sink_pad();
    sink_pad.set_object_flags(gst::ObjectFlags::from_bits_retain(
        gst::PadFlags::ACCEPT_TEMPLATE.bits(),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_a_poisoned_lock() {
        let mutex = Mutex::new(1);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = lock(&mutex);
            panic!("poison the mutex");
        }));
        assert_eq!(*lock(&mutex), 1);
    }
}
