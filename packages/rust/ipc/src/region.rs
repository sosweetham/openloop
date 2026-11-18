//! The shared memory of one session: its versioned layout, and the mapping both processes hold
//! (docs/ARCHITECTURE.md § Runtime ↔ openloopd).

use std::error::Error;
use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::mem::{offset_of, size_of};
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use crate::IPC_VERSION;
use crate::access_unit::AccessUnitRing;
use crate::doorbell::Doorbell;
use crate::pod::{Line, Pod};
use crate::records::{
    AccessUnit, ClockMapping, Event, FrameTiming, HeadsetInfo, SessionState, TrackingSample,
};
use crate::ring::SpscRing;
use crate::snapshot::Snapshots;
use crate::xpc::{self, XpcObject};

const MAGIC: u64 = u64::from_le_bytes(*b"TTHRSESS");

pub const AU_SLOTS: usize = 256;
pub const AU_SLOT_BYTES: usize = 32 * 1024;
pub type AccessUnits = AccessUnitRing<AU_SLOTS, AU_SLOT_BYTES>;
pub type Events = SpscRing<Event, 64>;

/// Written by `openloopd` before the region is shared, and never again.
#[repr(C)]
struct Header {
    magic: AtomicU64,
    version: AtomicU64,
    size: AtomicU64,
    fingerprint: AtomicU64,
    session: AtomicU64,
    boot_id: AtomicU64,
    created_mac_ns: AtomicI64,
}

impl Header {
    fn stamp(&self, session: u64, boot_id: u64, now_mac_ns: i64) {
        self.version
            .store(u64::from(IPC_VERSION), Ordering::Relaxed);
        self.size
            .store(size_of::<SessionLayout>() as u64, Ordering::Relaxed);
        self.fingerprint
            .store(LAYOUT_FINGERPRINT, Ordering::Relaxed);
        self.session.store(session, Ordering::Relaxed);
        self.boot_id.store(boot_id, Ordering::Relaxed);
        self.created_mac_ns.store(now_mac_ns, Ordering::Relaxed);
        self.magic.store(MAGIC, Ordering::Release);
    }
}

/// Every field has one writing process, named on it. Records hold the latest value of
/// something; rings carry each event once; a doorbell wakes the side that reads what it
/// guards.
#[repr(C)]
pub struct SessionLayout {
    header: Line<Header>,
    /// `openloopd`, every 100 ms: a stale beat with an interrupted connection means it is gone.
    heartbeat_mac_ns: Line<AtomicI64>,
    /// `openloopd`.
    pub headset: Snapshots<HeadsetInfo, 4>,
    /// `openloopd`. The slots double as the last 16 samples.
    pub tracking: Snapshots<TrackingSample, 16>,
    /// `openloopd`.
    pub clock: Snapshots<ClockMapping, 4>,
    /// `openloopd` (R2 S4).
    pub timing: Snapshots<FrameTiming, 4>,
    /// `openloopd`.
    pub state: Snapshots<SessionState, 4>,
    /// `openloopd` produces, the app consumes.
    pub to_app: Events,
    /// `openloopd` rings it after publishing timing or pushing to `to_app`.
    pub app_bell: Doorbell,
    /// The app produces, `openloopd` consumes.
    pub to_openloopd: Events,
    /// The app rings it after pushing to `to_openloopd` or `access_units`.
    pub openloopd_bell: Doorbell,
    /// The app produces, `openloopd` consumes (R2 S1 and S2).
    pub access_units: AccessUnits,
}

/// Changes whenever a record's size or the layout's shape changes, so two builds that disagree
/// refuse each other even if `IPC_VERSION` was not bumped. It cannot see a field change meaning
/// at the same size; that takes the version.
pub const LAYOUT_FINGERPRINT: u64 = fnv1a(&[
    size_of::<SessionLayout>() as u64,
    offset_of!(SessionLayout, heartbeat_mac_ns) as u64,
    offset_of!(SessionLayout, headset) as u64,
    offset_of!(SessionLayout, tracking) as u64,
    offset_of!(SessionLayout, clock) as u64,
    offset_of!(SessionLayout, timing) as u64,
    offset_of!(SessionLayout, state) as u64,
    offset_of!(SessionLayout, to_app) as u64,
    offset_of!(SessionLayout, app_bell) as u64,
    offset_of!(SessionLayout, to_openloopd) as u64,
    offset_of!(SessionLayout, openloopd_bell) as u64,
    offset_of!(SessionLayout, access_units) as u64,
    size_of::<HeadsetInfo>() as u64,
    size_of::<TrackingSample>() as u64,
    size_of::<ClockMapping>() as u64,
    size_of::<FrameTiming>() as u64,
    size_of::<SessionState>() as u64,
    size_of::<Event>() as u64,
    size_of::<AccessUnit>() as u64,
    AU_SLOTS as u64,
    AU_SLOT_BYTES as u64,
]);

