//! Windows userspace-filesystem-driver skeleton.
//!
//! This crate lifts the platform plumbing every WinFsp-based filesystem
//! driver duplicates -- SCM service dispatcher, disk-arrival event
//! pump, partition-table walker, raw-device I/O with sector
//! alignment, drive-letter selection -- behind a single trait. A
//! consumer crate plugs in three pieces of FS-specific state plus a
//! magic-byte detection function and reuses everything else.
//!
//! ## Skeleton vs consumer split
//!
//! The skeleton owns:
//!
//! - [`watch`] -- foreground variant of the auto-mount watcher
//!   (interactive `ext4 watch` or equivalent). Spawns child mount
//!   processes via `Command::new(current_exe()) ... mount ... --part N`.
//! - [`service`] (feature-gated) -- SCM service variant. Hands control
//!   to the SCM dispatcher, listens for disk-class device-interface
//!   arrivals, walks the partition table directly, asks
//!   WinFsp.Launcher to spawn per-partition mounts in the active
//!   console session.
//! - [`partition`] -- MBR/GPT parsing. Pure logic, FS-agnostic.
//! - [`device`] -- `BlockSource` trait + `FileSource` impl with
//!   sector-aligned raw-disk reads on Windows.
//! - [`probe`] -- drive-letter selection, `GUID_DEVINTERFACE_DISK`
//!   constant, `DEV_BROADCAST_DEVICEINTERFACE_W` payload parsing.
//!
//! The consumer owns:
//!
//! - The [`FsBackend`] impl: four constants + a single `detect`
//!   function that reads the FS's superblock magic from a byte slice.
//! - The actual WinFsp `FileSystemContext` (read/write/getinfo
//!   callbacks against the underlying FS library).
//! - CLI subcommands that interact directly with the filesystem
//!   (`info`, `ls`, `stat`, `cat`, `tree`, etc.).
//! - The MSI + Burn-bundle wxs files (templated; see
//!   `installer/README.md` in this repo for the parameters to fill in).
//!
//! ## Minimal consumer wiring
//!
//! ```ignore
//! use winfsp_fs_skeleton::FsBackend;
//!
//! struct Ext4Backend;
//! impl FsBackend for Ext4Backend {
//!     const FS_NAME: &'static str = "ext4";
//!     const SERVICE_NAME: &'static str = "ExtFsWatcher";
//!     const LAUNCHER_SERVICE_CLASS: &'static str = "ext4-mount";
//!     const FILE_EXTENSION: &'static str = "img";
//!
//!     fn detect(bytes: &[u8]) -> bool {
//!         // ext4 superblock magic at byte offset 1024+0x38.
//!         const OFF: usize = 1024 + 0x38;
//!         bytes.len() >= OFF + 2 && bytes[OFF] == 0x53 && bytes[OFF + 1] == 0xEF
//!     }
//! }
//!
//! fn main() -> anyhow::Result<()> {
//!     // your CLI dispatch here -- the service / watch arms call into
//!     // the skeleton:
//!     //   Cmd::Watch   => winfsp_fs_skeleton::watch::run::<Ext4Backend>(),
//!     //   Cmd::Service => winfsp_fs_skeleton::service::run::<Ext4Backend>(),
//!     # Ok(())
//! }
//! ```

#![allow(dead_code)] // many helpers are only used on Windows

pub mod device;
pub mod partition;
pub mod probe;

pub mod watch;

#[cfg(feature = "service")]
pub mod service;

/// The seam every consumer plugs into. Four constants identify the
/// consumer to Windows + WinFsp.Launcher, plus a `detect` function
/// the skeleton calls when probing raw bytes for the consumer's
/// filesystem magic.
///
/// Values are compile-time so the skeleton can use them in
/// service-name registration, log lines, launchctl invocations, etc.
/// without runtime indirection.
pub trait FsBackend {
    /// Short, lowercase name of the filesystem. Used in log lines and
    /// in error messages. Examples: `"ext4"`, `"ntfs"`, `"qcow2"`.
    const FS_NAME: &'static str;

    /// Windows Service name registered with SCM via the consumer's
    /// MSI. Must be unique per consumer so concurrent skeleton-based
    /// services on the same host don't collide. Examples:
    /// `"ExtFsWatcher"`, `"NtfsWatcher"`.
    const SERVICE_NAME: &'static str;

    /// Service-class name registered with WinFsp.Launcher (in
    /// `HKLM\SOFTWARE\WOW6432Node\WinFsp\Services\<class>`). Used as
    /// the `<class>` argument to launchctl. Conventionally
    /// `"<fs-name>-mount"`, e.g. `"ext4-mount"`.
    const LAUNCHER_SERVICE_CLASS: &'static str;

    /// Default file extension for the consumer's right-click "Mount
    /// as <FS>" verb. Without the leading dot, e.g. `"img"`,
    /// `"vhd"`, `"qcow2"`. The MSI's HKCR\\SystemFileAssociations
    /// registration uses this.
    const FILE_EXTENSION: &'static str;

    /// Probe a raw byte slice for this filesystem's superblock magic.
    /// The skeleton calls this with the first ~4 KiB of every newly-
    /// arrived partition (read at the partition's on-disk offset,
    /// sector-aligned). Implementations should be quick + total --
    /// don't allocate, don't fail; return `false` for "doesn't look
    /// like our FS."
    fn detect(bytes: &[u8]) -> bool;
}
