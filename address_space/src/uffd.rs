// Copyright (c) 2026 Huawei Technologies Co.,Ltd. All rights reserved.
//
// UFFD (userfaultfd) memory backend for StratoVirt snapshot restore.
//
// Protocol:
//   1. Create a userfaultfd fd via SYS_userfaultfd.
//   2. Enable it with UFFDIO_API.
//   3. For every guest RAM region, call UFFDIO_REGISTER (MISSING mode).
//   4. Connect to the external uffd daemon Unix socket at `socket_path`.
//   5. Send one message via sendmsg:
//      - iov[0]: JSON-encoded Vec<RegionMapping>  (region addresses + sizes)
//      - cmsg:   SCM_RIGHTS carrying the uffd fd
//   6. Keep the handoff socket alive for the backend lifetime.
//   7. The external uffd daemon takes ownership of the uffd and serves page faults
//      from the snapshot block device.

use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use machine_manager::qmp::qmp_schema::MemDirtyBitmap;
use util::bitmap::{bitmap_page_size, set_dense_bitmap_bit};
use util::unix::{host_page_size, PagemapBatchReader, PagemapEntry};
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

// ────────────────────────────────────────────────────────────────────────────
// Linux userfaultfd constants (not yet exposed by libc 0.2 / nix 0.26)
// ────────────────────────────────────────────────────────────────────────────

#[cfg(target_arch = "aarch64")]
const SYS_USERFAULTFD_NR: libc::c_long = 282;
#[cfg(target_arch = "x86_64")]
const SYS_USERFAULTFD_NR: libc::c_long = 323;

const UFFD_API: u64 = 0xAA;
const UFFDIO_API: u64 = 0xc018aa3f;
const UFFDIO_REGISTER: u64 = 0xc020aa00;
const UFFDIO_WRITEPROTECT: u64 = 0xc018aa06;
// Bit in UffdioRegister::ioctls, not the UFFDIO_COPY ioctl request value.
const UFFDIO_COPY_SUPPORTED: u64 = 1 << 3;

const UFFDIO_REGISTER_MODE_MISSING: u64 = 1 << 0;
const UFFDIO_REGISTER_MODE_WP: u64 = 1 << 1;
const UFFDIO_WRITEPROTECT_MODE_WP: u64 = 1 << 0;

const UFFD_FEATURE_MISSING_HUGETLBFS: u64 = 1 << 4;
const UFFD_FEATURE_WP_HUGETLBFS_SHMEM: u64 = 1 << 12;
// UFFD_FEATURE_WP_ASYNC: write-protect faults are cleared by the kernel
// automatically (no UFFD event), which is required for UFFDIO_COPY_MODE_WP.
const UFFD_FEATURE_WP_ASYNC: u64 = 1 << 15;
const UFFD_FEATURE_WP_UNPOPULATED: u64 = 1 << 13;

#[repr(C)]
struct UffdioApi {
    api: u64,
    features: u64,
    ioctls: u64,
}

#[repr(C)]
struct UffdioRange {
    start: u64,
    len: u64,
}

#[repr(C)]
struct UffdioRegister {
    range: UffdioRange,
    mode: u64,
    ioctls: u64,
}

#[repr(C)]
struct UffdioWriteProtect {
    range: UffdioRange,
    mode: u64,
}

static UFFD_BACKEND: OnceLock<Mutex<Option<UffdMemoryBackend>>> = OnceLock::new();

fn backend_state() -> &'static Mutex<Option<UffdMemoryBackend>> {
    UFFD_BACKEND.get_or_init(|| Mutex::new(None))
}

// ────────────────────────────────────────────────────────────────────────────
// Wire format sent to the external uffd daemon
// ────────────────────────────────────────────────────────────────────────────

/// One guest RAM region as serialised and sent to the external uffd daemon's UFFD handler.
#[derive(Debug, PartialEq, serde::Serialize)]
struct RegionMapping {
    base_host_virt_addr: u64,
    size: u64,
    /// Byte offset of this region's data within the snapshot file. For full
    /// snapshots this includes headers and need not be huge-page aligned.
    offset: u64,
    /// Kept as `page_size_kib` for wire-compatibility with Firecracker.
    /// The value is in *bytes*, matching the deprecated Firecracker field.
    page_size_kib: u64,
}

// ────────────────────────────────────────────────────────────────────────────
// Public API
// ────────────────────────────────────────────────────────────────────────────

/// UFFD memory backend: registers guest RAM with a userfaultfd and hands the
/// fd to the external uffd daemon so it can serve page faults from a snapshot source.
pub struct UffdMemoryBackend {
    socket_path: PathBuf,
    regions: Vec<RegionMapping>,
    features: u64,
    uffd_fd: RawFd,
    wp_async: bool,
    handoff_stream: Option<UnixStream>,
    // Start of RAM data in the snapshot source file (zero for external RAM).
    snapfile_offset: u64,
    standalone: bool,
}

