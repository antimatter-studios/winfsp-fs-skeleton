# winfsp-fs-skeleton

Reusable skeleton for Windows userspace filesystem drivers built on
[WinFsp](https://github.com/winfsp/winfsp). Plug in your filesystem's
detection magic + a `FileSystemContext` impl; reuse the SCM service,
disk-arrival watcher, partition walker, raw-device I/O, installer +
CI templates, and winget submission flow.

The first consumer is
[ext4-win-driver](https://github.com/antimatter-studios/ext4-win-driver);
the skeleton was extracted out of it once that driver was a
working end-to-end reference.

## What the skeleton owns

- **SCM service** (`service::run<B>`) -- the auto-mount watcher
  invoked by `sc start <SERVICE_NAME>`. Subscribes to disk-class
  device-interface arrivals, walks the partition table directly,
  asks WinFsp.Launcher to spawn per-partition mounts in the active
  console session.
- **Foreground watcher** (`watch::run<B>`) -- same logic but spawns
  a child process via `Command::spawn` instead of going through
  WinFsp.Launcher. Useful for development.
- **Partition table parsing** (`partition`) -- MBR + GPT.
- **Raw-device I/O** (`device`) -- `BlockSource` trait + `FileSource`
  with sector-aligned reads on Windows.
- **Helpers** (`probe`) -- drive-letter selection, the
  `GUID_DEVINTERFACE_DISK` constant, `DEV_BROADCAST_DEVICEINTERFACE_W`
  payload parsing.
- **Templates** (under `templates/`) -- WiX MSI + Burn bootstrapper,
  GH Actions release workflow (x64 + arm64), winget manifest skeleton.

## What the consumer owns

- The [`FsBackend`] impl: four constants identifying the consumer
  to Windows + WinFsp.Launcher, plus a `detect` function that reads
  the FS's superblock magic from a byte slice. Maybe 20 lines.
- The actual WinFsp `FileSystemContext` (read/write/getinfo
  callbacks against the underlying FS library). This is the
  GPL-licensed bit that links winfsp-rs.
- CLI subcommands that interact directly with the filesystem
  (`info`, `ls`, `stat`, `cat`, `tree`, etc.).
- A copy of the templates from `templates/`, customised with the
  consumer's product name, package GUIDs, and version.

## Minimal consumer wiring

```rust
use winfsp_fs_skeleton::FsBackend;

struct Ext4Backend;
impl FsBackend for Ext4Backend {
    const FS_NAME: &'static str = "ext4";
    const SERVICE_NAME: &'static str = "ExtFsWatcher";
    const LAUNCHER_SERVICE_CLASS: &'static str = "ext4-mount";
    const FILE_EXTENSION: &'static str = "img";

    fn detect(bytes: &[u8]) -> bool {
        // ext4 superblock magic at byte offset 1024 + 0x38.
        const OFF: usize = 1024 + 0x38;
        bytes.len() >= OFF + 2
            && bytes[OFF] == 0x53
            && bytes[OFF + 1] == 0xEF
    }
}

fn main() -> anyhow::Result<()> {
    // Your CLI dispatch -- the service / watch arms call into the
    // skeleton:
    //   Cmd::Watch   => winfsp_fs_skeleton::watch::run::<Ext4Backend>(),
    //   Cmd::Service => winfsp_fs_skeleton::service::run::<Ext4Backend>(),
    Ok(())
}
```

## Licensing

GPL-3.0-or-later. The skeleton's own dependencies (anyhow, ctrlc,
windows-sys, windows-service) are all permissively licensed, so the
GPL boundary is at the consumer's link line where `winfsp-rs` is
pulled in.

The skeleton license is **deliberately the same as WinFsp** -- any
consumer ends up GPL-3 because it links winfsp-rs anyway, so making
the skeleton more permissive would only add a license-boundary
hazard for future contributors without buying anyone real
flexibility.

## Minimum supported Rust version

1.70 -- needed for `std::sync::OnceLock`, which the service-mode
dispatcher uses to stash backend-supplied constants between the SCM
FFI shim and the rest of the module. We don't enforce MSRV in CI;
bumping it should be a deliberate, advertised change.

## Vendoring

Add the skeleton as a git submodule and a path-dependency in your
consumer crate's `Cargo.toml`:

```sh
git submodule add https://github.com/antimatter-studios/winfsp-fs-skeleton vendor/winfsp-fs-skeleton
```

```toml
# consumer Cargo.toml
[dependencies]
winfsp-fs-skeleton = { path = "vendor/winfsp-fs-skeleton", features = ["service"] }
```

Then copy `templates/installer/`, `templates/release.yml`, and
`templates/winget/` into your consumer repo and substitute the
parameters listed in `templates/README.md`.
