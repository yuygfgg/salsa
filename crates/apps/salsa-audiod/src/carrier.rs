//! Carrier input sources for the service.
//!
//! The production source is the bounded shared-memory ring written by the
//! macOS HAL or the Linux ALSA plug-in (`platform/common/salsa_carrier_shared.h`).
//! Offline validation reads an IEC 61937
//! capture (the `.spdif` fixtures) from a file. Both sources return one chunk at
//! a time; the depacketizer owns all framing and byte-order handling.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::clock::{host_now_ns, host_ticks_to_ns};

pub const SLOT_COUNT: usize = 16;
pub const SLOT_BYTES: usize = 64 * 1024;
pub const SHM_MAGIC: u32 = 0x5341_4c53;
pub const SHM_VERSION: u32 = 1;

pub const FLAG_DISCONTINUITY: u32 = 1 << 0;
pub const FLAG_DROPPED: u32 = 1 << 1;

/// Slots older than this are leftovers from a paused/stopped producer. Playing
/// them would anchor the service clock in the past and trigger a deadline miss
/// on the very first output buffer.
const MAX_SLOT_AGE: Duration = Duration::from_secs(2);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SlotHeader {
    generation: u64,
    start_sample: u64,
    host_time: u64,
    frames: u32,
    channels: u32,
    bytes_per_frame: u32,
    payload_bytes: u32,
    flags: u32,
    reserved: u32,
}

#[repr(C)]
struct Slot {
    header: SlotHeader,
    payload: [u8; SLOT_BYTES],
}

#[repr(C)]
struct Control {
    magic: AtomicU32,
    version: AtomicU32,
    generation: AtomicU64,
    declared_latency_frames: AtomicU64,
    latency_valid: AtomicU32,
    reserved0: AtomicU32,
    buffer_frames: AtomicU64,
    output_channels: AtomicU64,
    bytes_per_frame: AtomicU64,
    dropped_frames: AtomicU64,
    head: AtomicU64,
    tail: AtomicU64,
    clock_sample: AtomicU64,
    clock_host: AtomicU64,
    clock_seed: AtomicU64,
    clock_valid: AtomicU32,
    clock_sequence: AtomicU32,
    service_heartbeat: AtomicU64,
    service_markers: AtomicU64,
    rendered_markers: AtomicU64,
    late_markers: AtomicU64,
    reserved: [u8; 32],
}

#[repr(C)]
struct Shared {
    control: Control,
    slots: [Slot; SLOT_COUNT],
}

/// One chunk of carrier bytes and the device timeline of its first frame.
#[derive(Debug, Clone)]
pub struct CarrierChunk {
    pub bytes: Vec<u8>,
    /// Device sample position of the first carrier frame, when known.
    pub start_sample: Option<u64>,
    /// Host time of the first carrier frame, when known.
    pub host_time: Option<u64>,
    /// Carrier byte stride reported by the carrier producer, when known.
    pub bytes_per_frame: Option<u32>,
    pub generation: u64,
    pub flags: u32,
}

impl CarrierChunk {
    pub fn is_discontinuity(&self) -> bool {
        self.flags & (FLAG_DISCONTINUITY | FLAG_DROPPED) != 0
    }
}

/// One carrier byte source.
pub trait CarrierSource {
    fn next_chunk(&mut self) -> Result<Option<CarrierChunk>>;

    /// True when the source can block waiting for more carrier data.
    fn is_live(&self) -> bool {
        false
    }
}

/// Carrier byte source.
pub enum CarrierInput {
    File(FileInput),
    Shared(SharedInput),
    /// Pre-built chunks for lifecycle tests and diagnostic injection.
    Scripted(ScriptedInput),
}

impl CarrierInput {
    pub fn open_file(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::File(FileInput::open(path)?))
    }

    pub fn open_shared(name: &str, stop: Arc<AtomicBool>) -> Result<Self> {
        Ok(Self::Shared(SharedInput::open(name, stop)?))
    }

    pub fn open_scripted(chunks: Vec<CarrierChunk>) -> Self {
        Self::Scripted(ScriptedInput::new(chunks))
    }

    pub fn next_chunk(&mut self) -> Result<Option<CarrierChunk>> {
        self.as_source_mut().next_chunk()
    }

    /// True when the source can block waiting for more carrier data.
    pub fn is_live(&self) -> bool {
        self.as_source().is_live()
    }

    fn as_source(&self) -> &dyn CarrierSource {
        match self {
            Self::File(input) => input,
            Self::Shared(input) => input,
            Self::Scripted(input) => input,
        }
    }

    fn as_source_mut(&mut self) -> &mut dyn CarrierSource {
        match self {
            Self::File(input) => input,
            Self::Shared(input) => input,
            Self::Scripted(input) => input,
        }
    }
}