impl UffdMemoryBackend {
    /// Create a new backend that will connect to `socket_path` when
    /// [`Self::send_to_external_uffd_daemon`] is called.
    /// `snapfile_offset` is the start of RAM data in the snapshot source file.
    /// Bitmap indices are RAM-relative; daemon offsets remain file-relative.
    pub fn new(socket_path: &str, require_hugetlb: bool, snapfile_offset: u64) -> Result<Self> {
        let (uffd_fd, wp_async, features) =
            create_uffd(require_hugetlb).context("failed to create userfaultfd")?;
        Ok(UffdMemoryBackend {
            socket_path: PathBuf::from(socket_path),
            features,
            regions: Vec::new(),
            uffd_fd,
            wp_async,
            handoff_stream: None,
            snapfile_offset,
            standalone: false,
        })
    }

    /// Register a guest RAM region with the userfaultfd (MISSING fault mode).
    ///
    /// * `host_addr` – host virtual address of the region start.
    /// * `size`      – size in bytes.
    /// * `offset`    – byte offset of this region in the flat snapshot file.
    /// * `page_size` – actual mapping page size in bytes (2 MiB for HugeTLB).
    ///
    /// Before registering, the region is cleared with MADV_DONTNEED.
    /// UFFDIO_REGISTER does not evict pages that are already present: any
    /// page populated before registration (mem-prealloc touching every page,
    /// guest RAM accesses during the device-state restore that runs earlier,
    /// or even a stray read mapping the kernel zero page) would never raise
    /// a MISSING fault, and the restored VM would silently see zero/stale
    /// content instead of snapshot content.  The clear step restores the
    /// invariant that every access faults as MISSING and is served by the
    /// UFFD handler.  On an untouched anonymous mapping the madvise is
    /// essentially free, so it is kept unconditionally as a safety net.
    pub fn register_region(
        &mut self,
        host_addr: u64,
        size: u64,
        offset: u64,
        page_size: u64,
    ) -> Result<()> {
        if !page_size.is_power_of_two()
            || size == 0
            || host_addr & (page_size - 1) != 0
            || size & (page_size - 1) != 0
            || host_addr.checked_add(size).is_none()
            || offset.checked_add(size).is_none()
        {
            bail!("Invalid UFFD region alignment or range");
        }
        if page_size > host_page_size() {
            if self.features & UFFD_FEATURE_MISSING_HUGETLBFS == 0 {
                bail!("UFFD missing faults on HugeTLB are not supported");
            }
            if self.features & UFFD_FEATURE_WP_HUGETLBFS_SHMEM == 0 {
                self.wp_async = false;
            }
        }
        // SAFETY: the caller owns this valid RAM mapping for the duration of the call.
        let ret = unsafe {
            libc::madvise(
                host_addr as *mut libc::c_void,
                size as libc::size_t,
                libc::MADV_DONTNEED,
            )
        };
        if ret != 0 {
            bail!(
                "madvise(MADV_DONTNEED) failed for [{:#x}, +{:#x}): {}",
                host_addr,
                size,
                std::io::Error::last_os_error()
            );
        }

        let mode = if self.wp_async {
            UFFDIO_REGISTER_MODE_MISSING | UFFDIO_REGISTER_MODE_WP
        } else {
            UFFDIO_REGISTER_MODE_MISSING
        };
        let mut reg = UffdioRegister {
            range: UffdioRange {
                start: host_addr,
                len: size,
            },
            mode,
            ioctls: 0,
        };
        // SAFETY: uffd_fd is a valid userfaultfd opened by create_uffd(); `reg` is a
        // fully-initialised UffdioRegister on the stack and its pointer is valid for
        // the duration of the ioctl.
        let ret = unsafe { libc::ioctl(self.uffd_fd, UFFDIO_REGISTER as _, &mut reg as *mut _) };
        if ret != 0 {
            bail!(
                "UFFDIO_REGISTER failed for [{:#x}, +{:#x}): {}",
                host_addr,
                size,
                std::io::Error::last_os_error()
            );
        }
        if reg.ioctls & UFFDIO_COPY_SUPPORTED == 0 {
            bail!("UFFD region does not support UFFDIO_COPY");
        }
        self.regions.push(RegionMapping {
            base_host_virt_addr: host_addr,
            size,
            offset,
            page_size_kib: page_size,
        });
        Ok(())
    }

