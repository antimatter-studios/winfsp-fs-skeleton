//! Start-up enumeration of the disks already attached when the watcher
//! starts, which never produce an arrival notification.
//!
//! `RegisterDeviceNotificationW` reports *changes*. A disk that was
//! plugged in before the watcher registered -- every disk, at boot,
//! because the service starts after the storage stack has enumerated
//! them -- produces no `DBT_DEVICEARRIVAL`, so a watcher that only
//! reacts to arrivals mounts nothing until the user replugs. Both the
//! service and the foreground watcher therefore walk the disks that
//! are present when they start, and the service walks them again when
//! a console session appears (at boot there is none to mount into, so
//! the first scan is deferred).
//!
//! The walk is split from its Windows source so the decision of what
//! to probe is testable on any host: [`DiskSource`] lists the present
//! disk interface paths and [`disks_to_probe`] picks the ones not
//! already mounted.

use anyhow::Result;

/// Lists the disk-class device interface paths currently present --
/// the same `\\?\...#{53f56307-...}` strings `WM_DEVICECHANGE` hands
/// the arrival handler, so a path from either can be probed alike.
pub trait DiskSource {
    fn present_disks(&self) -> Result<Vec<String>>;
}

/// Whether two disk interface paths name the same device. The set-up
/// API returns them in lower case while the arrival notification
/// keeps the driver's mixed case, so a byte comparison would treat
/// one disk as two -- mounting it twice on a rescan, or failing to
/// unmount it on removal.
pub fn same_device(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The present disks that should be probed: every disk `src` lists,
/// once each, minus those already holding a mount (`tracked`). Order
/// follows `src`, so drive letters are assigned in enumeration order.
///
/// An enumeration failure is returned, not flattened into an empty
/// list, which would read exactly like a machine with nothing plugged
/// in.
pub fn disks_to_probe<S: DiskSource + ?Sized>(src: &S, tracked: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for disk in src.present_disks()? {
        let seen = tracked
            .iter()
            .chain(out.iter())
            .any(|t| same_device(t, &disk));
        if !seen {
            out.push(disk);
        }
    }
    Ok(out)
}

/// The Windows [`DiskSource`]: the set-up API's list of present
/// `GUID_DEVINTERFACE_DISK` interfaces.
#[cfg(target_os = "windows")]
pub struct PresentDisks;

#[cfg(target_os = "windows")]
impl DiskSource for PresentDisks {
    fn present_disks(&self) -> Result<Vec<String>> {
        unsafe { imp::present_disks() }
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use anyhow::{anyhow, Result};
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::ptr;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
        SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
        SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    };
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NO_MORE_ITEMS};

    use crate::probe::GUID_DEVINTERFACE_DISK;

    /// Offset of `DevicePath` in `SP_DEVICE_INTERFACE_DETAIL_DATA_W`:
    /// one `u32` (`cbSize`).
    const DEVICE_PATH_OFFSET: usize = 4;

    pub(super) unsafe fn present_disks() -> Result<Vec<String>> {
        let set = SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_DISK,
            ptr::null(),
            ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if set == -1 {
            return Err(anyhow!(
                "SetupDiGetClassDevsW(GUID_DEVINTERFACE_DISK) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let result = walk(set);
        SetupDiDestroyDeviceInfoList(set);
        result
    }

    unsafe fn walk(set: isize) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut index = 0u32;
        loop {
            let mut iface: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            iface.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(
                set,
                ptr::null(),
                &GUID_DEVINTERFACE_DISK,
                index,
                &mut iface,
            ) == 0
            {
                if GetLastError() == ERROR_NO_MORE_ITEMS {
                    return Ok(out);
                }
                return Err(anyhow!(
                    "SetupDiEnumDeviceInterfaces({index}) failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            index += 1;

            // First call sizes the buffer; it fails by design with
            // ERROR_INSUFFICIENT_BUFFER and fills `needed`.
            let mut needed = 0u32;
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &iface,
                ptr::null_mut(),
                0,
                &mut needed,
                ptr::null_mut(),
            );
            if (needed as usize) <= DEVICE_PATH_OFFSET {
                continue;
            }
            // u32-aligned backing so the struct header is aligned.
            let mut buf = vec![0u32; (needed as usize).div_ceil(4)];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(
                set,
                &iface,
                detail,
                needed,
                ptr::null_mut(),
                ptr::null_mut(),
            ) == 0
            {
                eprintln!(
                    "SetupDiGetDeviceInterfaceDetailW({}) failed: {}",
                    index - 1,
                    std::io::Error::last_os_error()
                );
                continue;
            }
            let chars = (needed as usize - DEVICE_PATH_OFFSET) / 2;
            let wide = std::slice::from_raw_parts(
                (detail as *const u8).add(DEVICE_PATH_OFFSET) as *const u16,
                chars,
            );
            let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
            if let Ok(path) = OsString::from_wide(&wide[..end]).into_string() {
                if !path.is_empty() {
                    out.push(path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::cell::Cell;

    /// Stands in for the set-up API: hands back a fixed list of disk
    /// interface paths, or an error, and counts how often it was asked.
    struct FakeDisks {
        disks: Vec<&'static str>,
        fail: bool,
        calls: Cell<usize>,
    }

    impl FakeDisks {
        fn new(disks: &[&'static str]) -> Self {
            FakeDisks {
                disks: disks.to_vec(),
                fail: false,
                calls: Cell::new(0),
            }
        }
        fn failing() -> Self {
            FakeDisks {
                disks: Vec::new(),
                fail: true,
                calls: Cell::new(0),
            }
        }
    }

    impl DiskSource for FakeDisks {
        fn present_disks(&self) -> anyhow::Result<Vec<String>> {
            self.calls.set(self.calls.get() + 1);
            if self.fail {
                return Err(anyhow!("SetupDiGetClassDevsW failed"));
            }
            Ok(self.disks.iter().map(|s| s.to_string()).collect())
        }
    }

    const USB: &str =
        r"\\?\usbstor#disk&ven_generic&prod_flash#0001#{53f56307-b6bf-11d0-94f2-00a0c91efb8b}";
    const SATA: &str =
        r"\\?\scsi#disk&ven_wdc&prod_wd40#4&1a2b3c&0&000100#{53f56307-b6bf-11d0-94f2-00a0c91efb8b}";

    /// The reported bug: a disk that was attached before the watcher
    /// started never produces an arrival, so the start-up scan is the
    /// only thing that can find it. With nothing mounted yet, every
    /// present disk must come back for probing.
    #[test]
    fn disks_present_at_start_are_probed() {
        let src = FakeDisks::new(&[USB, SATA]);
        let got = disks_to_probe(&src, &[]).unwrap();
        assert_eq!(got, vec![USB.to_string(), SATA.to_string()]);
        assert_eq!(src.calls.get(), 1);
    }

    #[test]
    fn no_present_disks_means_nothing_to_probe() {
        let src = FakeDisks::new(&[]);
        assert!(disks_to_probe(&src, &[]).unwrap().is_empty());
    }

    /// A rescan (at logon, say) must not mount a disk a second time.
    /// The arrival notification and the set-up API spell the same
    /// interface path in different case, so the match is case-blind.
    #[test]
    fn a_disk_already_mounted_is_not_probed_again() {
        let src = FakeDisks::new(&[USB, SATA]);
        let tracked = vec![USB.to_ascii_uppercase()];
        let got = disks_to_probe(&src, &tracked).unwrap();
        assert_eq!(got, vec![SATA.to_string()]);
    }

    #[test]
    fn a_disk_listed_twice_is_probed_once() {
        let upper = USB.to_ascii_uppercase();
        let src = FakeDisks::new(&[USB, SATA]);
        let mut twice = src.present_disks().unwrap();
        twice.push(upper);
        let dup = DupDisks(twice);
        let got = disks_to_probe(&dup, &[]).unwrap();
        assert_eq!(got, vec![USB.to_string(), SATA.to_string()]);
    }

    struct DupDisks(Vec<String>);
    impl DiskSource for DupDisks {
        fn present_disks(&self) -> anyhow::Result<Vec<String>> {
            Ok(self.0.clone())
        }
    }

    /// An enumeration failure is reported, not read as "no disks":
    /// an empty answer would look exactly like a machine with nothing
    /// plugged in.
    #[test]
    fn an_enumeration_failure_is_an_error() {
        let src = FakeDisks::failing();
        assert!(disks_to_probe(&src, &[]).is_err());
    }

    #[test]
    fn same_device_ignores_case_only() {
        assert!(same_device(USB, &USB.to_ascii_uppercase()));
        assert!(!same_device(USB, SATA));
        assert!(!same_device(USB, ""));
    }
}