pub struct ScriptedInput {
    chunks: std::vec::IntoIter<CarrierChunk>,
}

impl ScriptedInput {
    pub fn new(chunks: Vec<CarrierChunk>) -> Self {
        Self {
            chunks: chunks.into_iter(),
        }
    }
}

impl CarrierSource for ScriptedInput {
    fn next_chunk(&mut self) -> Result<Option<CarrierChunk>> {
        Ok(self.chunks.next())
    }
}

pub struct FileInput {
    file: File,
    path: PathBuf,
}

impl FileInput {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)
            .with_context(|| format!("cannot open carrier capture {}", path.display()))?;
        Ok(Self { file, path })
    }
}

impl CarrierSource for FileInput {
    fn next_chunk(&mut self) -> Result<Option<CarrierChunk>> {
        let mut bytes = vec![0u8; 64 * 1024];
        let read = self
            .file
            .read(&mut bytes)
            .with_context(|| format!("cannot read {}", self.path.display()))?;
        if read == 0 {
            return Ok(None);
        }
        bytes.truncate(read);
        Ok(Some(CarrierChunk {
            bytes,
            start_sample: None,
            host_time: None,
            bytes_per_frame: None,
            generation: 0,
            flags: 0,
        }))
    }
}

pub struct SharedInput {
    shared: *mut Shared,
    stop: Arc<AtomicBool>,
    skipped: u64,
    max_slot_age: Duration,
    pending_discontinuity: bool,
}

// SAFETY: `SharedInput` owns the mapping and is used by exactly one thread.
// The producer only publishes through the atomic `head` field and the payload
// is copied before the consumer publishes `tail`.
unsafe impl Send for SharedInput {}