    /// Connect to the external uffd daemon socket and hand over the uffd fd together
    /// with the region mapping JSON. The handoff socket is kept open for the backend
    /// lifetime so the daemon observes a stable peer; no ready-ACK is read back, since
    /// the userfaultfd kernel semantics already queue page faults until the daemon
    /// starts serving them.
    pub fn send_to_external_uffd_daemon(&mut self) -> Result<()> {
        let json =
            serde_json::to_vec(&self.regions).context("failed to serialise region mappings")?;

        let stream = send_fd_with_json(&self.socket_path, self.uffd_fd, &json)
            .context("failed to send uffd fd to external uffd daemon")?;
        self.handoff_stream = Some(stream);

        Ok(())
    }
}

/// Start strict WP-only tracking before any device restoration can write RAM.
/// Regions use (host address, length, flat RAM offset), in logical RAM order.
/// Repeating initialization for the same VM never resets an existing epoch.
pub fn initialize_indexed_tracking(regions: &[(u64, u64, u64)]) -> Result<()> {
    let mut stored = backend_state().lock().unwrap();
    if let Some(existing) = stored.as_ref() {
        if existing.standalone
            && existing.regions.len() == regions.len()
            && existing
                .regions
                .iter()
                .zip(regions)
                .all(|(a, b)| (a.base_host_virt_addr, a.size, a.offset) == *b)
        {
            return Ok(());
        }
        bail!("dirty tracking already owns different RAM mappings");
    }
    if host_page_size() != 4096 || regions.is_empty() {
        bail!("indexed dirty tracking requires 4096-byte RAM pages");
    }
    // Kernel-originated writers (KVM and vhost) are included; USER_MODE_ONLY
    // would not satisfy this contract and is deliberately not requested.
    // SAFETY: syscall number and flags match the supported Linux architectures.
    let fd = unsafe { libc::syscall(SYS_USERFAULTFD_NR, libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("create indexed WP_ASYNC userfaultfd");
    }
    let mut backend = UffdMemoryBackend {
        socket_path: PathBuf::new(),
        regions: Vec::new(),
        features: 0,
        uffd_fd: fd as RawFd,
        wp_async: true,
        handoff_stream: None,
        snapfile_offset: 0,
        standalone: true,
    };
    let required = UFFD_FEATURE_WP_ASYNC | UFFD_FEATURE_WP_UNPOPULATED;
    backend.features = enable_uffd_api(backend.uffd_fd, required)
        .context("indexed RAM requires WP_ASYNC and WP_UNPOPULATED")?;
    if backend.features & required != required {
        bail!("kernel lacks mandatory indexed RAM write-protection features");
    }
    let mut next = 0;
    for &(address, size, offset) in regions {
        if address == 0
            || address % 4096 != 0
            || size == 0
            || size % 4096 != 0
            || offset != next
            || address.checked_add(size).is_none()
        {
            bail!("invalid indexed RAM tracking range");
        }
        next = next
            .checked_add(size)
            .context("indexed tracking size overflow")?;
        let mut registration = UffdioRegister {
            range: UffdioRange {
                start: address,
                len: size,
            },
            mode: UFFDIO_REGISTER_MODE_WP,
            ioctls: 0,
        };
        // SAFETY: the caller retains these validated mappings for the VM lifetime.
        let ret = unsafe {
            libc::ioctl(
                backend.uffd_fd,
                UFFDIO_REGISTER as _,
                &mut registration as *mut _,
            )
        };
        if ret != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "register indexed file mapping [{address:#x}, +{size:#x}) for WP-only tracking"
                )
            });
        }
        if registration.ioctls & (1 << 6) == 0 {
            bail!("indexed RAM mapping does not support UFFDIO_WRITEPROTECT");
        }
        backend.regions.push(RegionMapping {
            base_host_virt_addr: address,
            size,
            offset,
            page_size_kib: 4096,
        });
    }
    write_protect_mappings(&backend, true).context("protect initial indexed RAM epoch")?;
    *stored = Some(backend);
    Ok(())
}

/// Keep the UFFD fd alive in StratoVirt and make dirty bitmap queries available.
pub fn store_uffd_backend(backend: UffdMemoryBackend) {
    *backend_state().lock().unwrap() = Some(backend);
}

