//! FS-agnostic helpers used by both the foreground watcher and the
//! SCM service variant.
//!
//! - [`pick_drive_letter`] -- the lowest free letter in `E..=Z`.
//! - [`GUID_DEVINTERFACE_DISK`] -- the device-interface class GUID
//!   passed to `RegisterDeviceNotificationW` for disk-arrival
//!   subscriptions.
//! - [`device_interface_name`] -- pull the device path string out of
//!   a `DEV_BROADCAST_DEVICEINTERFACE_W` lparam payload.
//!
//! Detection of a specific filesystem's superblock magic lives in the
//! consumer's `FsBackend::detect` -- not here.

#![allow(dead_code)]

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