const fn fnv1a(words: &[u64]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut index = 0;
    while index < words.len() {
        let bytes = words[index].to_le_bytes();
        let mut byte = 0;
        while byte < 8 {
            hash = (hash ^ bytes[byte] as u64).wrapping_mul(0x0000_0100_0000_01b3);
            byte += 1;
        }
        index += 1;
    }
    hash
}

impl SessionLayout {
    pub fn beat(&self, now_mac_ns: i64) {
        self.heartbeat_mac_ns.0.store(now_mac_ns, Ordering::Relaxed);
    }

    pub fn heartbeat_mac_ns(&self) -> i64 {
        self.heartbeat_mac_ns.0.load(Ordering::Relaxed)
    }
}

/// What `openloopd` stamped into a region's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionInfo {
    pub session: u64,
    /// Distinguishes `openloopd` launches, so a runtime can tell a restarted `openloopd` apart.
    pub boot_id: u64,
    pub created_mac_ns: i64,
}

#[derive(Debug)]
pub enum RegionError {
    NotShmem,
    MapFailed,
    TooSmall { len: usize, needed: usize },
    NotARegion,
    Version { found: u64, expected: u64 },
    Layout { size: u64, fingerprint: u64 },
    Io(io::Error),
}

impl fmt::Display for RegionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotShmem => write!(f, "the object is not an xpc_shmem region"),
            Self::MapFailed => write!(f, "xpc_shmem_map failed"),
            Self::TooSmall { len, needed } => {
                write!(f, "the region has {len} bytes, the layout needs {needed}")
            }
            Self::NotARegion => write!(f, "the region has no session header"),
            Self::Version { found, expected } => {
                write!(
                    f,
                    "the region is IPC version {found}, this build speaks {expected}"
                )
            }
            Self::Layout { size, fingerprint } => write!(
                f,
                "the region's layout ({size} bytes, fingerprint {fingerprint:#x}) is not this \
                 build's ({} bytes, {LAYOUT_FINGERPRINT:#x})",
                size_of::<SessionLayout>()
            ),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl Error for RegionError {}

/// One session's region, mapped into this process.
pub struct SessionRegion {
    memory: SharedMemory,
}

impl SessionRegion {
    /// `openloopd`: a fresh region for a new session.
    pub fn create(session: u64, boot_id: u64, now_mac_ns: i64) -> io::Result<SessionRegion> {
        let region = SessionRegion {
            memory: SharedMemory::new(size_of::<SessionLayout>())?,
        };
        region.layout().header.0.stamp(session, boot_id, now_mac_ns);
        Ok(region)
    }

    /// The runtime: maps the region `openloopd` sent, and refuses one this build cannot read.
    pub fn map(object: &XpcObject) -> Result<SessionRegion, RegionError> {
        let memory = SharedMemory::map(object)?;
        let needed = size_of::<SessionLayout>();
        if memory.len() < size_of::<Header>() {
            return Err(RegionError::TooSmall {
                len: memory.len(),
                needed,
            });
        }
        // SAFETY: the mapping is page-aligned and holds at least a header, all atomics.
        let header = unsafe { &*memory.as_ptr().cast::<Header>() };
        if header.magic.load(Ordering::Acquire) != MAGIC {
            return Err(RegionError::NotARegion);
        }
        let version = header.version.load(Ordering::Relaxed);
        if version != u64::from(IPC_VERSION) {
            return Err(RegionError::Version {
                found: version,
                expected: u64::from(IPC_VERSION),
            });
        }
        let size = header.size.load(Ordering::Relaxed);
        let fingerprint = header.fingerprint.load(Ordering::Relaxed);
        if size != needed as u64 || fingerprint != LAYOUT_FINGERPRINT {
            return Err(RegionError::Layout { size, fingerprint });
        }
        if memory.len() < needed {
            return Err(RegionError::TooSmall {
                len: memory.len(),
                needed,
            });
        }
        Ok(SessionRegion { memory })
    }