impl SharedInput {
    pub fn open(name: &str, stop: Arc<AtomicBool>) -> Result<Self> {
        let cname = std::ffi::CString::new(name)
            .with_context(|| format!("invalid shared-memory name {name}"))?;
        #[cfg(target_os = "linux")]
        let flags = libc::O_CREAT | libc::O_RDWR;
        #[cfg(not(target_os = "linux"))]
        let flags = libc::O_RDWR;
        let fd = unsafe { libc::shm_open(cname.as_ptr(), flags, 0o666) };
        if fd < 0 {
            bail!(
                "cannot open {name}: {} (is the Salsa carrier producer installed?)",
                std::io::Error::last_os_error()
            );
        }
        let size = std::mem::size_of::<Shared>();
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error).with_context(|| format!("cannot stat {name}"));
        }
        let stat = unsafe { stat.assume_init() };
        if (stat.st_size as usize) < size
            && unsafe { libc::ftruncate(fd, size as libc::off_t) } != 0
        {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error).with_context(|| format!("cannot size {name}"));
        }
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        unsafe { libc::close(fd) };
        if pointer == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot map {name}"));
        }
        let shared = pointer.cast::<Shared>();
        // Initialize only a fresh segment. A running producer keeps its state.
        let magic = unsafe { (*shared).control.magic.load(Ordering::Acquire) };
        let version = unsafe { (*shared).control.version.load(Ordering::Acquire) };
        if magic != SHM_MAGIC || version != SHM_VERSION {
            unsafe {
                std::ptr::write_bytes(pointer, 0, size);
                (*shared).control.magic.store(SHM_MAGIC, Ordering::Release);
                (*shared)
                    .control
                    .version
                    .store(SHM_VERSION, Ordering::Release);
            }
        }
        Ok(Self {
            shared,
            stop,
            skipped: 0,
            max_slot_age: MAX_SLOT_AGE,
            pending_discontinuity: false,
        })
    }

    fn next_chunk(&mut self) -> Result<Option<CarrierChunk>> {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let control = unsafe { &(*self.shared).control };
            // The shared consumer position is authoritative across sessions.
            // The producer retains both indices when it changes generation.
            let generation = control.generation.load(Ordering::Acquire);
            let tail = control.tail.load(Ordering::Acquire);
            let head = control.head.load(Ordering::Acquire);
            if head < tail {
                // A legacy producer can briefly publish head=0 before tail=0
                // during reset. Wait for its matching consumer-position store.
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            if head - tail > SLOT_COUNT as u64 {
                bail!("invalid carrier ring positions: head={head}, tail={tail}");
            }
            if tail == head {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            let slot = unsafe { &(*self.shared).slots[(tail % SLOT_COUNT as u64) as usize] };
            // The producer publishes the payload before advancing `head` with
            // release ordering; the acquire above makes this copy safe.
            let header = slot.header;
            if control.generation.load(Ordering::Acquire) != generation {
                continue;
            }
            if header.generation != generation {
                self.skipped += 1;
                control.tail.store(tail + 1, Ordering::Release);
                continue;
            }
            if self.slot_is_stale(header.host_time) {
                self.skipped += 1;
                self.pending_discontinuity = true;
                control.tail.store(tail + 1, Ordering::Release);
                continue;
            }
            let payload_bytes = (header.payload_bytes as usize).min(SLOT_BYTES);
            let frame_bytes = header.bytes_per_frame as usize;
            let expected = (header.frames as usize)
                .saturating_mul(frame_bytes)
                .min(SLOT_BYTES);
            if frame_bytes != 0 && expected != payload_bytes {
                self.skipped += 1;
            }
            let bytes = slot.payload[..payload_bytes].to_vec();
            control.tail.store(tail + 1, Ordering::Release);
            if control.generation.load(Ordering::Acquire) != generation {
                self.skipped += 1;
                continue;
            }
            let flags = if self.pending_discontinuity {
                self.pending_discontinuity = false;
                header.flags | FLAG_DISCONTINUITY
            } else {
                header.flags
            };
            return Ok(Some(CarrierChunk {
                bytes,
                start_sample: Some(header.start_sample),
                host_time: Some(header.host_time),
                bytes_per_frame: Some(header.bytes_per_frame),
                generation: header.generation,
                flags,
            }));
        }
    }

    fn slot_is_stale(&self, host_time: u64) -> bool {
        if self.max_slot_age == Duration::MAX {
            return false;
        }
        let now = host_now_ns();
        if now == 0 {
            return false;
        }
        if host_time == 0 {
            return true;
        }
        let host = host_ticks_to_ns(host_time);
        let age = now.saturating_sub(host);
        let limit = self.max_slot_age.as_nanos().min(u128::from(u64::MAX)) as u64;
        age > limit
    }

    pub fn skipped_slots(&self) -> u64 {
        self.skipped
    }

    /// Publish a physical-output clock mapping for the carrier producer.
    pub fn publish_clock(&self, sample: u64, host: u64, seed: u64) {
        let control = unsafe { &(*self.shared).control };
        control.clock_sequence.fetch_add(1, Ordering::AcqRel);
        control.clock_sample.store(sample, Ordering::Relaxed);
        control.clock_host.store(host, Ordering::Relaxed);
        control.clock_seed.store(seed, Ordering::Relaxed);
        control.clock_valid.store(1, Ordering::Relaxed);
        control.clock_sequence.fetch_add(1, Ordering::AcqRel);
    }

    pub fn declare_latency(&self, frames: u64) {
        let control = unsafe { &(*self.shared).control };
        control
            .declared_latency_frames
            .store(frames, Ordering::Release);
        control.latency_valid.store(1, Ordering::Release);
    }

    pub fn head_position(&self) -> (u64, u64) {
        let control = unsafe { &(*self.shared).control };
        (
            control.head.load(Ordering::Acquire),
            control.tail.load(Ordering::Acquire),
        )
    }

    /// Carrier frames dropped by the producer because the ring was full.
    pub fn dropped_frames(&self) -> u64 {
        unsafe {
            (*self.shared)
                .control
                .dropped_frames
                .load(Ordering::Acquire)
        }
    }
}

impl CarrierSource for SharedInput {
    fn next_chunk(&mut self) -> Result<Option<CarrierChunk>> {
        SharedInput::next_chunk(self)
    }

    fn is_live(&self) -> bool {
        true
    }
}

impl Drop for SharedInput {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.shared.cast(), std::mem::size_of::<Shared>());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_input() -> SharedInput {
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                std::mem::size_of::<Shared>(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        assert_ne!(pointer, libc::MAP_FAILED);
        SharedInput {
            shared: pointer.cast(),
            stop: Arc::default(),
            skipped: 0,
            max_slot_age: Duration::MAX,
            pending_discontinuity: false,
        }
    }

