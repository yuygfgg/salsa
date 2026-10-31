use std::slice;
use std::sync::OnceLock;

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::meta::ObjectAudioMeta;

/// Where a borrowed payload came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadSource {
    ObjectAudio,
    DlbObjectAudio,
}

/// A payload borrowed from a buffer meta.
#[derive(Debug, Clone, Copy)]
pub struct BorrowedPayload<'a> {
    pub source: PayloadSource,
    pub bytes: &'a [u8],
    pub offset: usize,
    pub bpf: usize,
}

#[repr(C)]
#[allow(dead_code)]
struct DlbObjectAudioMetaRaw {
    meta: gst::ffi::GstMeta,
    payload: *mut std::ffi::c_void,
    size: usize,
    offset: usize,
    bpf: usize,
}

fn meta_info(name: &std::ffi::CStr) -> Option<*const gst::ffi::GstMetaInfo> {
    let info = unsafe { gst::ffi::gst_meta_get_info(name.as_ptr()) };
    if info.is_null() { None } else { Some(info) }
}

fn dlb_meta_info() -> Option<*const gst::ffi::GstMetaInfo> {
    static INFO: OnceLock<usize> = OnceLock::new();
    if let Some(address) = INFO.get() {
        return Some(*address as *const gst::ffi::GstMetaInfo);
    }
    // The Dolby reference plugins put the meta implementation in
    // `libdlb_metalib.dylib`, but only register it on first use. Ask the
    // library to register it, then look it up by name again.
    let info = meta_info(c"DlbObjectAudioMeta")
        .or_else(register_dolby_meta)
        .or_else(|| meta_info(c"DlbObjectAudioMeta"))?;
    let _ = INFO.set(info as usize);
    Some(info)
}

#[cfg(unix)]
type DlbMetaInfoFn = unsafe extern "C" fn() -> *const gst::ffi::GstMetaInfo;

#[cfg(unix)]
fn lookup_dlb_symbol(handle: *mut std::ffi::c_void) -> Option<DlbMetaInfoFn> {
    let symbol = unsafe { libc::dlsym(handle, c"dlb_object_audio_meta_get_info".as_ptr()) };
    if symbol.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<*mut std::ffi::c_void, DlbMetaInfoFn>(symbol) })
    }
}

/// Register Dolby's `DlbObjectAudioMeta` by calling the implementation that
/// ships with the Reference Player's `libdlb_metalib.dylib`.
///
/// Some Dolby plugins are loaded with `RTLD_LOCAL`, so the symbol is not
/// always visible through `RTLD_DEFAULT`. Re-open the plugin with
/// `RTLD_GLOBAL` and resolve the symbol against that handle instead.
#[cfg(unix)]
fn register_dolby_meta() -> Option<*const gst::ffi::GstMetaInfo> {
    if let Some(function) = lookup_dlb_symbol(libc::RTLD_DEFAULT) {
        let info = unsafe { function() };
        if !info.is_null() {
            return Some(info);
        }
    }

    for element in ["dlboar", "dlbac3dec", "dlbdapvr", "dlbtruehddec"] {
        let Some(factory) = gst::ElementFactory::find(element) else {
            continue;
        };
        let Some(plugin) = factory.plugin() else {
            continue;
        };
        let Some(filename) = plugin.filename() else {
            continue;
        };
        let Ok(path) = std::ffi::CString::new(filename.to_string_lossy().as_bytes()) else {
            continue;
        };
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL) };
        if handle.is_null() {
            continue;
        }
        if let Some(function) = lookup_dlb_symbol(handle) {
            let info = unsafe { function() };
            if !info.is_null() {
                return Some(info);
            }
        }
    }
    None
}

#[cfg(not(unix))]
fn register_dolby_meta() -> Option<*const gst::ffi::GstMetaInfo> {
    None
}

fn object_meta_info() -> Option<*const gst::ffi::GstMetaInfo> {
    static INFO: OnceLock<usize> = OnceLock::new();
    if let Some(address) = INFO.get() {
        return Some(*address as *const gst::ffi::GstMetaInfo);
    }
    let info = meta_info(c"ObjectAudioMeta")?;
    let _ = INFO.set(info as usize);
    Some(info)
}

/// Whether any process participant has registered `ObjectAudioMeta`.
///
/// This is a read-only lookup; it never registers the type. A renderer can use
/// it to decide whether the upstream decoder is present.
pub fn object_audio_available() -> bool {
    object_meta_info().is_some()
}

/// Whether `DlbObjectAudioMeta` is available in this process.
///
/// If it is not registered yet, this may ask a loaded Dolby plugin to register
/// its own implementation; this crate never registers a conflicting type.
pub fn dlb_available() -> bool {
    dlb_meta_info().is_some()
}

