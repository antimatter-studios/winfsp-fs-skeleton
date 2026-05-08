//! Windows Service variant of the auto-mount watcher.
//!
//! Same disk-arrival logic as [`crate::watch`], but:
//!   - registered with the SCM via `windows-service::service_dispatcher`,
//!     so the consumer's binary can be started by
//!     `sc start <SERVICE_NAME>` / auto-started at boot;
//!   - mounts are launched via WinFsp.Launcher's
//!     `launchctl-<arch>.exe` rather than `Command::spawn` so the
//!     WinFsp service spawns the mount in the active console session
//!     (where Explorer can see it), not session 0.
//!
//! Service-class name registered with WinFsp.Launcher comes from
//! [`crate::FsBackend::LAUNCHER_SERVICE_CLASS`]. When a partition we
//! recognise arrives we run:
//!
//! ```text
//! launchctl-<arch>.exe start <CLASS> <letter> <letter:> <disk_path> <part_n>
//! ```
//!
//! and on removal:
//!
//! ```text
//! launchctl-<arch>.exe stop <CLASS> <letter>
//! ```
//!
//! The "service-name when calling Launcher" arg (`<letter>`) is
//! arbitrary but must be unique per concurrent mount -- using the
//! drive letter is convenient and lines up with what we'd want to see
//! in `sc query`.
//!
//! ## Parameterisation
//!
//! `define_windows_service!` builds a fixed-signature FFI shim, so we
//! can't carry the consumer's `B: FsBackend` type parameter through
//! it. Instead, [`run`] -- which IS generic -- writes B's constants
//! and `B::detect` function pointer into module-level `OnceLock`
//! statics before calling `service_dispatcher::start`. Everything
//! downstream (the FFI, `service_main`, the wndproc) reads from
//! those statics. A single binary runs only one backend at a time,
//! so the global state is fine.

#![allow(dead_code)]

use anyhow::Result;

use crate::FsBackend;

#[cfg(all(target_os = "windows", feature = "service"))]
pub fn run<B: FsBackend>() -> Result<()> {
    // Stash B's static data in module-level slots so the FFI shim
    // (which can't be generic) can reach them.
    imp::FS_NAME.set(B::FS_NAME).ok();
    imp::SERVICE_NAME.set(B::SERVICE_NAME).ok();
    imp::LAUNCHER_SERVICE_CLASS.set(B::LAUNCHER_SERVICE_CLASS).ok();
    imp::DETECT.set(B::detect as fn(&[u8]) -> bool).ok();
    imp::run()
}

#[cfg(not(all(target_os = "windows", feature = "service")))]
pub fn run<B: FsBackend>() -> Result<()> {
    eprintln!(
        "[{fs}] service is unavailable in this build. Reasons:\n  \
         - target is not Windows ({os})\n  \
         - `service` feature is not enabled (feature = {feat})\n\
         Rebuild on a Windows host with --features mount,service to enable.",
        fs = B::FS_NAME,
        os = std::env::consts::OS,
        feat = cfg!(feature = "service"),
    );
    Ok(())
}

#[cfg(all(target_os = "windows", feature = "service"))]
mod imp {
    use anyhow::{Context, Result, anyhow};
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::ptr;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{
        self, ServiceControlHandlerResult, ServiceStatusHandle,
    };
    use windows_service::service_dispatcher;
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

    use crate::probe;

    // ---------------------------------------------------------------------
    // Backend-supplied data, set once by the public `run<B>()`.
    // ---------------------------------------------------------------------

    pub(super) static FS_NAME: OnceLock<&'static str> = OnceLock::new();
    pub(super) static SERVICE_NAME: OnceLock<&'static str> = OnceLock::new();
    pub(super) static LAUNCHER_SERVICE_CLASS: OnceLock<&'static str> = OnceLock::new();
    pub(super) static DETECT: OnceLock<fn(&[u8]) -> bool> = OnceLock::new();

    fn fs_name() -> &'static str { FS_NAME.get().copied().unwrap_or("fs") }
    fn service_name() -> &'static str { SERVICE_NAME.get().copied().unwrap_or("WinFspFsWatcher") }
    fn launcher_class() -> &'static str { LAUNCHER_SERVICE_CLASS.get().copied().unwrap_or("fs-mount") }
    fn detect(bytes: &[u8]) -> bool {
        match DETECT.get() {
            Some(f) => f(bytes),
            None => false,
        }
    }

    // ---------------------------------------------------------------------
    // Window class. Wide-encoded inline.
    // ---------------------------------------------------------------------

    const CLASS_NAME: &[u16] = &[
        b'w' as u16, b'f' as u16, b's' as u16, b'_' as u16, b's' as u16, b'k' as u16,
        b'e' as u16, b'l' as u16, b'_' as u16, b's' as u16, b'v' as u16, b'c' as u16,
        0,
    ];

