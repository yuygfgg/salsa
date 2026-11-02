use std::ffi::{CStr, c_char};
use std::ptr;
use std::slice;
use std::sync::OnceLock;

use gst::glib;
use gst::glib::translate::{IntoGlib, from_glib};
use gst::meta::MetaAPI;
use gstreamer as gst;
use objectaudio_ir::SpatialPayload;

use crate::MetaError;

const META_API_NAME: &CStr = c"ObjectAudioMetaAPI";
const META_IMPL_NAME: &CStr = c"ObjectAudioMeta";
const META_TAGS: [&CStr; 2] = [c"oamdi", c"objectaudio"];

/// Payload semantics selected by [`ObjectAudioMeta::payload_type`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum PayloadType {
    /// A versioned salsa `SpatialPayload`.
    Spatial = 0,
}

impl PayloadType {
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Spatial),
            _ => None,
        }
    }
}

/// Object-audio metadata attached to a raw audio buffer.
#[repr(C)]
#[derive(Debug)]
pub struct ObjectAudioMeta {
    pub meta: gst::ffi::GstMeta,
    pub version: u32,
    pub payload_type: u32,
    pub payload: *mut std::ffi::c_void,
    pub size: usize,
    pub offset: usize,
    pub bpf: usize,
}

// The payload is owned, copied on add, and never mutated through the safe API.
unsafe impl Send for ObjectAudioMeta {}
unsafe impl Sync for ObjectAudioMeta {}

impl ObjectAudioMeta {
    pub fn payload(&self) -> &[u8] {
        if self.payload.is_null() || self.size == 0 {
            return &[];
        }
        unsafe { slice::from_raw_parts(self.payload.cast::<u8>(), self.size) }
    }

    pub fn payload_type(&self) -> PayloadType {
        PayloadType::from_u32(self.payload_type).unwrap_or(PayloadType::Spatial)
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn bpf(&self) -> usize {
        self.bpf
    }
}

/// A decoded spatial payload borrowed from a buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialSnapshot {
    pub payload: SpatialPayload,
    pub bpf: usize,
}

unsafe impl MetaAPI for ObjectAudioMeta {
    type GstType = ObjectAudioMeta;

    fn meta_api() -> glib::Type {
        meta_api_type()
    }
}

static META_API: OnceLock<glib::Type> = OnceLock::new();
static META_INFO: OnceLock<usize> = OnceLock::new();

/// The GType registered for `ObjectAudioMeta`.
///
/// Every plugin dylib built from this workspace carries its own copy of this
/// crate. GType registration is process-global and rejects a second
/// registration of the same name, so look up an already-registered API/info
/// before attempting to create one.
pub fn meta_api_type() -> glib::Type {
    ensure_gst_init();
    *META_API.get_or_init(|| unsafe {
        let info = gst::ffi::gst_meta_get_info(META_IMPL_NAME.as_ptr());
        if !info.is_null() {
            return from_glib((*info).api);
        }
        let tags: [*const c_char; 3] = [META_TAGS[0].as_ptr(), META_TAGS[1].as_ptr(), ptr::null()];
        let api =
            gst::ffi::gst_meta_api_type_register(META_API_NAME.as_ptr(), tags.as_ptr().cast_mut());
        from_glib(api)
    })
}

/// Register the meta type with the GStreamer registry. Idempotent.
pub fn register() {
    let _ = meta_info();
}

pub(crate) fn meta_info() -> *const gst::ffi::GstMetaInfo {
    ensure_gst_init();
    let address = *META_INFO.get_or_init(|| {
        let existing = unsafe { gst::ffi::gst_meta_get_info(META_IMPL_NAME.as_ptr()) };
        if !existing.is_null() {
            return existing as usize;
        }
        let info = unsafe {
            gst::ffi::gst_meta_register(
                meta_api_type().into_glib(),
                META_IMPL_NAME.as_ptr(),
                std::mem::size_of::<ObjectAudioMeta>(),
                Some(init_meta),
                Some(free_meta),
                Some(transform_meta),
            )
        };
        assert!(!info.is_null(), "failed to register ObjectAudioMeta");
        info as usize
    });
    address as *const gst::ffi::GstMetaInfo
}

/// Scale a sample offset across a sample-rate change.
pub fn scale_offset(offset: usize, from_rate: u32, to_rate: u32) -> usize {
    if from_rate == 0 || to_rate == 0 || from_rate == to_rate {
        return offset;
    }
    ((offset as u128 * to_rate as u128) / from_rate as u128) as usize
}

