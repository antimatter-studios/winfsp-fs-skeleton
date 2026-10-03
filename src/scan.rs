//! Start-up enumeration of the disks already attached when the watcher
//! starts, which never produce an arrival notification.

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