    #[test]
    fn full_new_session_is_read_from_the_shared_consumer_position() {
        let mut input = memory_input();
        unsafe {
            let shared = &mut *input.shared;
            shared.control.head.store(40, Ordering::Release);
            shared.control.tail.store(39, Ordering::Release);
            shared.slots[39 % SLOT_COUNT].header.generation = 0;
        }
        input.next_chunk().unwrap().unwrap();
        // Reproduce a reset by an older producer before the consumer wakes.
        unsafe {
            let shared = &mut *input.shared;
            shared.control.generation.store(1, Ordering::Release);
            shared.control.tail.store(0, Ordering::Release);
            for (index, slot) in shared.slots.iter_mut().enumerate() {
                slot.header.generation = 1;
                slot.header.start_sample = index as u64;
            }
            shared
                .control
                .head
                .store(SLOT_COUNT as u64, Ordering::Release);
        }
        for expected in 0..SLOT_COUNT {
            let chunk = input.next_chunk().unwrap().unwrap();
            assert_eq!(chunk.start_sample, Some(expected as u64));
            assert_eq!(input.head_position().1, expected as u64 + 1);
        }
        assert_eq!(input.head_position(), (16, 16));
    }

    #[test]
    fn generation_change_discards_old_slots_without_resetting_indices() {
        let mut input = memory_input();
        unsafe {
            let shared = &mut *input.shared;
            shared.control.generation.store(2, Ordering::Release);
            shared.control.tail.store(40, Ordering::Release);
            shared.control.head.store(42, Ordering::Release);
            shared.slots[40 % SLOT_COUNT].header.generation = 1;
            shared.slots[41 % SLOT_COUNT].header.generation = 2;
        }
        assert_eq!(input.next_chunk().unwrap().unwrap().generation, 2);
        assert_eq!(input.head_position(), (42, 42));
        assert_eq!(input.skipped_slots(), 1);
    }

    #[test]
    fn file_source_returns_eof() {
        let path = std::env::temp_dir().join("salsa-audiod-carrier-test.bin");
        std::fs::write(&path, [0u8; 10]).unwrap();
        let mut input = CarrierInput::open_file(&path).unwrap();
        let chunk = input.next_chunk().unwrap().unwrap();
        assert_eq!(chunk.bytes.len(), 10);
        assert!(input.next_chunk().unwrap().is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn shared_struct_matches_c_header_constants() {
        assert_eq!(std::mem::size_of::<SlotHeader>(), 48);
        assert_eq!(SHM_MAGIC, 0x5341_4c53);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dropped_stale_slot_marks_the_next_chunk_as_discontinuous() {
        use crate::clock::host_now_ticks;

        let mut input = memory_input();
        input.max_slot_age = MAX_SLOT_AGE;
        unsafe {
            let shared = &mut *input.shared;
            shared.control.generation.store(3, Ordering::Release);
            let stale = &mut shared.slots[0];
            stale.header = SlotHeader {
                generation: 3,
                start_sample: 100,
                host_time: 0,
                frames: 1,
                channels: 2,
                bytes_per_frame: 4,
                payload_bytes: 4,
                flags: 0,
                reserved: 0,
            };
            stale.payload[..4].copy_from_slice(&[0x11; 4]);
            let valid = &mut shared.slots[1];
            valid.header = SlotHeader {
                generation: 3,
                start_sample: 200,
                host_time: host_now_ticks(),
                frames: 1,
                channels: 2,
                bytes_per_frame: 4,
                payload_bytes: 4,
                flags: 0,
                reserved: 0,
            };
            valid.payload[..4].copy_from_slice(&[0x22; 4]);
            shared.control.head.store(2, Ordering::Release);
        }

        let chunk = input.next_chunk().unwrap().unwrap();
        assert_eq!(chunk.start_sample, Some(200));
        assert_ne!(chunk.flags & FLAG_DISCONTINUITY, 0);
        assert_eq!(input.skipped_slots(), 1);
    }

    #[test]
    fn shared_input_copies_published_slots() {
        let mut input = memory_input();
        unsafe {
            let shared = &mut *input.shared;
            shared.control.magic.store(SHM_MAGIC, Ordering::Release);
            shared.control.version.store(SHM_VERSION, Ordering::Release);
            shared.control.generation.store(7, Ordering::Release);
            let slot = &mut shared.slots[0];
            slot.header = SlotHeader {
                generation: 7,
                start_sample: 123_456,
                host_time: 999,
                frames: 4,
                channels: 2,
                bytes_per_frame: 4,
                payload_bytes: 16,
                flags: 0,
                reserved: 0,
            };
            slot.payload[..16].copy_from_slice(&[0x11; 16]);
            shared.control.head.store(1, Ordering::Release);
        }

        let chunk = input.next_chunk().unwrap().unwrap();
        assert_eq!(chunk.bytes, vec![0x11; 16]);
        assert_eq!(chunk.start_sample, Some(123_456));
        assert_eq!(chunk.host_time, Some(999));
        assert_eq!(chunk.generation, 7);
        assert_eq!(chunk.bytes_per_frame, Some(4));
        assert_eq!(input.head_position(), (1, 1));
    }
}
