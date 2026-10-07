//! FS-agnostic helpers used by both the foreground watcher and the
//! SCM service variant.
//!
//! - [`pick_drive_letter`] -- the lowest free letter in `E..=Z`.
//! - [`GUID_DEVINTERFACE_DISK`] -- the device-interface class GUID
//!   passed to `RegisterDeviceNotificationW` for disk-arrival
//!   subscriptions.
//! - [`device_interface_name`] -- pull the device path string out of
//!   a `DEV_BROADCAST_DEVICEINTERFACE_W` lparam payload.
//! - [`detect_at`] -- read a partition's probe window and hand it to
//!   the consumer's `FsBackend::detect`. Both the foreground watcher
//!   and the service probe through it, and it is host-independent, so
//!   it is tested on every host.
//!
//! Detection of a specific filesystem's superblock magic lives in the
//! consumer's `FsBackend::detect` -- not here.

#![allow(dead_code)]

use crate::device::BlockSource;
use crate::FsBackend;

/// The probe window is read in whole units of this many bytes: a
/// multiple of both 512-byte and 4Kn sector sizes, so a raw device
/// accepts the read.
pub const PROBE_SECTOR: usize = 4096;

/// Read the probe window of the partition that starts `offset` bytes
/// into `src`, and ask `B::detect` whether it holds `B`'s filesystem.
///
/// A read that fails -- the window runs past the end of the device, or
/// the device refuses it -- is "not ours", not an error: the watcher
/// probes every partition it sees, and most of them belong to another
/// filesystem.
pub fn detect_at<B: FsBackend, S: BlockSource + ?Sized>(src: &S, offset: u64) -> bool {
    let mut buf = vec![0u8; PROBE_SECTOR];
    if src.read_at(offset, &mut buf).is_err() {
        return false;
    }
    B::detect(&buf)
}

/// Pick the lowest free drive letter in `E..=Z` (skipping ones already
/// in use according to `GetLogicalDrives`). Returns `None` if none are
/// free.
///
/// Skips A..D so we don't collide with floppy / system / CD-ROM
/// reservations the user expects to be sticky.
#[cfg(target_os = "windows")]
pub fn pick_drive_letter() -> Option<char> {
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;
    let in_use = unsafe { GetLogicalDrives() };
    // Bit 0 = A, bit 4 = E, ...
    for i in 4u32..26 {
        if (in_use >> i) & 1 == 0 {
            return Some((b'A' + i as u8) as char);
        }
    }
    None
}

/// `GUID_DEVINTERFACE_DISK` -- physical disk device interface class.
/// Pass this in `DEV_BROADCAST_DEVICEINTERFACE_W` when registering for
/// `WM_DEVICECHANGE` notifications to receive disk arrival/removal
/// events. Disk-level (rather than volume-level) subscription is
/// required because Windows refuses to assign drive letters to
/// partitions whose type code it doesn't recognise (e.g. `0x83`
/// Linux), so a volume-level subscription would never fire for
/// typical Linux filesystem media.
#[cfg(target_os = "windows")]
pub const GUID_DEVINTERFACE_DISK: windows_sys::core::GUID = windows_sys::core::GUID {
    data1: 0x53F5_6307,
    data2: 0xB6BF,
    data3: 0x11D0,
    data4: [0x94, 0xF2, 0x00, 0xA0, 0xC9, 0x1E, 0xFB, 0x8B],
};

