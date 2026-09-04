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

## Getting started

End-to-end: empty directory to a binary that's auto-mounting on disk
arrival. The `mount` subcommand stays a stub here; replacing it with a
real winfsp-rs `FileSystemContext` is the work the skeleton can't do
for you.

### 1. Prerequisites

- Rust 1.70+ (`rustup default stable` is fine).
- [WinFsp](https://winfsp.dev/) installed on the Windows host you'll
  test on.
- A Windows host for runtime (the SCM dispatcher + disk-arrival pump
  are Windows-only). `customize.sh` itself runs on macOS/Linux/WSL --
  scaffolding from a Unix workstation and testing on Windows is the
  expected loop.

### 2. Bootstrap the consumer repo

```sh
# The skeleton is checked out as a SIBLING of the driver, not inside
# it. Every existing driver works this way -- the pin lives in the
# consumer's chores.yml and `chore siblings` fetches it -- so one copy
# on a machine serves all of them. A submodule would pin a copy per
# consumer, which is how the same crate ends up on several versions at
# once with nothing reporting it.
git clone https://github.com/antimatter-studios/winfsp-fs-skeleton
mkdir myfs-win-driver && cd myfs-win-driver
git init

../winfsp-fs-skeleton/templates/customize.sh \
  --target . \
  --name myfs-win-driver \
  --fs-name myfs \
  --service-name MyFsWatcher \
  --launcher-class myfs-mount \
  --file-extension img \
  --publisher-id YourOrg \
  --publisher-name "Your Org" \
  --manufacturer "Your Name" \
  --winfsp-version 2.1.25156 \
  --winfsp-sha256 073a70e00f77423e34bed98b86e600def93393ba5822204fac57a29324db9f7a
```

That drops a customised `installer/`, `.github/workflows/release.yml`,
and `winget/` into your repo. `--target .` requires the directory to
be empty other than `.git/`, which is why we run before `cargo new`.
See [`templates/README.md`](templates/README.md) for the full
substitution table and per-flag reference.

### 3. Add Cargo.toml + src/

`customize.sh` deliberately doesn't touch your crate manifest -- write
those next:

```toml
# Cargo.toml
[package]
name = "myfs-win-driver"
version = "0.1.0"
edition = "2021"

[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
winfsp-fs-skeleton = { path = "../winfsp-fs-skeleton", features = ["service"] }
```

```rust
// src/main.rs
use clap::{Parser, Subcommand};
use winfsp_fs_skeleton::FsBackend;

struct MyFs;
impl FsBackend for MyFs {
    const FS_NAME: &'static str = "myfs";
    const SERVICE_NAME: &'static str = "MyFsWatcher";
    const LAUNCHER_SERVICE_CLASS: &'static str = "myfs-mount";
    const FILE_EXTENSION: &'static str = "img";

    fn detect(bytes: &[u8]) -> bool {
        // Replace with your FS's superblock magic check. ext4 lives at
        // 1024 + 0x38; FAT32 at offset 0x52; etc.
        bytes.starts_with(b"MYFS")
    }
}

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Foreground auto-mount watcher (development).
    Watch,
    /// SCM service variant -- started by `sc start MyFsWatcher`.
    Service,
    /// Mount one partition. Spawned by Watch/Service per detected hit.
    Mount {
        disk: String,
        #[arg(long)] drive: String,
        #[arg(long)] part: Option<usize>,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Watch   => winfsp_fs_skeleton::watch::run::<MyFs>(),
        Cmd::Service => winfsp_fs_skeleton::service::run::<MyFs>(),
        Cmd::Mount { disk, drive, part } => mount(&disk, &drive, part),
    }
}

fn mount(disk: &str, drive: &str, part: Option<usize>) -> anyhow::Result<()> {
    // TODO: open `disk` as a BlockSource, seek to `part`'s offset (use
    // winfsp_fs_skeleton::partition::list), hand the slice to your
    // winfsp-rs FileSystemContext, and FileSystem::mount on `drive`.
    println!("[myfs] mount stub: disk={disk} drive={drive} part={part:?}");
    std::thread::park(); // keep the child alive so Watch's tracking is meaningful
    Ok(())
}
```

This compiles and runs. The watch/service arms are wired; `mount` is
the seam where you'll link winfsp-rs.

### 4. Smoke-test with a raw disk image

Before writing any winfsp-rs code, you can verify the detection +
spawn pipeline end-to-end by giving the watcher a USB stick whose
first bytes match `MyFs::detect`:

```sh
# on the Unix box -- 64 MiB image whose first 4 bytes are "MYFS"
dd if=/dev/zero of=myfs.img bs=1M count=64
printf 'MYFS' | dd of=myfs.img conv=notrunc
# write to a USB / SD card (replace sdX)
sudo dd if=myfs.img of=/dev/sdX bs=4M status=progress && sync
```

Then on Windows:

```powershell
cargo run -- watch
# plug in the USB
# expected: [myfs] myfs detected on \\?\STORAGE#Disk#... -> mounting on Z:
#           [myfs] mount stub: disk=... drive=Z: part=None
```

#### What the watcher does with a partition table

Two paths fire on `DBT_DEVICEARRIVAL`:

1. **No MBR signature.** If sector 0 doesn't end with `0x55 0xAA` at
   bytes 510-511, [`partition::list`](src/partition.rs) returns "no
   MBR signature" and the watcher falls back to probing the whole
   disk at offset 0. Hits spawn `mount` with no `--part` flag (the
   stub above sees `part=None`). The image we just built takes this
   path -- it's 64 MiB of zeros with `MYFS` at byte 0, no MBR.
2. **Signature present.** The walker reads the four MBR partition
   entries (446-509). If any entry has type `0xEE`, it's a GPT
   protective MBR and the walker switches to parsing the GPT header
   at LBA 1; otherwise it returns the MBR entries as-is. The watcher
   then probes each partition at `start_lba * 512` (sector size is
   fixed at 512) and spawns `mount --part N` for hits, with `N`
   1-indexed in MBR slot order.

So a partitioned smoke-test needs three things on disk: the `0x55
0xAA` signature, at least one valid MBR entry, and the magic placed
at that entry's `start_lba * 512`.

#### MBR layout, just the bytes that matter

```text
offset  size  field
  0     446   bootloader code (leave zero, we don't care)
  446    16   partition entry 1
  462    16   partition entry 2
  478    16   partition entry 3
  494    16   partition entry 4
  510     2   0x55 0xAA signature
```

Each 16-byte entry: byte +4 is the type code (`0x83` = Linux native,
`0x07` = NTFS, etc. -- the watcher doesn't care which, it probes
unconditionally), `+8..+12` is the start LBA (LE u32), `+12..+16` is
length in sectors (LE u32). The other fields (boot flag, CHS
geometry) are irrelevant for our purposes.

#### Building a partitioned image

`sfdisk` lays the MBR out for you; you just seek the magic onto the
partition's start LBA:

```sh
# 64 MiB image, single Linux partition spanning the disk.
dd if=/dev/zero of=myfs.img bs=1M count=64
echo ',,L' | sfdisk myfs.img

# Find where partition 1 starts (sfdisk usually picks LBA 2048 = 1 MiB).
PART_LBA=$(sfdisk -d myfs.img | awk '/start=/ {gsub(",",""); print $4; exit}')

# Drop the magic at that sector.
printf 'MYFS' | dd of=myfs.img bs=512 seek=$PART_LBA count=1 conv=notrunc
```

`sudo dd` it to a USB stick, plug in on Windows with `cargo run --
watch` running, and the log line changes to:

```text
[myfs] myfs detected on \\?\STORAGE#Disk#... -> mounting on Z:
[myfs] mount stub: disk=... drive=Z: part=Some(1)
```

The `parses_plain_mbr` / `parses_gpt` tests in
[`partition.rs`](src/partition.rs) build these tables byte-by-byte if
you want a runnable reference for an exotic layout (multiple
partitions, GPT, custom type codes).

#### Beyond the stub

Replace the `mount` body with a winfsp-rs `FileSystemContext` against
a real on-disk format. [`FileSource`](src/device.rs) is what you'll
hand it -- it handles the sector-aligned-read dance Windows raw-disk
handles require for `\\?\STORAGE#Disk#...` paths. For a partitioned
mount, offset all FS reads by `partition::list(disk)[part-1].start_lba
* 512`.

### 5. Ship

`customize.sh` already laid down the MSI/Burn bundle, the GitHub
release workflow, and the winget manifest. See
[`templates/README.md`](templates/README.md) for tagging conventions
and the WinFsp-pin update flow.

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