    /// The object to send over the connection; every copy maps the same memory.
    pub fn to_xpc(&self) -> io::Result<XpcObject> {
        self.memory.to_xpc()
    }

    pub fn layout(&self) -> &SessionLayout {
        // SAFETY: the mapping is page-aligned, at least as large as the layout (checked on
        // map, sized on create) and lives as long as `self`. Every field is an atomic or a
        // cell accessed through atomics, valid for any bytes the peer may write.
        unsafe { &*self.memory.as_ptr().cast::<SessionLayout>() }
    }

    pub fn info(&self) -> RegionInfo {
        let header = &self.layout().header.0;
        RegionInfo {
            session: header.session.load(Ordering::Relaxed),
            boot_id: header.boot_id.load(Ordering::Relaxed),
            created_mac_ns: header.created_mac_ns.load(Ordering::Relaxed),
        }
    }
}

impl Deref for SessionRegion {
    type Target = SessionLayout;

    fn deref(&self) -> &SessionLayout {
        self.layout()
    }
}

/// Anonymous `MAP_SHARED` pages, the only memory `xpc_shmem_create` may box (`<xpc/xpc.h>`),
/// unmapped on drop.
pub struct SharedMemory {
    base: NonNull<u8>,
    len: usize,
}

// SAFETY: the pages are only reached through `&` and the atomics laid over them.
unsafe impl Send for SharedMemory {}
// SAFETY: as above.
unsafe impl Sync for SharedMemory {}

impl SharedMemory {
    /// Zeroed by the kernel, rounded up to whole pages.
    pub fn new(len: usize) -> io::Result<SharedMemory> {
        // SAFETY: no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let len = len.max(1).div_ceil(page) * page;
        // SAFETY: a fresh anonymous mapping; the result is checked.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(SharedMemory {
            base: NonNull::new(base.cast()).expect("mmap returned null"),
            len,
        })
    }

    pub fn map(object: &XpcObject) -> Result<SharedMemory, RegionError> {
        if !object.is_shmem() {
            return Err(RegionError::NotShmem);
        }
        let mut base = std::ptr::null_mut();
        // SAFETY: a valid shmem object and an out pointer.
        let len = unsafe { xpc::xpc_shmem_map(object.as_ptr(), &mut base) };
        match NonNull::new(base.cast::<u8>()) {
            Some(base) if len != 0 => Ok(SharedMemory { base, len }),
            _ => Err(RegionError::MapFailed),
        }
    }

    pub fn to_xpc(&self) -> io::Result<XpcObject> {
        // SAFETY: the pages came from mmap(MAP_SHARED) and stay mapped while `self` lives; the
        // object holds its own reference to the memory once created.
        let object = unsafe { xpc::xpc_shmem_create(self.base.as_ptr().cast(), self.len) };
        // SAFETY: xpc_shmem_create returns a +1 reference or null.
        unsafe { XpcObject::from_owned(object) }
            .ok_or_else(|| io::Error::other("xpc_shmem_create failed"))
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        // SAFETY: this mapping is ours, and nothing borrowed from it outlives `self`.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.len) };
    }
}

/// Types whose all-zero bytes are a valid, empty value: what fresh shared pages hold.
///
/// # Safety
/// Every field is an integer, an atomic, or a [`Pod`] cell.
pub unsafe trait Zeroable {}

// SAFETY: all of these are atomics and Pod cells.
unsafe impl Zeroable for SessionLayout {}
// SAFETY: as above.
unsafe impl<T: Pod, const N: usize> Zeroable for Snapshots<T, N> {}
// SAFETY: as above.
unsafe impl<T: Pod, const N: usize> Zeroable for SpscRing<T, N> {}
// SAFETY: as above.
unsafe impl<const S: usize, const B: usize> Zeroable for AccessUnitRing<S, B> {}
// SAFETY: as above.
unsafe impl Zeroable for Doorbell {}
// SAFETY: an array of zeroable values.
unsafe impl<T: Zeroable, const N: usize> Zeroable for [T; N] {}

/// A zeroed `T` on its own shared pages, for using the region's primitives inside one process.
pub struct Zeroed<T> {
    memory: SharedMemory,
    _type: PhantomData<T>,
}

