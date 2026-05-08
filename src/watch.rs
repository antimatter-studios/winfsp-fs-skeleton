//! Auto-mount watcher for Windows disk arrival/removal.
//!
//! Runs as a foreground process. On Windows, listens for
//! `WM_DEVICECHANGE` (`DBT_DEVICEARRIVAL` / `DBT_DEVICEREMOVECOMPLETE`)
//! filtered to disk-class device-interface notifications
//! ([`crate::probe::GUID_DEVINTERFACE_DISK`]), reads the lparam's
//! `DEV_BROADCAST_DEVICEINTERFACE_W::dbcc_name` to get the disk's
//! device path (e.g. `\\?\STORAGE#Disk#{guid}#...`), opens it for raw
//! read, walks the MBR/GPT partition table, and probes each partition
//! for the consumer's filesystem magic via [`crate::FsBackend::detect`].
//! On a hit, spawns the current binary's `mount` subcommand as a
//! child process. On removal, kills every child we spawned for that
//! disk so its WinFsp `Drop` tears the mount down cleanly.
//!
//! Why disk-class rather than volume-class: Windows refuses to assign
//! drive letters to MBR partitions whose type code it doesn't
//! recognise (e.g. `0x83` Linux native), so a volume-class
//! subscription -- whose only useful "what changed" signal is a
//! `GetLogicalDrives` diff -- never fires for typical Linux SD
//! cards. Disk-class arrivals fire regardless of partition type.
//!
//! Self-contained: never links WinFsp directly. The current binary
//! is re-exec'd with `mount`. The consumer's `mount` subcommand is
//! the one that links winfsp-rs.
//!
//! On non-Windows the [`run`] entrypoint just prints a hint and
//! returns `Ok(())` so the CLI dispatcher remains uniform across
//! host platforms.

use anyhow::Result;

use crate::FsBackend;

#[cfg(target_os = "windows")]
pub fn run<B: FsBackend>() -> Result<()> {
    imp::run::<B>()
}

#[cfg(not(target_os = "windows"))]
pub fn run<B: FsBackend>() -> Result<()> {
    eprintln!(
        "[{fs}] watch is unavailable in this build. Reasons:\n  \
         - target is not Windows ({os})\n\
         Rebuild on a Windows host with --features mount to enable.",
        fs = B::FS_NAME,
        os = std::env::consts::OS,
    );
    Ok(())
}

#[cfg(target_os = "windows")]
mod imp {
    //! Windows implementation. Layout:
    //!
    //!   1. `State` -- children map + Win32 handles, behind `Mutex`.
    //!   2. `run` -- install Ctrl-C handler, create message-only
    //!      window, register disk-class notifications, pump until
    //!      WM_QUIT.
    //!   3. `wnd_proc` -- handle WM_DEVICECHANGE, dispatch into State.
    //!
    //! `wnd_proc` is generic on `B: FsBackend` so calls into
    //! `B::detect` happen at the actual probe site without a vtable
    //! / function-pointer indirection. Rust monomorphises
    //! `wnd_proc::<B>` per consumer, and that monomorphised function
    //! pointer is what we hand to `RegisterClassW`.