    // ---------------------------------------------------------------------
    // SCM entry. The FFI shim is non-generic; it dispatches via the
    // OnceLock statics above.
    // ---------------------------------------------------------------------

    windows_service::define_windows_service!(ffi_service_main, service_main);

    pub fn run() -> Result<()> {
        service_dispatcher::start(service_name(), ffi_service_main)
            .context("service_dispatcher::start (run as a service, not directly)")
    }

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = service_main_inner() {
            eprintln!("[{}] fatal: {e:#}", fs_name());
            if let Some(handle) = status_handle().get().cloned() {
                let _ = handle.set_service_status(ServiceStatus {
                    service_type: ServiceType::OWN_PROCESS,
                    current_state: ServiceState::Stopped,
                    controls_accepted: ServiceControlAccept::empty(),
                    exit_code: ServiceExitCode::ServiceSpecific(1),
                    checkpoint: 0,
                    wait_hint: Duration::default(),
                    process_id: None,
                });
            }
        }
    }

    fn status_handle() -> &'static OnceLock<ServiceStatusHandle> {
        static H: OnceLock<ServiceStatusHandle> = OnceLock::new();
        &H
    }

    fn service_main_inner() -> Result<()> {
        let main_thread = unsafe { GetCurrentThreadId() };

        let event_handler = move |control| -> ServiceControlHandlerResult {
            match control {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    if let Ok(mut s) = state().lock() {
                        s.shutting_down = true;
                    }
                    unsafe {
                        PostThreadMessageW(main_thread, WM_QUIT, 0, 0);
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let handle = service_control_handler::register(service_name(), event_handler)
            .context("service_control_handler::register")?;
        let _ = status_handle().set(handle.clone());

        handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::StartPending,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        })?;

        let state_ptr = state() as *const Mutex<State> as isize;
        let pump = unsafe { Pump::open(state_ptr)? };

        handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        unsafe { pump.run() };
        drop(pump);

        let mut st = state().lock().unwrap();
        st.shutting_down = true;
        let drained: Vec<((String, usize), char)> = st.mounts.drain().collect();
        drop(st);
        let launcher = LauncherClient::locate().ok();
        for ((dev, n), letter) in drained {
            if let Some(l) = &launcher {
                if let Err(e) = l.stop(letter) {
                    eprintln!(
                        "[{}] launchctl stop {letter}: {e:#} ({dev}#part{n})",
                        fs_name()
                    );
                }
            }
            println!(
                "[{}] {dev}#part{n} -> launchctl stop {} {letter} (shutdown)",
                fs_name(),
                launcher_class()
            );
        }

        handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        Ok(())
    }

    // ---------------------------------------------------------------------
    // Pump: window + notification registration, RAII teardown.
    // ---------------------------------------------------------------------

    struct Pump {
        hwnd: HWND,
        dev_handle: *mut std::ffi::c_void,
        hinstance: windows_sys::Win32::Foundation::HINSTANCE,
    }

    impl Pump {
        unsafe fn open(state_ptr: isize) -> Result<Self> {
            let hinstance = GetModuleHandleW(ptr::null());

            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(wnd_proc);
            wc.hInstance = hinstance;
            wc.lpszClassName = CLASS_NAME.as_ptr();
            let atom = RegisterClassW(&wc);
            if atom == 0 {
                let err = std::io::Error::last_os_error();
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

            Ok(Pump {
                hwnd,
                dev_handle,
                hinstance,
            })
        }

        unsafe fn run(&self) {
            let mut msg: MSG = std::mem::zeroed();
            loop {
                let r = GetMessageW(&mut msg, ptr::null_mut(), 0, 0);
                if r == 0 {
                    break;
                }
                if r == -1 {
                    eprintln!(
                        "[{}] GetMessageW error: {}",
                        fs_name(),
                        std::io::Error::last_os_error()
                    );
                    break;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    impl Drop for Pump {
        fn drop(&mut self) {
            unsafe {
                UnregisterDeviceNotification(self.dev_handle);
                DestroyWindow(self.hwnd);
                UnregisterClassW(CLASS_NAME.as_ptr(), self.hinstance);
            }
        }
    }

    // ---------------------------------------------------------------------
    // State + wndproc + arrival/removal handlers
    // ---------------------------------------------------------------------

    fn state() -> &'static Mutex<State> {
        static STATE: OnceLock<Mutex<State>> = OnceLock::new();
        STATE.get_or_init(|| Mutex::new(State::default()))
    }

    #[derive(Default)]
    struct State {
        mounts: HashMap<(String, usize), char>,
        shutting_down: bool,
    }

    unsafe extern "system" fn wnd_proc(
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
                            handle_arrival(state, &disk_path);
                        } else {
                            handle_removal(state, &disk_path);
                        }
                    }
                }
            }
            return 1;
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    fn handle_arrival(state: &Mutex<State>, disk_path: &str) {
        if state.lock().map(|s| s.shutting_down).unwrap_or(true) {
            return;
        }
        if active_console_session().is_none() {
            eprintln!(
                "[{}] {disk_path}: no active console session, deferring mount",
                fs_name()
            );
            return;
        }

        let parts = match crate::partition::list(Path::new(disk_path)) {
            Ok(v) => v,
            Err(e) => {
                if format!("{e:#}").contains("no MBR signature") {
                    // Probe the whole disk at offset 0 for the
                    // consumer's FS before spawning. Otherwise
                    // every unpartitioned non-FS disk triggers a
                    // launchctl spawn that fails downstream.
                    match probe_at_offset(disk_path, 0) {
                        Ok(true) => spawn_partition_mount(state, disk_path, 0),
                        Ok(false) => {}
                        Err(e) => eprintln!(
                            "[{}] probe {disk_path} whole-disk: {e:#}",
                            fs_name()
                        ),
                    }
                } else {
                    eprintln!(
                        "[{}] partition::list({disk_path}) failed: {e:#}",
                        fs_name()
                    );
                }
                return;
            }
        };

        for (idx, part) in parts.iter().enumerate() {
            let n = idx + 1;
            let part_offset = part.start_lba * 512;
            match probe_at_offset(disk_path, part_offset) {
                Ok(true) => spawn_partition_mount(state, disk_path, n),
                Ok(false) => {}
                Err(e) => eprintln!("[{}] probe {disk_path} part {n}: {e:#}", fs_name()),
            }
        }
    }

    fn handle_removal(state: &Mutex<State>, disk_path: &str) {
        let stops: Vec<((String, usize), char)> = {
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
            keys.into_iter()
                .filter_map(|k| st.mounts.remove(&k).map(|v| (k, v)))
                .collect()
        };
        if stops.is_empty() {
            return;
        }
        let launcher = match LauncherClient::locate() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[{}] cannot locate launchctl: {e:#}", fs_name());
                return;
            }
        };
        for ((_, n), letter) in stops {
            match launcher.stop(letter) {
                Ok(()) => println!(
                    "[{}] {disk_path}#part{n} removed -> launchctl stop {} {letter}",
                    fs_name(),
                    launcher_class()
                ),
                Err(e) => eprintln!("[{}] launchctl stop {letter}: {e:#}", fs_name()),
            }
        }
    }

    fn probe_at_offset(disk_path: &str, offset: u64) -> Result<bool> {
        use crate::device::{BlockSource, FileSource};
        let src = FileSource::open(Path::new(disk_path))
            .with_context(|| format!("opening {disk_path} for probe"))?;
        let mut buf = vec![0u8; 4096];
        if src.read_at(offset, &mut buf).is_err() {
            return Ok(false);
        }
        Ok(detect(&buf))
    }

    fn spawn_partition_mount(state: &Mutex<State>, disk_path: &str, n: usize) {
        let mount_letter = match probe::pick_drive_letter() {
            Some(c) => c,
            None => {
                eprintln!(
                    "[{}] {disk_path}#part{n}: filesystem detected but no free drive letter",
                    fs_name()
                );
                return;
            }
        };

        let launcher = match LauncherClient::locate() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[{}] cannot locate launchctl: {e:#}", fs_name());
                return;
            }
        };

        println!(
            "[{}] {} detected on {disk_path}#part{n} -> launchctl start {} {mount_letter} {disk_path} {n}",
            fs_name(),
            fs_name(),
            launcher_class()
        );

        match launcher.start(mount_letter, disk_path, n) {
            Ok(()) => {
                if let Ok(mut st) = state.lock() {
                    st.mounts.insert((disk_path.to_string(), n), mount_letter);
                }
            }
            Err(e) => eprintln!("[{}] launchctl start failed: {e:#}", fs_name()),
        }
    }

    // ---------------------------------------------------------------------
    // Active-console-session helper
    // ---------------------------------------------------------------------

    fn active_console_session() -> Option<u32> {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn WTSGetActiveConsoleSessionId() -> u32;
        }
        let id = unsafe { WTSGetActiveConsoleSessionId() };
        if id == 0xFFFF_FFFF { None } else { Some(id) }
    }

    // ---------------------------------------------------------------------
    // LauncherClient -- locate launchctl-<arch>.exe + invoke it.
    // ---------------------------------------------------------------------

    struct LauncherClient {
        exe: PathBuf,
    }

    impl LauncherClient {
        fn locate() -> Result<Self> {
            let install_dir = winfsp_install_dir().context(
                "locating WinFsp install dir (HKLM\\SOFTWARE\\WOW6432Node\\WinFsp\\InstallDir)",
            )?;
            let exe = install_dir.join("bin").join(launchctl_exe_name());
            if !exe.exists() {
                return Err(anyhow!("launchctl not found at {}", exe.display()));
            }
            Ok(LauncherClient { exe })
        }

        fn start(&self, letter: char, disk_path: &str, partition: usize) -> Result<()> {
            let letter_s = format!("{letter}");
            // `launchctl-<arch> start <ClassName> <InstanceName>
            // [TemplateArgs...]`. WinFsp.Launcher substitutes
            // template args into the registered CommandLine starting
            // at `%1` -- the InstanceName itself isn't substitutable
            // -- so we pass the drive letter twice: once as
            // InstanceName (so `sc query` shows distinct services
            // for concurrent mounts), once as the first template arg.
            //   %1 = drive letter (e.g. F:)
            //   %2 = disk device path
            //   %3 = 1-indexed partition number, or "0" for "no
            //        partition table; treat the whole device as the
            //        FS"
            // The CommandLine the consumer registers must match this
            // shape, e.g. `mount %2 --drive %1 --part %3`.
            let part_s = format!("{partition}");
            let drive_arg = format!("{letter}:");
            let status = Command::new(&self.exe)
                .arg("start")
                .arg(launcher_class())
                .arg(&letter_s)
                .arg(&drive_arg)
                .arg(disk_path)
                .arg(&part_s)
                .status()
                .with_context(|| format!("running {}", self.exe.display()))?;
            if !status.success() {
                return Err(anyhow!("{} start exited with {status}", self.exe.display()));
            }
            Ok(())
        }

        fn stop(&self, letter: char) -> Result<()> {
            let letter_s = format!("{letter}");
            let status = Command::new(&self.exe)
                .arg("stop")
                .arg(launcher_class())
                .arg(&letter_s)
                .status()
                .with_context(|| format!("running {}", self.exe.display()))?;
            if !status.success() {
                return Err(anyhow!("{} stop exited with {status}", self.exe.display()));
            }
            Ok(())
        }
    }

    fn launchctl_exe_name() -> &'static str {
        if cfg!(target_arch = "x86_64") {
            "launchctl-x64.exe"
        } else if cfg!(target_arch = "aarch64") {
            "launchctl-a64.exe"
        } else if cfg!(target_arch = "x86") {
            "launchctl-x86.exe"
        } else {
            "launchctl-x64.exe"
        }
    }

    fn winfsp_install_dir() -> Result<PathBuf> {
        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::System::Registry::{
            HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_WOW64_32KEY, REG_SZ, RegCloseKey,
            RegOpenKeyExW, RegQueryValueExW,
        };

        let subkey = wide_z("SOFTWARE\\WinFsp");
        let value_name = wide_z("InstallDir");

        let mut hkey: HKEY = ptr::null_mut();
        let r = unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                subkey.as_ptr(),
                0,
                KEY_QUERY_VALUE | KEY_WOW64_32KEY,
                &mut hkey,
            )
        };
        if r != ERROR_SUCCESS {
            return Err(anyhow!(
                "RegOpenKeyExW(HKLM\\SOFTWARE\\WinFsp) failed: code {r}"
            ));
        }

        let mut ty: u32 = 0;
        let mut size: u32 = 0;
        let r = unsafe {
            RegQueryValueExW(
                hkey,
                value_name.as_ptr(),
                ptr::null_mut(),
                &mut ty,
                ptr::null_mut(),
                &mut size,
            )
        };
        if r != ERROR_SUCCESS {
            unsafe { RegCloseKey(hkey) };
            return Err(anyhow!(
                "RegQueryValueExW(InstallDir) size query failed: code {r}"
            ));
        }
        if ty != REG_SZ {
            unsafe { RegCloseKey(hkey) };
            return Err(anyhow!("InstallDir is type {ty}, expected REG_SZ"));
        }

        let mut buf: Vec<u16> = vec![0u16; (size as usize / 2) + 1];
        let mut size2 = size;
        let r = unsafe {
            RegQueryValueExW(
                hkey,
                value_name.as_ptr(),
                ptr::null_mut(),
                &mut ty,
                buf.as_mut_ptr() as *mut u8,
                &mut size2,
            )
        };
        unsafe { RegCloseKey(hkey) };
        if r != ERROR_SUCCESS {
            return Err(anyhow!("RegQueryValueExW(InstallDir) failed: code {r}"));
        }

        while buf.last() == Some(&0) {
            buf.pop();
        }
        let s = String::from_utf16_lossy(&buf);
        Ok(PathBuf::from(s))
    }

    fn wide_z(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
}
