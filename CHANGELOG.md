# Changelog

Notable changes to `winfsp-fs-skeleton`, newest first. This is a `0.x` crate,
so the **minor** is the compatibility boundary: a minor bump may break API, a
patch never does.

## [Unreleased]

## [0.2.1] — 2026-09-04

Patch rather than minor: purely additive, so nothing that compiled
against 0.2.0 stops compiling.

### Added

- **`filetime_to_unix`** — the inverse of `unix_to_filetime`, which the
  drivers needed and had been writing themselves. Their copies were wrong
  in both directions: they returned `None` for any FILETIME predating the
  Unix epoch, which the caller reads as WinFSP's "leave unchanged"
  sentinel and so silently discards a time the user asked for, and they
  clamped anything past 2106 because they returned `u32` seconds.

  Here `None` means only the zero sentinel, and the return type is the
  signed `Timestamp`, so a pre-1970 time converts rather than vanishing.


## [0.2.0] — 2026-09-04

Two new public modules. Together they are what a driver needs in order to stop
being written against one specific filesystem reader, and the split between
them is deliberate: `translate` absorbs differences in how a value is
**represented**, `reader` absorbs differences in the **shape of the calls**.

### Added

- **`translate` — the POSIX-to-Windows conversions, written once.** Both
  drivers carried their own copies of `unix_to_filetime` and
  `winpath_to_unix`; the latter was byte-for-byte identical in both, which is
  the clearest possible case for lifting it rather than writing it a third
  time.

  It also carries the types that make those conversions safe to call.
  `NodeKind` exists because a naive `mode_to_attributes(mode: u16)` would have
  misclassified every EROFS directory: EROFS's `mode` carries no type bits, so
  a shared helper that reads them would have been silently wrong for one of
  its two callers while looking right.

- **`reader` — `FsReader` and `FsWriter`.** `FsBackend` is four constants and
  `detect()`, which covers the parts of a driver that only need to *name* the
  filesystem. It does not cover `mount.rs`, so each driver called its own
  reader's API directly at around thirty sites.

  `FsReader::Node` is an associated type, and that is the whole design.
  Whatever a reader must carry between "I resolved this path" and "now read
  from it" lives there, opaque to the skeleton — EROFS puts an inode in it,
  XFS puts an inode *and* the raw inode fork, because an XFS inode keeps its
  extents and inline data there. An abstraction that refused to carry those
  bytes would be wrong about XFS.

  Measured rather than assumed: copying `erofs-win-driver` into an XFS driver
  and renaming every identifier left a tree eight compile errors from
  building, and five of the eight were this.

  Reading and writing are separate traits because the drivers are — EROFS
  mounts a read-only format and gets its writability from an in-memory
  overlay, ext4 writes to the device. One trait would force the read-only
  driver to implement mutating methods it could only answer with an error.

- **`writable(&fs)?`**, the single point at which a mount's *permission* is
  checked before its *capability* is used. Implementing `FsWriter` says this
  reader has a write path; `is_writable()` says this mount was opened for
  writing. Both questions have to be asked, and `is_writable` has no default
  so that neither can be answered by accident.

### Fixed

- **`unix_to_filetime` takes signed seconds.** It took `u64`, and the comment
  explained at length why 64 bits rather than 32 while saying nothing about
  the sign — which is the other half. A Unix timestamp before 1970 is
  negative, and FILETIME represents it perfectly well: its epoch is 1601, 369
  years earlier than Unix's, so a negative Unix second is still a positive
  FILETIME. With an unsigned parameter the loss happened at the call site,
  before the function ran.

  `Timestamp::to_filetime` had been compensating with a hand-written branch
  for the negative case — a second implementation of one conversion. It is
  gone, and removing it is what made the tests honest: with the branch in
  place a mutation to the shared function was invisible, because the special
  case caught it first.

### Changed

- **The toolchain is pinned at 1.95.0.** CI ran `dtolnay/rust-toolchain@stable`
  with no `rust-toolchain.toml` to override it, so the compiler moved whenever
  upstream shipped. It did: clippy 1.98 added a lint that fires on
  `decode_utf16le` — code nobody touched — and `-D warnings` turned it into a
  hard failure on an unrelated pull request.

  Not a free choice: this crate is statically linked into the win-drivers
  alongside the `am-fs-*` drivers, and two crates built by different
  toolchains link two copies of `_rust_eh_personality`.

## [0.1.1] — 2026-05-29

Note that `Cargo.toml` was never bumped for this release and still read
`0.1.0`; the tag is the only record. Corrected from 0.2.0 onwards.

### Added

- Multi-OS check/test/clippy/fmt CI.
- `customize.sh` for consumer onboarding, and a Getting Started guide.
- Coverage for `device.rs` alignment and device-interface name parsing.

### Fixed

- **The `LINUX_FS` GUID's mixed-endian encoding**, which mis-identified Linux
  partitions.
- **Whole-disk probing before auto-mounting unpartitioned media**, so a
  superfloppy is recognised rather than skipped.
- `pub mod service` is exposed unconditionally, with the stub `run()` handling
  the no-feature and non-Windows builds.
- Watch and service unavailability messages agree with each other.
- The `Win32_Graphics_Gdi` feature is declared, which `WNDCLASSW` and
  `RegisterClassW` need.

## [0.1.0]

### Added

- Initial release, extracted from `ext4-win-driver`: the SCM dispatcher, the
  disk-arrival event pump, the partition walker, raw-device I/O with sector
  alignment, and drive-letter selection — the parts of a WinFSP driver that
  have nothing to do with which filesystem is being hosted.

[Unreleased]: https://github.com/antimatter-studios/winfsp-fs-skeleton/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/antimatter-studios/winfsp-fs-skeleton/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/antimatter-studios/winfsp-fs-skeleton/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/antimatter-studios/winfsp-fs-skeleton/releases/tag/v0.1.1