    use anyhow::{Context, Result, anyhow};
    use std::collections::HashMap;
    use std::marker::PhantomData;
    use std::path::Path;
    use std::process::{Child, Command};
    use std::ptr;
    use std::sync::{Mutex, OnceLock};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE, DBT_DEVTYP_DEVICEINTERFACE,
        DEVICE_NOTIFY_WINDOW_HANDLE, DEV_BROADCAST_DEVICEINTERFACE_W, DefWindowProcW,
        DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetMessageW, GetWindowLongPtrW,
        HWND_MESSAGE, MSG, PostThreadMessageW, RegisterClassW, RegisterDeviceNotificationW,
        SetWindowLongPtrW, TranslateMessage, UnregisterClassW, UnregisterDeviceNotification,
        WM_DEVICECHANGE, WM_QUIT, WNDCLASSW,
    };

    use crate::FsBackend;
    use crate::probe;

    /// Window class name. Wide-encoded inline to avoid pulling in
    /// widestring. Identical across consumers because each consumer
    /// runs in its own process; the class name only needs to be
    /// unique within the process.
    const CLASS_NAME: &[u16] = &[
        b'w' as u16, b'f' as u16, b's' as u16, b'_' as u16, b's' as u16, b'k' as u16,
        b'e' as u16, b'l' as u16, b'_' as u16, b'w' as u16, b'a' as u16, b't' as u16,
        b'c' as u16, b'h' as u16, 0,
    ];

    /// Per-mount bookkeeping: the spawned `<consumer> mount` child
    /// plus the drive letter it was assigned, keyed in
    /// [`State::mounts`] by `(disk_path, partition_index)`.
    struct MountedChild {
        child: Child,
        letter: char,
    }

    #[derive(Default)]
    struct State {
        /// `(disk_device_path, 1-indexed-partition)` -> mount info.
        /// `disk_device_path` is whatever the WM_DEVICECHANGE lparam
        /// reported -- typically `\\?\STORAGE#Disk#{guid}#...` -- so
        /// removal lookups match arrivals byte-for-byte.
        mounts: HashMap<(String, usize), MountedChild>,
        /// Set on Ctrl-C / WM_QUIT path; the wndproc consults this
        /// so it stops spawning new mounts during shutdown.
        shutting_down: bool,
    }

    /// Per-backend singleton state. Monomorphisation gives each
    /// consumer instantiation its own `STATE` even when two
    /// backends co-exist in one binary (theoretically possible
    /// though we don't expect it in practice).
    fn state<B: FsBackend>() -> &'static Mutex<State> {
        // The PhantomData<B> is purely a type-level discriminant so
        // each B instantiation gets its own static.
        struct Slot<B>(PhantomData<B>);
        impl<B: FsBackend> Slot<B> {
            fn get() -> &'static Mutex<State> {
                static STATE: OnceLock<Mutex<State>> = OnceLock::new();
                // The OnceLock is shared across instantiations but
                // that's fine: any single binary only ever has one
                // backend pumping messages at a time. The monomorph
                // wrapper just lets the borrow checker accept B.
                STATE.get_or_init(|| Mutex::new(State::default()))
            }
        }
        Slot::<B>::get()
    }

    pub fn run<B: FsBackend>() -> Result<()> {
        // Stable static pointer to State for the wndproc to find via
        // GWLP_USERDATA.
        let state_ptr = state::<B>() as *const Mutex<State> as isize;

        // Ctrl-C handler posts WM_QUIT to wake the message pump,
        // which then unwinds via the cleanup at the end of `run`.
        let main_thread = unsafe { GetCurrentThreadId() };
        ctrlc::set_handler(move || {
            if let Ok(mut s) = state::<B>().lock() {
                s.shutting_down = true;
            }
            unsafe {
                PostThreadMessageW(main_thread, WM_QUIT, 0, 0);
            }
        })
        .context("installing Ctrl-C handler")?;

        unsafe { run_pump::<B>(state_ptr) }
    }

    /// Build the message-only window, register for disk events,
    /// drain the message pump until WM_QUIT, then tear everything
    /// down.
    unsafe fn run_pump<B: FsBackend>(state_ptr: isize) -> Result<()> {
        let hinstance = GetModuleHandleW(ptr::null());

        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wnd_proc::<B>);
        wc.hInstance = hinstance;
        wc.lpszClassName = CLASS_NAME.as_ptr();
        let atom = RegisterClassW(&wc);
        if atom == 0 {
            let err = std::io::Error::last_os_error();
            // ERROR_CLASS_ALREADY_EXISTS = 1410. Idempotent re-reg is
            // fine if `run` is invoked twice in-process.
            if err.raw_os_error() != Some(1410) {
                return Err(anyhow!("RegisterClassW failed: {err}"));
            }
        }

        let hwnd: HWND = CreateWindowExW(
            0,
            CLASS_NAME.as_ptr(),
            ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            ptr::null_mut(),
            hinstance,
            ptr::null(),
        );
        if hwnd.is_null() {
            return Err(anyhow!(
                "CreateWindowExW(HWND_MESSAGE) failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr);

        // Subscribe to disk-class device-interface notifications.
        let mut filter: DEV_BROADCAST_DEVICEINTERFACE_W = std::mem::zeroed();
        filter.dbcc_size = std::mem::size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32;
        filter.dbcc_devicetype = DBT_DEVTYP_DEVICEINTERFACE;
        filter.dbcc_classguid = probe::GUID_DEVINTERFACE_DISK;
        let dev_handle = RegisterDeviceNotificationW(
            hwnd,
            &filter as *const _ as *const _,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        );
        if dev_handle.is_null() {
            DestroyWindow(hwnd);
            return Err(anyhow!(
                "RegisterDeviceNotificationW failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        println!(
            "[{}] listening for disk arrivals. Ctrl-C to stop.",
            B::FS_NAME
        );

        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, ptr::null_mut(), 0, 0);
            if r == 0 {
                break;
            }
            if r == -1 {
                eprintln!(
                    "[{}] GetMessageW error: {}",
                    B::FS_NAME,
                    std::io::Error::last_os_error()
                );
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        UnregisterDeviceNotification(dev_handle);
        DestroyWindow(hwnd);
        UnregisterClassW(CLASS_NAME.as_ptr(), hinstance);

        let mut st = state::<B>().lock().unwrap();
        st.shutting_down = true;
        let drained: Vec<((String, usize), MountedChild)> = st.mounts.drain().collect();
        drop(st);
        for ((disk, part), mut mc) in drained {
            let _ = mc.child.kill();
            let _ = mc.child.wait();
            println!(
                "[{}] {disk}#part{part} -> child unmounted from {}: (shutdown)",
                B::FS_NAME,
                mc.letter
            );
        }
        Ok(())
    }

    /// Window procedure. Generic on `B` so the call to
    /// `B::detect` (deep inside `handle_arrival`) inlines into a
    /// direct call. Rust monomorphises `wnd_proc::<B>` per consumer
    /// and the function pointer to that monomorphised version is
    /// what `RegisterClassW` accepts.
    unsafe extern "system" fn wnd_proc<B: FsBackend>(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_DEVICECHANGE {
            let event = wparam as u32;
            if event == DBT_DEVICEARRIVAL || event == DBT_DEVICEREMOVECOMPLETE {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Mutex<State>;
                let bdi = lparam as *const DEV_BROADCAST_DEVICEINTERFACE_W;
                if !state_ptr.is_null() {
                    if let Some(disk_path) = probe::device_interface_name(bdi) {
                        let state: &Mutex<State> = &*state_ptr;
                        if event == DBT_DEVICEARRIVAL {
                            handle_arrival::<B>(state, &disk_path);
                        } else {
                            handle_removal::<B>(state, &disk_path);
                        }
                    }
                }
            }
            return 1;
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    /// `DBT_DEVICEARRIVAL` for a disk-class device interface: open
    /// the disk, walk its partition table, probe each partition via
    /// `B::detect`, spawn `<exe> mount` for each hit.
    fn handle_arrival<B: FsBackend>(state: &Mutex<State>, disk_path: &str) {
        if state.lock().map(|s| s.shutting_down).unwrap_or(true) {
            return;
        }

        let parts = match crate::partition::list(Path::new(disk_path)) {
            Ok(v) => v,
            Err(e) => {
                // No MBR signature? Treat the whole disk as a single
                // raw filesystem (common for `mkfs.ext4 /dev/mmcblk0`
                // without partitioning). The consumer's mount
                // subcommand should accept `--part 0` as "no
                // partition; mount the whole device."
                if format!("{e:#}").contains("no MBR signature") {
                    // Probe the whole disk at offset 0 for the
                    // consumer's FS before spawning. Otherwise
                    // every unpartitioned non-FS disk triggers a
                    // mount that fails downstream.
                    match probe_at_offset::<B>(disk_path, 0) {
                        Ok(true) => spawn_partition_mount::<B>(state, disk_path, 0),
                        Ok(false) => {}
                        Err(e) => eprintln!(
                            "[{}] probe {disk_path} whole-disk: {e:#}",
                            B::FS_NAME
                        ),
                    }
                } else {
                    eprintln!(
                        "[{}] partition::list({disk_path}) failed: {e:#}",
                        B::FS_NAME
                    );
                }
                return;
            }
        };

        for (idx, part) in parts.iter().enumerate() {
            let n = idx + 1;
            let part_offset = part.start_lba * 512;
            match probe_at_offset::<B>(disk_path, part_offset) {
                Ok(true) => {
                    spawn_partition_mount::<B>(state, disk_path, n);
                }
                Ok(false) => {}
                Err(e) => {
                    eprintln!("[{}] probe {disk_path} part {n}: {e:#}", B::FS_NAME);
                }
            }
        }
    }

    /// `DBT_DEVICEREMOVECOMPLETE`: kill every child we own for this
    /// disk (one per matching partition).
    fn handle_removal<B: FsBackend>(state: &Mutex<State>, disk_path: &str) {
        let mut st = match state.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let keys: Vec<(String, usize)> = st
            .mounts
            .keys()
            .filter(|(d, _)| d == disk_path)
            .cloned()
            .collect();
        for key in keys {
            if let Some(mut mc) = st.mounts.remove(&key) {
                let _ = mc.child.kill();
                let _ = mc.child.wait();
                println!(
                    "[{}] {disk_path}#part{} removed -> unmounted from {}:",
                    B::FS_NAME,
                    key.1,
                    mc.letter
                );
            }
        }
    }

    /// Read a small sector-aligned window at `offset` from
    /// `disk_path` and run `B::detect` on it. 4 KiB covers both
    /// 512-byte and 4Kn devices and is large enough for any
    /// superblock magic any FS we support uses.
    fn probe_at_offset<B: FsBackend>(disk_path: &str, offset: u64) -> Result<bool> {
        use crate::device::{BlockSource, FileSource};
        let src = FileSource::open(Path::new(disk_path))
            .with_context(|| format!("opening {disk_path} for probe"))?;
        let mut buf = vec![0u8; 4096];
        if src.read_at(offset, &mut buf).is_err() {
            return Ok(false);
        }
        Ok(B::detect(&buf))
    }

    /// Pick a free drive letter and spawn `<current_exe> mount
    /// <disk_path> --drive <X:> [--part <N>]` (omitting `--part`
    /// when `n == 0` to signal whole-disk / no-partition-table).
    /// Tracks the spawned child in `State.mounts` keyed by
    /// `(disk_path, n)`.
    fn spawn_partition_mount<B: FsBackend>(state: &Mutex<State>, disk_path: &str, n: usize) {
        let mount_letter = match probe::pick_drive_letter() {
            Some(c) => c,
            None => {
                eprintln!(
                    "[{}] {disk_path}#part{n}: filesystem detected but no free drive letter",
                    B::FS_NAME
                );
                return;
            }
        };

        println!(
            "[{}] {} detected on {disk_path}#part{n} -> mounting on {mount_letter}:",
            B::FS_NAME,
            B::FS_NAME
        );

        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[{}] current_exe() failed: {e}", B::FS_NAME);
                return;
            }
        };
        let drive_arg = format!("{mount_letter}:");
        let mut cmd = Command::new(&exe);
        cmd.arg("mount").arg(disk_path).arg("--drive").arg(&drive_arg);
        if n > 0 {
            cmd.arg("--part").arg(n.to_string());
        }
        match cmd.spawn() {
            Ok(child) => {
                if let Ok(mut st) = state.lock() {
                    st.mounts.insert(
                        (disk_path.to_string(), n),
                        MountedChild {
                            child,
                            letter: mount_letter,
                        },
                    );
                }
            }
            Err(e) => {
                eprintln!(
                    "[{}] spawn `{} mount {disk_path} --drive {drive_arg}{}` failed: {e}",
                    B::FS_NAME,
                    exe.display(),
                    if n > 0 {
                        format!(" --part {n}")
                    } else {
                        String::new()
                    }
                );
            }
        }
    }
}