/// Add a meta carrying a [`SpatialPayload`].
pub fn add_spatial(
    buffer: &mut gst::BufferRef,
    payload: &SpatialPayload,
    bpf: usize,
) -> Result<(), MetaError> {
    let bytes = payload.encode().map_err(MetaError::Payload)?;
    add_spatial_bytes(buffer, &bytes, bpf)
}

/// Add a meta carrying already-encoded `SpatialPayload` bytes.
pub fn add_spatial_bytes(
    buffer: &mut gst::BufferRef,
    bytes: &[u8],
    bpf: usize,
) -> Result<(), MetaError> {
    if bytes.is_empty() {
        return Err(MetaError::EmptyPayload);
    }
    if bpf == 0 {
        return Err(MetaError::InvalidBpf);
    }
    let payload = SpatialPayload::decode(bytes).map_err(MetaError::Payload)?;
    let _ = meta_info();
    let meta = unsafe {
        add_raw(
            buffer.as_mut_ptr(),
            bytes,
            payload.snapshot_offset.max(0) as usize,
            bpf,
            u32::from(SpatialPayload::VERSION),
            PayloadType::Spatial.as_u32(),
        )
    };
    if meta.is_null() {
        Err(MetaError::AddFailed)
    } else {
        Ok(())
    }
}

/// First `ObjectAudioMeta` on the buffer.
pub fn get(buffer: &gst::BufferRef) -> Option<gst::meta::MetaRef<'_, ObjectAudioMeta>> {
    let _ = meta_info();
    buffer.meta::<ObjectAudioMeta>()
}

/// Decode the first `ObjectAudioMeta` on the buffer as a spatial payload.
pub fn get_spatial(buffer: &gst::BufferRef) -> Option<SpatialSnapshot> {
    let meta = get(buffer)?;
    if meta.version != u32::from(SpatialPayload::VERSION)
        || meta.payload_type() != PayloadType::Spatial
    {
        return None;
    }
    let payload = SpatialPayload::decode(meta.payload()).ok()?;
    Some(SpatialSnapshot {
        payload,
        bpf: meta.bpf(),
    })
}

/// Iterate all `ObjectAudioMeta` instances on the buffer.
pub fn iter(
    buffer: &gst::BufferRef,
) -> impl Iterator<Item = gst::meta::MetaRef<'_, ObjectAudioMeta>> + '_ {
    let _ = meta_info();
    buffer.iter_meta::<ObjectAudioMeta>()
}

/// Decode every `ObjectAudioMeta` on the buffer, in attachment order.
pub fn iter_spatial(buffer: &gst::BufferRef) -> Vec<SpatialSnapshot> {
    iter(buffer)
        .filter(|meta| {
            meta.version == u32::from(SpatialPayload::VERSION)
                && meta.payload_type() == PayloadType::Spatial
        })
        .filter_map(|meta| {
            let payload = SpatialPayload::decode(meta.payload()).ok()?;
            Some(SpatialSnapshot {
                payload,
                bpf: meta.bpf(),
            })
        })
        .collect()
}

/// Remove every `ObjectAudioMeta` from the buffer.
pub fn remove_all(buffer: &mut gst::BufferRef) {
    let _ = meta_info();
    unsafe {
        loop {
            let meta =
                gst::ffi::gst_buffer_get_meta(buffer.as_mut_ptr(), meta_api_type().into_glib());
            if meta.is_null() {
                break;
            }
            if gst::ffi::gst_buffer_remove_meta(buffer.as_mut_ptr(), meta) == 0 {
                break;
            }
        }
    }
}

fn ensure_gst_init() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let _ = gst::init();
    });
}

unsafe extern "C" fn init_meta(
    meta: *mut gst::ffi::GstMeta,
    _params: glib::ffi::gpointer,
    _buffer: *mut gst::ffi::GstBuffer,
) -> glib::ffi::gboolean {
    let meta = unsafe { &mut *(meta as *mut ObjectAudioMeta) };
    meta.version = u32::from(SpatialPayload::VERSION);
    meta.payload_type = PayloadType::Spatial.as_u32();
    meta.payload = ptr::null_mut();
    meta.size = 0;
    meta.offset = 0;
    meta.bpf = 1;
    glib::ffi::GTRUE
}