/// Read `ObjectAudioMeta` without registering it.
///
/// Two GStreamer plugin dylibs built from this workspace each carry their own
/// copy of the meta crate. Registering the GType in both copies is a fatal
/// GType error, so readers use `gst_meta_get_info` and the C layout instead of
/// the lazy registration path used by `add()`.
pub fn object_audio_payload<'a>(buffer: &'a gst::BufferRef) -> Option<BorrowedPayload<'a>> {
    let info = object_meta_info()?;
    let raw = unsafe {
        gst::ffi::gst_buffer_get_meta(buffer.as_mut_ptr(), (*info).api) as *const ObjectAudioMeta
    };
    if raw.is_null() {
        return None;
    }
    let raw = unsafe { &*raw };
    if raw.payload.is_null() || raw.size == 0 {
        return None;
    }
    Some(BorrowedPayload {
        source: PayloadSource::ObjectAudio,
        bytes: unsafe { slice::from_raw_parts(raw.payload.cast::<u8>(), raw.size) },
        offset: raw.offset,
        bpf: raw.bpf,
    })
}

/// Read `DlbObjectAudioMeta`.
///
/// If the type is not registered yet, this may ask a loaded Dolby plugin to
/// register its own implementation. This crate never registers the type.
pub fn dlb_payload<'a>(buffer: &'a gst::BufferRef) -> Option<BorrowedPayload<'a>> {
    let info = dlb_meta_info()?;
    let raw = unsafe {
        gst::ffi::gst_buffer_get_meta(buffer.as_mut_ptr(), (*info).api)
            as *const DlbObjectAudioMetaRaw
    };
    if raw.is_null() {
        return None;
    }
    let raw = unsafe { &*raw };
    if raw.payload.is_null() || raw.size == 0 {
        return None;
    }
    Some(BorrowedPayload {
        source: PayloadSource::DlbObjectAudio,
        bytes: unsafe { slice::from_raw_parts(raw.payload.cast::<u8>(), raw.size) },
        offset: raw.offset,
        bpf: raw.bpf,
    })
}

/// Read every `ObjectAudioMeta` on a buffer, in attachment order.
pub fn object_audio_payloads<'a>(buffer: &'a gst::BufferRef) -> Vec<BorrowedPayload<'a>> {
    let Some(info) = object_meta_info() else {
        return Vec::new();
    };
    iterate_payloads(buffer, info, PayloadSource::ObjectAudio)
}

/// Read every `DlbObjectAudioMeta` on a buffer, in attachment order.
pub fn dlb_payloads<'a>(buffer: &'a gst::BufferRef) -> Vec<BorrowedPayload<'a>> {
    let Some(info) = dlb_meta_info() else {
        return Vec::new();
    };
    iterate_payloads(buffer, info, PayloadSource::DlbObjectAudio)
}

fn iterate_payloads<'a>(
    buffer: &'a gst::BufferRef,
    info: *const gst::ffi::GstMetaInfo,
    source: PayloadSource,
) -> Vec<BorrowedPayload<'a>> {
    let mut out = Vec::new();
    let mut state: *mut std::ffi::c_void = std::ptr::null_mut();
    unsafe {
        loop {
            let meta = gst::ffi::gst_buffer_iterate_meta_filtered(
                buffer.as_mut_ptr(),
                &mut state,
                (*info).api,
            );
            if meta.is_null() {
                break;
            }
            let (payload, size, offset, bpf) = match source {
                PayloadSource::ObjectAudio => {
                    let raw = &*(meta as *const ObjectAudioMeta);
                    (raw.payload, raw.size, raw.offset, raw.bpf)
                }
                PayloadSource::DlbObjectAudio => {
                    let raw = &*(meta as *const DlbObjectAudioMetaRaw);
                    (raw.payload, raw.size, raw.offset, raw.bpf)
                }
            };
            if payload.is_null() || size == 0 {
                continue;
            }
            out.push(BorrowedPayload {
                source,
                bytes: slice::from_raw_parts(payload.cast::<u8>(), size),
                offset,
                bpf,
            });
        }
    }
    out
}

/// Copy a payload into a new `DlbObjectAudioMeta` on `buffer`.
///
/// The meta type must already be registered by another plugin (normally
/// Dolby's `dlbac3dec` / `dlboar`); this crate never registers it.
pub fn add_dlb_payload(
    buffer: &mut gst::BufferRef,
    bytes: &[u8],
    offset: usize,
    bpf: usize,
) -> Result<(), crate::MetaError> {
    if bytes.is_empty() {
        return Err(crate::MetaError::EmptyPayload);
    }
    if bpf == 0 {
        return Err(crate::MetaError::InvalidBpf);
    }
    let info = dlb_meta_info().ok_or(crate::MetaError::DlbNotRegistered)?;
    unsafe {
        let meta = gst::ffi::gst_buffer_add_meta(buffer.as_mut_ptr(), info, std::ptr::null_mut());
        if meta.is_null() {
            return Err(crate::MetaError::AddFailed);
        }
        let raw = &mut *(meta as *mut DlbObjectAudioMetaRaw);
        let copy = gst::glib::ffi::g_malloc(bytes.len()) as *mut u8;
        if copy.is_null() {
            return Err(crate::MetaError::AddFailed);
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), copy, bytes.len());
        raw.payload = copy.cast::<std::ffi::c_void>();
        raw.size = bytes.len();
        raw.offset = offset;
        raw.bpf = bpf;
    }
    Ok(())
}