fn build_bitmap_from_pagemap<F>(
    backend: &UffdMemoryBackend,
    mut is_dirty: F,
) -> Result<MemDirtyBitmap>
where
    F: FnMut(PagemapEntry, bool) -> bool,
{
    let base_page_size = host_page_size();
    let page_size = bitmap_page_size(
        backend.regions.iter().map(|region| region.page_size_kib),
        base_page_size,
    )?;
    let mut pagemap = PagemapBatchReader::new()?;
    let mut bitmap = Vec::new();
    let entries_per_page = page_size / base_page_size;

    for &RegionMapping {
        base_host_virt_addr,
        size,
        offset,
        page_size_kib: region_page_size,
    } in &backend.regions
    {
        let ram_offset = offset
            .checked_sub(backend.snapfile_offset)
            .context("UFFD region precedes snapshot RAM data")?;
        if region_page_size < page_size
            || region_page_size % page_size != 0
            || ram_offset % page_size != 0
        {
            bail!(
                "UFFD mapping is not aligned to bitmap page size {} at offset {}",
                page_size,
                ram_offset
            );
        }
        let mut resident = vec![0_u8; usize::try_from(size / base_page_size)?];
        // SAFETY: the backend owns this page-aligned RAM mapping, and the
        // vector has one byte for each host page in the range.
        let ret = unsafe {
            libc::mincore(
                base_host_virt_addr as *mut libc::c_void,
                usize::try_from(size)?,
                resident.as_mut_ptr(),
            )
        };
        if ret != 0 {
            bail!("UFFD mincore failed: {}", std::io::Error::last_os_error());
        }
        let page_base = ram_offset / page_size;
        pagemap.scan_range(base_host_virt_addr, size, base_page_size, |index, entry| {
            if index % entries_per_page == 0 && is_dirty(entry, resident[index as usize] & 1 != 0) {
                let bitmap_index = page_base
                    .checked_add(index / entries_per_page)
                    .context("UFFD bitmap index overflow")?;
                set_dense_bitmap_bit(&mut bitmap, bitmap_index)?;
            }
            Ok(())
        })?;
    }

    Ok(MemDirtyBitmap { bitmap, page_size })
}

fn build_dirty_bitmap(backend: &UffdMemoryBackend) -> Result<MemDirtyBitmap> {
    build_bitmap_from_pagemap(backend, |entry, resident| {
        // HugeTLB migration entries can have neither pagemap present nor swapped
        // set. Treat inconsistent residency as dirty, even if the WP bit is set.
        entry.is_uffd_dirty() || (resident && !entry.is_resident())
    })
}

/// Fallback dirty bitmap when UFFD WP_ASYNC is unavailable: report the
/// current resident set (mincore or pagemap present/swapped), including
/// migration entries that pagemap alone can miss.
fn build_resident_bitmap(backend: &UffdMemoryBackend) -> Result<MemDirtyBitmap> {
    build_bitmap_from_pagemap(backend, |entry, resident| resident || entry.is_resident())
}

fn write_protect_mappings(backend: &UffdMemoryBackend, enabled: bool) -> Result<()> {
    let mode = if enabled {
        UFFDIO_WRITEPROTECT_MODE_WP
    } else {
        0
    };

    for &RegionMapping {
        base_host_virt_addr,
        size,
        ..
    } in &backend.regions
    {
        if size == 0 {
            continue;
        }

        let mut wp = UffdioWriteProtect {
            range: UffdioRange {
                start: base_host_virt_addr,
                len: size,
            },
            mode,
        };
        // SAFETY: backend.uffd_fd is an open userfaultfd and `wp` points to a
        // valid UFFDIO_WRITEPROTECT payload for the duration of the ioctl.
        let ret = unsafe {
            libc::ioctl(
                backend.uffd_fd,
                UFFDIO_WRITEPROTECT as _,
                &mut wp as *mut UffdioWriteProtect,
            )
        };
        if ret < 0 {
            bail!(
                "Failed to {} UFFD-WP range at {:#x}: {}",
                if enabled { "enable" } else { "disable" },
                base_host_virt_addr,
                std::io::Error::last_os_error()
            );
        }
    }
    Ok(())
}

/// Return dirty pages since the previous query, then reset dirty tracking.
///
/// Tracking mode is tiered by kernel support:
///   1. UFFD WP_ASYNC: write-protect all RAM, reads pagemap bit 57.
///   2. Fallback: report the current resident set (mincore or pagemap bits 63/62)
///      every query.
fn query_and_reset_dirty_bitmap_impl(backend: &mut UffdMemoryBackend) -> Result<MemDirtyBitmap> {
    if backend.wp_async {
        let mut bitmap = build_dirty_bitmap(backend)?;
        if backend.standalone {
            let bytes = backend
                .regions
                .iter()
                .try_fold(0u64, |end, r| {
                    r.offset.checked_add(r.size).map(|n| end.max(n))
                })
                .context("dirty bitmap size overflow")?;
            bitmap
                .bitmap
                .resize(usize::try_from((bytes / 4096).div_ceil(64))?, 0);
        }
        write_protect_mappings(backend, true)
            .with_context(|| "Failed to reset UFFD-WP dirty tracking")?;
        return Ok(bitmap);
    }

    // Without WP_ASYNC there is no write-protect tracking, so every query
    // returns the current resident set (never incremental).
    build_resident_bitmap(backend)
}