unsafe extern "C" fn free_meta(meta: *mut gst::ffi::GstMeta, _buffer: *mut gst::ffi::GstBuffer) {
    let meta = unsafe { &mut *(meta as *mut ObjectAudioMeta) };
    if !meta.payload.is_null() {
        unsafe { glib::ffi::g_free(meta.payload) };
        meta.payload = ptr::null_mut();
    }
    meta.size = 0;
}

unsafe extern "C" fn transform_meta(
    dest: *mut gst::ffi::GstBuffer,
    meta: *mut gst::ffi::GstMeta,
    _buffer: *mut gst::ffi::GstBuffer,
    type_: glib::ffi::GQuark,
    data: glib::ffi::gpointer,
) -> glib::ffi::gboolean {
    let copy_quark = glib::Quark::from_str("gst-copy").into_glib();
    if type_ != copy_quark || data.is_null() {
        return glib::ffi::GTRUE;
    }

    let source = unsafe { &*(meta as *const ObjectAudioMeta) };
    let copy = unsafe { &*(data as *const gst::ffi::GstMetaTransformCopy) };
    if source.bpf == 0 || source.version != u32::from(SpatialPayload::VERSION) {
        return glib::ffi::GFALSE;
    }
    let Ok(mut payload) = SpatialPayload::decode(source.payload()) else {
        return glib::ffi::GFALSE;
    };

    if copy.region != 0 {
        let Some(sliced) = payload.slice((copy.offset / source.bpf) as i64, copy.size / source.bpf)
        else {
            return glib::ffi::GTRUE;
        };
        payload = sliced;
    }
    let offset = payload.snapshot_offset.max(0) as usize;

    let Ok(bytes) = payload.encode() else {
        return glib::ffi::GFALSE;
    };
    let added = unsafe {
        add_raw(
            dest,
            &bytes,
            offset,
            source.bpf,
            source.version,
            source.payload_type,
        )
    };
    if added.is_null() {
        glib::ffi::GFALSE
    } else {
        glib::ffi::GTRUE
    }
}

pub(crate) unsafe fn add_raw(
    dest: *mut gst::ffi::GstBuffer,
    payload: &[u8],
    offset: usize,
    bpf: usize,
    version: u32,
    payload_type: u32,
) -> *mut ObjectAudioMeta {
    if payload.is_empty() || bpf == 0 || dest.is_null() {
        return ptr::null_mut();
    }

    let data = unsafe { glib::ffi::g_malloc(payload.len()) };
    if data.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(payload.as_ptr(), data.cast::<u8>(), payload.len());
    }

    let meta = unsafe {
        gst::ffi::gst_buffer_add_meta(dest, meta_info(), ptr::null_mut()) as *mut ObjectAudioMeta
    };
    if meta.is_null() {
        unsafe { glib::ffi::g_free(data) };
        return ptr::null_mut();
    }

    unsafe {
        (*meta).version = version;
        (*meta).payload_type = payload_type;
        (*meta).payload = data;
        (*meta).size = payload.len();
        (*meta).offset = offset;
        (*meta).bpf = bpf;
    }
    meta
}

#[cfg(test)]
mod tests {
    use super::*;
    use objectaudio_ir::{ObjectEvent, ObjectState, PayloadError};
    use proptest::prelude::*;

    fn buffer(size: usize) -> gst::Buffer {
        let _ = gst::init();
        gst::Buffer::with_size(size).unwrap()
    }

    fn payload() -> SpatialPayload {
        SpatialPayload::new(
            48_000,
            vec![ObjectState::new(1, [0.5, 0.5, 0.0])],
            vec![ObjectEvent {
                offset: 64,
                transition: objectaudio_ir::Transition::All(32),
                state: ObjectState::new(1, [1.0, 0.0, 0.0]),
            }],
        )
    }

    #[test]
    fn scales_offsets_for_rate_changes() {
        assert_eq!(scale_offset(100, 48_000, 96_000), 200);
        assert_eq!(scale_offset(100, 96_000, 48_000), 50);
        assert_eq!(scale_offset(100, 48_000, 48_000), 100);
        assert_eq!(scale_offset(100, 0, 48_000), 100);
    }