/// Pull the device path out of a `DEV_BROADCAST_DEVICEINTERFACE_W`
/// pointer received via `WM_DEVICECHANGE` lparam. The struct's
/// `dbcc_name` field is a flexible array of `u16`; the actual name
/// length is `dbcc_size` minus the fixed-prefix size, terminated by
/// the first null. Returns the path as a Rust `String` (the
/// device-interface name is always ASCII-printable in practice).
///
/// # Safety
///
/// `bdi` must point at a properly-aligned, fully-initialised
/// `DEV_BROADCAST_DEVICEINTERFACE_W` whose `dbcc_size` covers the
/// embedded `dbcc_name` payload. WM_DEVICECHANGE delivers exactly
/// such pointers, so the foreground/service wndprocs satisfy this.
#[cfg(target_os = "windows")]
pub unsafe fn device_interface_name(
    bdi: *const windows_sys::Win32::UI::WindowsAndMessaging::DEV_BROADCAST_DEVICEINTERFACE_W,
) -> Option<String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    if bdi.is_null() {
        return None;
    }
    let total = (*bdi).dbcc_size as usize;
    // Offset of `dbcc_name` within the struct: dbcc_size (u32, 4) +
    // dbcc_devicetype (u32, 4) + dbcc_reserved (u32, 4) +
    // dbcc_classguid (GUID, 16) = 28. We can't use `size_of - 2`
    // because the struct is padded to its 4-byte alignment, so
    // size_of returns 32 -- which would skip the first wide char of
    // the name (so `\\?\STORAGE...` becomes `\?\STORAGE...`, an
    // ERROR_INVALID_NAME path).
    const DBCC_NAME_OFFSET: usize = 4 + 4 + 4 + 16;
    if total <= DBCC_NAME_OFFSET {
        return None;
    }
    let name_bytes = total - DBCC_NAME_OFFSET;
    let name_chars = name_bytes / 2;
    let ptr = (bdi as *const u8).add(DBCC_NAME_OFFSET) as *const u16;
    let slice = std::slice::from_raw_parts(ptr, name_chars);
    let trimmed = match slice.iter().position(|&c| c == 0) {
        Some(n) => &slice[..n],
        None => slice,
    };
    let s = OsString::from_wide(trimmed).into_string().ok()?;
    Some(s)
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A disk image in memory. A read past its end fails, as a
    /// device's does.
    struct Mem(Vec<u8>);

    impl BlockSource for Mem {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
            let start = usize::try_from(offset).map_err(|_| std::io::ErrorKind::InvalidInput)?;
            let end = start
                .checked_add(buf.len())
                .filter(|&end| end <= self.0.len())
                .ok_or(std::io::ErrorKind::UnexpectedEof)?;
            buf.copy_from_slice(&self.0[start..end]);
            Ok(())
        }

        fn size(&self) -> u64 {
            self.0.len() as u64
        }
    }

    /// Where the partition under test starts on the disk: past the
    /// first MiB, as a partitioning tool puts it, so a probe that reads
    /// from the start of the disk rather than of the partition misses.
    const PART: usize = 1 << 20;

    /// Btrfs's magic: the primary superblock is at 64 KiB, and the
    /// magic at 0x40 within it.
    const DEEP_MAGIC_AT: usize = 0x1_0040;
    const DEEP_MAGIC: &[u8] = b"_BHRfS_M";

    /// The length of the slice `Deep::detect` was last handed.
    static DEEP_SAW: AtomicUsize = AtomicUsize::new(0);

    /// A backend whose magic is where Btrfs's is.
    struct Deep;

    impl FsBackend for Deep {
        const FS_NAME: &'static str = "deep";
        const SERVICE_NAME: &'static str = "DeepWatcher";
        const LAUNCHER_SERVICE_CLASS: &'static str = "deep-mount";
        const FILE_EXTENSION: &'static str = "img";
        const PROBE_BYTES: usize = DEEP_MAGIC_AT + DEEP_MAGIC.len();

        fn detect(bytes: &[u8]) -> bool {
            DEEP_SAW.store(bytes.len(), Ordering::SeqCst);
            bytes.get(DEEP_MAGIC_AT..DEEP_MAGIC_AT + DEEP_MAGIC.len()) == Some(DEEP_MAGIC)
        }
    }

    /// The length of the slice `Shallow::detect` was last handed.
    static SHALLOW_SAW: AtomicUsize = AtomicUsize::new(0);

    /// A backend that leaves `PROBE_BYTES` at its default, with its
    /// magic where ext4's is.
    struct Shallow;

    impl FsBackend for Shallow {
        const FS_NAME: &'static str = "shallow";
        const SERVICE_NAME: &'static str = "ShallowWatcher";
        const LAUNCHER_SERVICE_CLASS: &'static str = "shallow-mount";
        const FILE_EXTENSION: &'static str = "img";

        fn detect(bytes: &[u8]) -> bool {
            SHALLOW_SAW.store(bytes.len(), Ordering::SeqCst);
            bytes.get(1024 + 0x38..1024 + 0x3A) == Some(&[0x53, 0xEF][..])
        }
    }

    /// A disk of `PART` bytes of nothing, then a partition of
    /// `part_len` bytes, with `magic` written `at` bytes into it.
    fn disk(part_len: usize, at: usize, magic: &[u8]) -> Mem {
        let mut bytes = vec![0u8; PART + part_len];
        bytes[PART + at..PART + at + magic.len()].copy_from_slice(magic);
        Mem(bytes)
    }

    #[test]
    fn a_magic_at_64_kib_is_detected_through_the_probe() {
        let img = disk(4 << 20, DEEP_MAGIC_AT, DEEP_MAGIC);
        assert!(
            detect_at::<Deep, _>(&img, PART as u64),
            "a backend whose magic is at 0x1_0040 was not detected: detect was handed {} bytes",
            DEEP_SAW.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn the_window_is_what_the_backend_asks_for_in_whole_sectors() {
        let img = disk(4 << 20, 0, b"");
        detect_at::<Deep, _>(&img, PART as u64);
        let saw = DEEP_SAW.load(Ordering::SeqCst);
        assert!(
            saw >= Deep::PROBE_BYTES,
            "detect was handed {saw} bytes, the backend asks for {}",
            Deep::PROBE_BYTES
        );
        assert_eq!(
            saw % PROBE_SECTOR,
            0,
            "a {saw}-byte window is not whole sectors"
        );
    }

    #[test]
    fn a_backend_that_asks_for_the_default_gets_4096_bytes() {
        let img = disk(4 << 20, 1024 + 0x38, &[0x53, 0xEF]);
        assert!(detect_at::<Shallow, _>(&img, PART as u64));
        assert_eq!(SHALLOW_SAW.load(Ordering::SeqCst), 4096);
    }

    #[test]
    fn a_partition_without_the_magic_is_not_detected() {
        let img = disk(4 << 20, 0, b"");
        assert!(!detect_at::<Deep, _>(&img, PART as u64));
    }

    #[test]
    fn a_window_past_the_end_of_the_device_is_not_ours_rather_than_an_error() {
        let img = disk(2048, 0, b"");
        assert!(!detect_at::<Shallow, _>(&img, PART as u64));
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::DEV_BROADCAST_DEVICEINTERFACE_W;

    /// Hand-build a `DEV_BROADCAST_DEVICEINTERFACE_W` byte blob with a
    /// known device path tail and assert `device_interface_name`
    /// extracts it exactly. Regression for the historical bug where
    /// the dbcc_name offset was computed via `size_of - 2`, which
    /// rounds up to the struct's 4-byte alignment and dropped the
    /// first wide char (turning `\\?\STORAGE...` into `\?\STORAGE...`,
    /// an ERROR_INVALID_NAME path).
    fn build_blob(path: &str) -> Vec<u8> {
        // Layout: dbcc_size(4) + dbcc_devicetype(4) + dbcc_reserved(4)
        //       + dbcc_classguid(16) + dbcc_name (wide chars + NUL)
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let header = 4 + 4 + 4 + 16usize;
        let total = header + wide.len() * 2;
        let mut blob = vec![0u8; total];
        blob[0..4].copy_from_slice(&(total as u32).to_le_bytes());
        // dbcc_devicetype, dbcc_reserved, dbcc_classguid stay zero --
        // device_interface_name doesn't read them.
        let name_bytes_off = header;
        for (i, &w) in wide.iter().enumerate() {
            let off = name_bytes_off + i * 2;
            blob[off..off + 2].copy_from_slice(&w.to_le_bytes());
        }
        blob
    }

    #[test]
    fn extracts_storage_disk_path() {
        let path = r"\\?\STORAGE#Disk#{12345678-1234-1234-1234-1234567890ab}#abcdef";
        let blob = build_blob(path);
        let bdi = blob.as_ptr() as *const DEV_BROADCAST_DEVICEINTERFACE_W;
        let got = unsafe { device_interface_name(bdi) };
        assert_eq!(got.as_deref(), Some(path));
    }

    #[test]
    fn extracts_short_path() {
        let path = r"\\?\X:";
        let blob = build_blob(path);
        let bdi = blob.as_ptr() as *const DEV_BROADCAST_DEVICEINTERFACE_W;
        let got = unsafe { device_interface_name(bdi) };
        assert_eq!(got.as_deref(), Some(path));
    }

    #[test]
    fn returns_none_on_null() {
        let got = unsafe { device_interface_name(std::ptr::null()) };
        assert!(got.is_none());
    }

    #[test]
    fn returns_none_when_size_is_too_small() {
        // Size header smaller than the fixed prefix -- must not panic
        // and must report None instead of slicing OOB.
        let mut blob = vec![0u8; 28];
        blob[0..4].copy_from_slice(&(20u32).to_le_bytes());
        let bdi = blob.as_ptr() as *const DEV_BROADCAST_DEVICEINTERFACE_W;
        let got = unsafe { device_interface_name(bdi) };
        assert!(got.is_none());
    }

    #[test]
    fn first_wide_char_is_preserved() {
        // Specific regression for the `size_of - 2` bug: the leading
        // backslash of a `\\?\...` path used to be dropped because the
        // computed offset overshot by 2 bytes. Verify the first char
        // is still '\\'.
        let path = r"\\?\STORAGE#Disk#abc";
        let blob = build_blob(path);
        let bdi = blob.as_ptr() as *const DEV_BROADCAST_DEVICEINTERFACE_W;
        let got = unsafe { device_interface_name(bdi) }.expect("Some");
        assert!(
            got.starts_with(r"\\?\"),
            "expected leading \\\\?\\, got {got:?}"
        );
    }
}