// SAFETY: the value is only reached through `&T`.
unsafe impl<T: Sync> Send for Zeroed<T> {}
// SAFETY: as above.
unsafe impl<T: Sync> Sync for Zeroed<T> {}

impl<T: Zeroable> Zeroed<T> {
    pub fn new() -> Zeroed<T> {
        let memory = SharedMemory::new(size_of::<T>()).expect("mapping shared pages");
        Zeroed {
            memory,
            _type: PhantomData,
        }
    }
}

impl<T: Zeroable> Default for Zeroed<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Deref for Zeroed<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: page-aligned, zeroed, sized for T, and T: Zeroable (checked in `new`).
        unsafe { &*self.memory.as_ptr().cast::<T>() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doorbell::now_ns;
    use crate::records::{Message, Pose};

    fn mapped_twice() -> (SessionRegion, SessionRegion) {
        let created = SessionRegion::create(7, 99, now_ns()).unwrap();
        let mapped = SessionRegion::map(&created.to_xpc().unwrap()).unwrap();
        (created, mapped)
    }

    #[test]
    fn the_layout_fits_the_hardware() {
        assert_eq!(std::mem::align_of::<SessionLayout>(), 128);
        let size = size_of::<SessionLayout>();
        eprintln!("SessionLayout: {size} bytes, fingerprint {LAYOUT_FINGERPRINT:#x}");
        assert!(size < 9 << 20, "{size} bytes");
    }

    #[test]
    fn a_second_mapping_sees_the_same_memory() {
        let (created, mapped) = mapped_twice();
        assert_eq!(mapped.info(), created.info());
        assert_eq!(mapped.info().session, 7);
        let mut publisher = created.tracking.publisher();
        let sample = TrackingSample {
            sample_mac_ns: 5,
            head: Pose::IDENTITY,
            ..Default::default()
        };
        created.tracking.publish(&mut publisher, &sample);
        assert_eq!(mapped.tracking.latest().unwrap().value, sample);
        let mut producer = mapped.to_openloopd.producer();
        mapped
            .to_openloopd
            .push(&mut producer, &Message::KeyframeRequest.into())
            .unwrap();
        let event = created
            .to_openloopd
            .pop(&mut created.to_openloopd.consumer())
            .unwrap();
        assert_eq!(event.unwrap().decode(), Ok(Message::KeyframeRequest));
    }

    #[test]
    fn refuses_a_region_it_cannot_read() {
        let (created, _) = mapped_twice();
        let header = &created.layout().header.0;
        let refused = |expect: fn(&RegionError) -> bool| {
            let error = SessionRegion::map(&created.to_xpc().unwrap())
                .err()
                .expect("mapped");
            assert!(expect(&error), "{error}");
        };

        header
            .version
            .store(u64::from(IPC_VERSION) + 1, Ordering::Relaxed);
        refused(|e| matches!(e, RegionError::Version { .. }));
        header
            .version
            .store(u64::from(IPC_VERSION), Ordering::Relaxed);

        header
            .fingerprint
            .store(LAYOUT_FINGERPRINT ^ 1, Ordering::Relaxed);
        refused(|e| matches!(e, RegionError::Layout { .. }));
        header
            .fingerprint
            .store(LAYOUT_FINGERPRINT, Ordering::Relaxed);

        header.size.store(64, Ordering::Relaxed);
        refused(|e| matches!(e, RegionError::Layout { .. }));
        header
            .size
            .store(size_of::<SessionLayout>() as u64, Ordering::Relaxed);

        header.magic.store(0, Ordering::Relaxed);
        refused(|e| matches!(e, RegionError::NotARegion));
        header.magic.store(MAGIC, Ordering::Relaxed);

        assert!(SessionRegion::map(&created.to_xpc().unwrap()).is_ok());
    }

    #[test]
    fn refuses_memory_too_small_for_the_layout() {
        let small = SharedMemory::new(1).unwrap();
        let error = SessionRegion::map(&small.to_xpc().unwrap()).err().unwrap();
        assert!(matches!(error, RegionError::NotARegion), "{error}");
        // SAFETY: a page holds a header, all atomics.
        unsafe { &*small.as_ptr().cast::<Header>() }.stamp(1, 1, 0);
        let error = SessionRegion::map(&small.to_xpc().unwrap()).err().unwrap();
        assert!(matches!(error, RegionError::TooSmall { .. }), "{error}");
    }
}