    #[test]
    fn adds_gets_and_iterates() {
        let mut buffer = buffer(256);
        add_spatial(
            buffer.get_mut().unwrap(),
            &payload().with_snapshot_offset(7),
            4,
        )
        .unwrap();
        add_spatial(
            buffer.get_mut().unwrap(),
            &payload().with_snapshot_offset(8),
            4,
        )
        .unwrap();

        let meta = get(&buffer).unwrap();
        assert_eq!(meta.offset(), 7);
        assert_eq!(meta.bpf(), 4);
        assert_eq!(meta.version, u32::from(SpatialPayload::VERSION));
        assert_eq!(meta.payload_type(), PayloadType::Spatial);
        assert_eq!(iter(&buffer).count(), 2);
        assert_eq!(iter_spatial(&buffer).len(), 2);
        assert_eq!(
            get_spatial(&buffer).unwrap().payload,
            payload().with_snapshot_offset(7)
        );
    }

    #[test]
    fn rejects_invalid_payloads() {
        let mut buffer = buffer(16);
        assert_eq!(
            add_spatial(buffer.get_mut().unwrap(), &payload(), 0),
            Err(MetaError::InvalidBpf)
        );
        assert!(matches!(
            add_spatial_bytes(buffer.get_mut().unwrap(), b"not a payload", 4),
            Err(MetaError::Payload(PayloadError::Decode(_)))
        ));
    }

    #[test]
    fn copy_region_rebases_events_without_changing_ramps() {
        let mut buffer = buffer(1024);
        add_spatial(
            buffer.get_mut().unwrap(),
            &payload().with_snapshot_offset(64),
            8,
        )
        .unwrap();

        let copy = buffer
            .copy_region(
                gst::BufferCopyFlags::META | gst::BufferCopyFlags::MEMORY,
                128..1024,
            )
            .unwrap();
        let snapshot = get_spatial(&copy).unwrap();
        assert_eq!(snapshot.payload.snapshot_offset, 48);
        assert_eq!(snapshot.payload.events.len(), 1);
        assert_eq!(snapshot.payload.events[0].offset, 48);

        let copy = buffer
            .copy_region(
                gst::BufferCopyFlags::META | gst::BufferCopyFlags::MEMORY,
                0..128,
            )
            .unwrap();
        assert!(get(&copy).is_none());
    }

    #[test]
    fn copy_outside_the_event_region_drops_it() {
        let mut buffer = buffer(1024);
        add_spatial(buffer.get_mut().unwrap(), &payload(), 8).unwrap();

        let copy = buffer
            .copy_region(
                gst::BufferCopyFlags::META | gst::BufferCopyFlags::MEMORY,
                0..256,
            )
            .unwrap();
        let snapshot = get_spatial(&copy).unwrap();
        assert!(snapshot.payload.events.is_empty());
    }

    #[test]
    fn deep_copy_keeps_the_offset() {
        let mut buffer = buffer(256);
        add_spatial(
            buffer.get_mut().unwrap(),
            &payload().with_snapshot_offset(12),
            4,
        )
        .unwrap();

        let copy = buffer.copy_deep().unwrap();
        let meta = get(&copy).unwrap();
        assert_eq!(meta.offset(), 12);
        assert_eq!(
            get_spatial(&copy).unwrap().payload,
            payload().with_snapshot_offset(12)
        );
    }

    #[test]
    fn remove_all_drops_the_meta() {
        let mut buffer = buffer(64);
        add_spatial(buffer.get_mut().unwrap(), &payload(), 4).unwrap();
        assert!(get(&buffer).is_some());
        remove_all(buffer.get_mut().unwrap());
        assert!(get(&buffer).is_none());
    }

    proptest! {
        #[test]
        fn multiple_meta_copy_preserves_offsets(
            offsets in prop::collection::vec(0usize..1024, 1..8),
            bpf in 1usize..16,
        ) {
            let mut buffer = buffer(4096);
            for offset in &offsets {
                let payload = payload().with_snapshot_offset(*offset as i64);
                add_spatial(buffer.get_mut().unwrap(), &payload, bpf).unwrap();
            }

            let copy = buffer.copy_deep().unwrap();
            let metas = iter(&copy).collect::<Vec<_>>();
            prop_assert_eq!(metas.len(), offsets.len());
            for (meta, offset) in metas.iter().zip(offsets.iter()) {
                prop_assert_eq!(meta.offset(), *offset);
                prop_assert_eq!(meta.bpf(), bpf);
            }
        }
    }
}