/// Return dirty pages since the previous query, then reset dirty tracking.
pub fn query_and_reset_dirty_bitmap() -> Result<MemDirtyBitmap> {
    let mut guard = backend_state().lock().unwrap();
    let backend = guard
        .as_mut()
        .with_context(|| "UFFD dirty tracking is not enabled")?;
    query_and_reset_dirty_bitmap_impl(backend)
}

impl Drop for UffdMemoryBackend {
    fn drop(&mut self) {
        if self.uffd_fd >= 0 {
            // SAFETY: uffd_fd >= 0 guarantees the fd is valid; Drop runs exactly once,
            // so this is the sole close for this fd.
            unsafe { libc::close(self.uffd_fd) };
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Helper: create userfaultfd
// ────────────────────────────────────────────────────────────────────────────

fn create_uffd(require_hugetlb: bool) -> Result<(RawFd, bool, u64)> {
    // O_CLOEXEC = 0x80000 on both x86_64 and aarch64
    // SAFETY: SYS_USERFAULTFD_NR is the correct syscall number for this architecture;
    // O_CLOEXEC | O_NONBLOCK are valid flags accepted by userfaultfd(2).
    let fd = unsafe { libc::syscall(SYS_USERFAULTFD_NR, libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if fd < 0 {
        bail!(
            "SYS_userfaultfd failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let fd = fd as RawFd;

    let mut required = 0;
    if require_hugetlb {
        required |= UFFD_FEATURE_MISSING_HUGETLBFS;
    }

    let mut preferred = required | UFFD_FEATURE_WP_ASYNC;
    if require_hugetlb {
        preferred |= UFFD_FEATURE_WP_HUGETLBFS_SHMEM;
    }

    match enable_uffd_api(fd, preferred) {
        Ok(features) => Ok((fd, true, features)),
        Err(wp_err) => match enable_uffd_api(fd, required) {
            Ok(features) => Ok((fd, false, features)),
            Err(err) => {
                // SAFETY: fd was successfully opened by the syscall above; closing it on
                // the error path prevents a file-descriptor leak.
                unsafe { libc::close(fd) };
                bail!(
                    "UFFDIO_API failed: {}; fallback without WP_ASYNC failed: {}",
                    wp_err,
                    err
                );
            }
        },
    }
}

fn enable_uffd_api(fd: RawFd, features: u64) -> Result<u64> {
    let mut api = UffdioApi {
        api: UFFD_API,
        features,
        ioctls: 0,
    };
    // SAFETY: fd is a valid userfaultfd just opened above; `api` is a valid UffdioApi
    // struct on the stack and its pointer is live for the duration of the ioctl.
    let ret = unsafe { libc::ioctl(fd, UFFDIO_API as _, &mut api as *mut _) };
    if ret != 0 {
        bail!("{}", std::io::Error::last_os_error());
    }
    Ok(api.features)
}

// ────────────────────────────────────────────────────────────────────────────
// Helper: send fd + JSON over a Unix socket (SCM_RIGHTS)
// ────────────────────────────────────────────────────────────────────────────

/// Connect to `socket_path`, send `fd` as SCM_RIGHTS ancillary data together
/// with `json` as the message payload, and return the open `UnixStream` so the
/// caller can keep the handoff socket alive for the backend lifetime.
fn send_fd_with_json(socket_path: &PathBuf, fd: RawFd, json: &[u8]) -> Result<UnixStream> {
    let stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connect to UFFD socket {:?}", socket_path))?;
    stream
        .send_with_fd(json, fd)
        .with_context(|| "sendmsg to UFFD socket failed")?;
    Ok(stream)
}

// ────────────────────────────────────────────────────────────────────────────
// Unit tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixListener;
    use std::thread;

    /// Create a `UffdMemoryBackend` pointing at `socket_path`.
    /// Returns `None` when `userfaultfd` is not available in this environment
    /// (e.g. a restricted container), so callers can skip the test gracefully.
    fn try_new_uffd(socket_path: &str) -> Option<UffdMemoryBackend> {
        UffdMemoryBackend::new(socket_path, false, 0).ok()
    }

    /// Spawn a mock daemon thread that accepts one connection and drains the
    /// incoming sendmsg payload (the ancillary SCM_RIGHTS fd is silently
    /// discarded). It sends no ACK, mirroring the Firecracker-compatible
    /// handoff where the sender does not wait for a ready byte.
    fn spawn_mock_daemon_no_ack(listener: UnixListener) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("mock daemon: accept");
            let mut buf = [0u8; 65536];
            // Drain the sendmsg payload; ancillary SCM_RIGHTS fd is silently discarded.
            // SAFETY: buf is a valid mutable slice on the stack; stream.as_raw_fd()
            // returns a valid connected socket fd for the duration of this call.
            unsafe {
                libc::recv(
                    stream.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    0,
                )
            };
        })
    }

    #[test]
    fn test_send_to_external_uffd_daemon_keeps_handoff_socket() {
        let socket_path = "/tmp/sv_test_uffd_handoff.sock";
        let _ = std::fs::remove_file(socket_path);
        let Some(mut uffd) = try_new_uffd(socket_path) else {
            eprintln!("skip: userfaultfd not available");
            let _ = std::fs::remove_file(socket_path);
            return;
        };
        let listener = UnixListener::bind(socket_path).expect("bind");

        let handle = spawn_mock_daemon_no_ack(listener);

        assert!(
            uffd.send_to_external_uffd_daemon().is_ok(),
            "should succeed without a daemon ACK"
        );
        assert!(uffd.handoff_stream.is_some());

        handle.join().expect("daemon thread");
        let _ = std::fs::remove_file(socket_path);
    }

    #[test]
    fn test_send_to_external_uffd_daemon_socket_missing() {
        // Socket path does not exist → connect fails immediately.
        match try_new_uffd("/tmp/sv_test_uffd_nonexistent.sock") {
            Some(mut uffd) => {
                assert!(
                    uffd.send_to_external_uffd_daemon().is_err(),
                    "should fail when socket path does not exist"
                );
            }
            None => eprintln!("skip: userfaultfd not available"),
        }
    }

    #[test]
    fn test_region_mapping_serializes_base_host_virt_addr() {
        let mapping = RegionMapping {
            base_host_virt_addr: 0x1000,
            size: 0x2000,
            offset: 0,
            page_size_kib: 4096,
        };

        let json = serde_json::to_value(mapping).unwrap();
        assert_eq!(json["base_host_virt_addr"], 0x1000);
    }

    #[test]
    fn test_query_dirty_bitmap_fallback_reports_resident_pages() {
        // Without WP_ASYNC, the fallback reports the current resident set:
        // only pages present (or swapped) are marked dirty. Writing a page
        // makes it resident, so a write is always reported, and the report is
        // a conservative superset of pages written since the last query.
        let page_size = host_page_size();
        let size = page_size as usize * 4;
        // SAFETY: anonymous RW mapping of `size` bytes; `mmap` returns either a
        // page-aligned pointer or MAP_FAILED, both valid to test against.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            eprintln!("skip: mmap failed");
            return;
        }

        let mut backend = UffdMemoryBackend {
            socket_path: PathBuf::from("/tmp/unused.sock"),
            regions: vec![RegionMapping {
                base_host_virt_addr: ptr as u64,
                size: size as u64,
                offset: 0,
                page_size_kib: page_size,
            }],
            features: 0,
            uffd_fd: -1,
            wp_async: false,
            handoff_stream: None,
            snapfile_offset: 0,
            standalone: false,
        };

        // Write pages 0 and 2, leave 1 and 3 untouched so only 0 and 2 are
        // resident.
        // SAFETY: ptr and ptr + 2*page_size are within the valid mapping.
        unsafe {
            std::ptr::write_volatile(ptr as *mut u8, 1u8);
            std::ptr::write_volatile((ptr as *mut u8).add((2 * page_size) as usize), 1u8);
        }
        let first = query_and_reset_dirty_bitmap_impl(&mut backend).unwrap();
        assert_eq!(first.page_size, page_size);
        let mut expected = Vec::new();
        set_dense_bitmap_bit(&mut expected, 0).unwrap();
        set_dense_bitmap_bit(&mut expected, 2).unwrap();
        assert_eq!(first.bitmap, expected);

        // The fallback is a full resident report every query: a new write is
        // reported together with all previously resident pages.
        // SAFETY: ptr + 3*page_size is within the valid mapping.
        unsafe { std::ptr::write_volatile((ptr as *mut u8).add((3 * page_size) as usize), 1u8) };
        let second = query_and_reset_dirty_bitmap_impl(&mut backend).unwrap();
        assert_eq!(second.page_size, page_size);
        let mut expected = Vec::new();
        set_dense_bitmap_bit(&mut expected, 0).unwrap();
        set_dense_bitmap_bit(&mut expected, 2).unwrap();
        set_dense_bitmap_bit(&mut expected, 3).unwrap();
        assert_eq!(second.bitmap, expected);

        // SAFETY: the mapping established above is still valid.
        unsafe { libc::munmap(ptr, size) };
    }
}

#[cfg(test)]
mod indexed_tracking_tests {
    use super::*;
    use crate::{FileBackend, GuestAddress, HostMemMapping};
    use std::io::{Read, Seek, Write};
    use std::sync::Arc;
    use vmm_sys_util::tempfile::TempFile;

    fn indexed_page_observations(
        mapping: &HostMemMapping,
        phase: &str,
    ) -> Vec<(bool, PagemapEntry)> {
        let mut residency = vec![0u8; (mapping.size() / 4096) as usize];
        // SAFETY: the live mapping owner retains this entire page-aligned range.
        assert_eq!(
            unsafe {
                libc::mincore(
                    mapping.host_address() as *mut libc::c_void,
                    mapping.size() as usize,
                    residency.as_mut_ptr(),
                )
            },
            0
        );
        let mut result = Vec::new();
        PagemapBatchReader::new()
            .unwrap()
            .scan_range(
                mapping.host_address(),
                mapping.size(),
                4096,
                |index, entry| {
                    result.push((residency[index as usize] & 1 != 0, entry));
                    Ok(())
                },
            )
            .unwrap();
        eprintln!(
            "{}: {:?}",
            phase,
            result
                .iter()
                .map(|(resident, entry)| (*resident, format!("{:#x}", entry.raw())))
                .collect::<Vec<_>>()
        );
        result
    }

    fn indexed_mapping_swap_bytes(mapping: &HostMemMapping) -> u64 {
        let mut selected = false;
        for line in std::fs::read_to_string("/proc/self/smaps").unwrap().lines() {
            if let Some((start, end)) = line
                .split_whitespace()
                .next()
                .and_then(|part| part.split_once('-'))
            {
                let start = u64::from_str_radix(start, 16).unwrap();
                let end = u64::from_str_radix(end, 16).unwrap();
                selected = start <= mapping.host_address() && mapping.host_address() < end;
                if selected {
                    assert_eq!(
                        (start, end - start),
                        (mapping.host_address(), mapping.size()),
                        "swap accounting must cover only the owned mapping"
                    );
                }
            } else if selected {
                if let Some(kib) = line.strip_prefix("Swap:") {
                    return kib
                        .split_whitespace()
                        .next()
                        .unwrap()
                        .parse::<u64>()
                        .unwrap()
                        * 1024;
                }
            }
        }
        panic!("owned mapping missing from smaps");
    }

    /// Host qualification test: failure is intentional if mandatory kernel
    /// facilities or permissions are absent. It is never a resident-page fallback.
    #[test]
    #[ignore = "requires kernel WP_ASYNC on private file mappings and privileged userfaultfd"]
    fn indexed_wp_async_tracks_private_zero_writes_across_epochs() {
        let file = if let Some(path) = std::env::var_os("CONCH_TEST_EROFS_FILE") {
            std::fs::File::open(path).unwrap()
        } else {
            let mut file = TempFile::new().unwrap().into_file();
            file.write_all(&vec![7; 8192]).unwrap();
            file.rewind().unwrap();
            file
        };
        assert_eq!(
            file.metadata().unwrap().len(),
            8192,
            "qualification fixture must contain 8192 bytes of value 7"
        );
        let file = Arc::new(file);
        // Heat the backing cache through read(2), without faulting the RAM VMA.
        // This is deliberately different from reading through the mapping.
        let mut heater = file.try_clone().unwrap();
        heater.rewind().unwrap();
        let mut hot = vec![0u8; 8192];
        heater.read_exact(&mut hot).unwrap();
        assert!(hot.iter().all(|v| *v == 7));
        let mapping = HostMemMapping::new(
            GuestAddress(0),
            None,
            8192,
            Some(FileBackend {
                file: file.clone(),
                offset: 0,
                page_size: 4096,
            }),
            false,
            false,
            false,
        )
        .unwrap();
        let untracked = indexed_page_observations(&mapping, "cache-hot before UFFD registration");
        assert!(untracked.iter().all(|(_, entry)| !entry.is_resident()));
        assert!(
            untracked.iter().all(|(resident, _)| *resident),
            "fixture did not heat the backing page cache"
        );
        let regions = [(mapping.host_address(), 8192, 0)];
        initialize_indexed_tracking(&regions)
            .expect("host does not satisfy indexed RAM WP_ASYNC requirements");
        let protected =
            indexed_page_observations(&mapping, "cache-hot after WP-only registration/protection");
        assert!(
            protected
                .iter()
                .all(|(_, entry)| !entry.is_present() && entry.is_uffd_wp()),
            "registration must leave previously unfaulted pages non-present and protected"
        );
        assert_eq!(
            indexed_mapping_swap_bytes(&mapping),
            0,
            "unpopulated WP markers are not swapped COW payloads"
        );
        let peer = HostMemMapping::new(
            GuestAddress(0),
            None,
            8192,
            Some(FileBackend {
                file: file.clone(),
                offset: 0,
                page_size: 4096,
            }),
            false,
            false,
            false,
        )
        .unwrap();
        // SAFETY: peer owns both pages; these reads model another instance making
        // the shared backing cache hot while this instance has no present PTEs.
        unsafe {
            assert_eq!(std::ptr::read_volatile(peer.host_address() as *const u8), 7);
            assert_eq!(
                std::ptr::read_volatile((peer.host_address() + 4096) as *const u8),
                7
            );
        }
        indexed_page_observations(
            &mapping,
            "after another private mapping reads the cache-hot file",
        );
        let first = query_and_reset_dirty_bitmap().unwrap();
        assert_eq!(first.bitmap, vec![0]);
        // A read fault creates a present clean PTE, but does not create a dirty page.
        assert_eq!(
            unsafe { std::ptr::read_volatile(mapping.host_address() as *const u8) },
            7
        );
        indexed_page_observations(&mapping, "after a clean read fault");
        assert_eq!(query_and_reset_dirty_bitmap().unwrap().bitmap, vec![0]);
        // SAFETY: the owner retains both pages throughout this test.
        unsafe { std::ptr::write_bytes(mapping.host_address() as *mut u8, 0, 4096) };
        initialize_indexed_tracking(&regions).unwrap(); // Must preserve existing dirty bits.
        let second = query_and_reset_dirty_bitmap().unwrap();
        assert_eq!(second.bitmap, vec![1]);
        assert_eq!(query_and_reset_dirty_bitmap().unwrap().bitmap, vec![0]);
        unsafe { std::ptr::write_volatile((mapping.host_address() + 4096) as *mut u8, 9) };
        assert_eq!(query_and_reset_dirty_bitmap().unwrap().bitmap, vec![2]);
        if std::env::var_os("CONCH_TEST_SWAP").is_some() {
            assert!(
                std::fs::read_to_string("/proc/swaps")
                    .unwrap()
                    .lines()
                    .count()
                    > 1,
                "swap qualification requires an active swap area"
            );
            // Both pages are now private COW pages with a clean protected epoch.
            // PAGEOUT requests reclaim only for this owned 8 KiB test mapping.
            assert_eq!(
                unsafe {
                    libc::madvise(
                        mapping.host_address() as *mut libc::c_void,
                        8192,
                        libc::MADV_PAGEOUT,
                    )
                },
                0
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
            let swapped =
                indexed_page_observations(&mapping, "protected COW pages after explicit pageout");
            let swapped_bytes = indexed_mapping_swap_bytes(&mapping);
            eprintln!("owned VMA Swap after PAGEOUT: {} bytes", swapped_bytes);
            assert_eq!(
                swapped_bytes, 8192,
                "PAGEOUT did not place the owned COW payloads in swap"
            );
            assert!(
                swapped.iter().all(|(_, entry)| !entry.is_present()
                    && entry.is_swapped()
                    && entry.is_uffd_wp()),
                "protected COW pages did not enter swap with WP retained"
            );
            assert_eq!(query_and_reset_dirty_bitmap().unwrap().bitmap, vec![0]);
            // SAFETY: the owner retains this page; writing zero faults it back
            // from swap and must clear WP even though its logical value stays zero.
            unsafe { std::ptr::write_volatile(mapping.host_address() as *mut u8, 0) };
            assert_eq!(
                unsafe {
                    libc::madvise(
                        mapping.host_address() as *mut libc::c_void,
                        4096,
                        libc::MADV_PAGEOUT,
                    )
                },
                0
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
            let dirty_swap =
                indexed_page_observations(&mapping, "dirty zero COW page after explicit pageout");
            assert!(
                !dirty_swap[0].1.is_present()
                    && dirty_swap[0].1.is_swapped()
                    && !dirty_swap[0].1.is_uffd_wp(),
                "dirty COW page did not preserve its unprotected swap state"
            );
            assert_eq!(query_and_reset_dirty_bitmap().unwrap().bitmap, vec![1]);
            // SAFETY: both owned COW pages remain mapped; swapped contents must
            // survive and a read fault must not consume the next dirty epoch.
            unsafe {
                assert_eq!(
                    std::ptr::read_volatile(mapping.host_address() as *const u8),
                    0
                );
                assert_eq!(
                    std::ptr::read_volatile((mapping.host_address() + 4096) as *const u8),
                    9
                );
            }
            assert_eq!(query_and_reset_dirty_bitmap().unwrap().bitmap, vec![0]);
        }
        let mut original = file.try_clone().unwrap();
        original.rewind().unwrap();
        let mut bytes = vec![0; 8192];
        original.read_exact(&mut bytes).unwrap();
        assert!(bytes.iter().all(|v| *v == 7));
        *backend_state().lock().unwrap() = None;
    }
}
