# Templates

Copy these into the matching paths in your consumer repo and search-
replace the placeholders below. They're checked into the skeleton in
their **shipped form** (i.e. the actual files
[ext4-win-driver](https://github.com/antimatter-studios/ext4-win-driver)
uses today, with `ext4` / `ext4-win-driver` / `ExtFsWatcher` etc.
still hard-coded), so you can diff them against your customised copy
to see what's a parameter and what's structural.

## File map

| Skeleton path | Consumer path |
|---|---|
| `templates/installer/Bundle.wxs` | `installer/Bundle.wxs` |
| `templates/installer/Product.wxs` | `installer/Product.wxs` |
| `templates/installer/build.ps1` | `installer/build.ps1` |
| `templates/installer/Mount-Fs.ps1` | `installer/Mount-<Fs>.ps1` |
| `templates/installer/update-winfsp-pin.sh` | `installer/update-winfsp-pin.sh` |
| `templates/release.yml` | `.github/workflows/release.yml` |
| `templates/winget/installer.yaml` | `winget/v<X.Y.Z>/<Publisher>.<Package>.installer.yaml` |
| `templates/winget/locale.en-US.yaml` | `winget/v<X.Y.Z>/<Publisher>.<Package>.locale.en-US.yaml` |
| `templates/winget/version.yaml` | `winget/v<X.Y.Z>/<Publisher>.<Package>.yaml` |

## Scripted onboarding

If you'd rather not do the copy + sed dance by hand, run
`./customize.sh` from this directory. Typical invocation:

```sh
./customize.sh \
  --name ntfs-win-driver \
  --fs-name ntfs \
  --service-name NtfsFsWatcher \
  --launcher-class ntfs-mount \
  --file-extension vhd \
  --publisher-id AntimatterStudios \
  --publisher-name "Antimatter Studios" \
  --manufacturer "Your Name" \
  --winfsp-version 2.1.25156 \
  --winfsp-sha256 0123abcd... \
  --target /path/to/new-consumer-repo
```

The target must exist and be empty (or contain only `.git/`). Fresh
GUIDs for `MSI UpgradeCode` + `Bundle UpgradeCode` are generated via
`uuidgen` if `--upgrade-code-msi` / `--upgrade-code-bundle` are
omitted. The script's substitution table mirrors the table below --
keep them in sync if you tweak either.

## Substitutions

Find/replace these tokens with your project's values. Most appear in
multiple files; a single sed pass over the copied tree handles it.

| Token | Example (ext4-win-driver) | Where it appears |
|---|---|---|
| Crate / package name | `ext4-win-driver` | `Cargo.toml`, MSI Name, file names |
| FS short name | `ext4` | log lines, service-class prefix |
| SCM service name | `ExtFsWatcher` | `<ServiceInstall Name=...>` in Product.wxs, your `FsBackend::SERVICE_NAME` |
| WinFsp.Launcher class | `ext4-mount` | registry key in Product.wxs, your `FsBackend::LAUNCHER_SERVICE_CLASS` |
| File extension verb | `.img` | `HKCR\SystemFileAssociations\.<ext>\shell\...` in Product.wxs |
| Verb display name | `Mount as ext4` | Product.wxs |
| Manufacturer / publisher | `Chris Thomas`, `Antimatter Studios` | Product.wxs, Bundle.wxs, winget locale |
| MSI UpgradeCode (GUID) | `d6f3a7c2-9b14-4e7a-9b23-7c5f1f4a8e91` | **regenerate per consumer**, never reuse |
| Bundle UpgradeCode (GUID) | `b6d4e8a1-3f29-4c7d-9e22-1a8c5d6f7e34` | **regenerate per consumer**, never reuse |
| GitHub repo URL | `antimatter-studios/ext4-win-driver` | Product.wxs `AboutUrl`, Bundle.wxs, release.yml `gh release upload` |
| winget package id | `AntimatterStudios.ext4-win-driver` | all three winget yamls |
| WinFsp redist version | `2.1.25156` | build.ps1's `WinFspVersion` pin |
| WinFsp redist SHA256 | hex | build.ps1's `WinFspSha256` pin |

> **Generate fresh GUIDs per consumer.** Reusing `UpgradeCode` across
> products breaks Windows' major-upgrade detection (a `winget upgrade`
> on `ext4-win-driver` would try to "upgrade" to your unrelated package).
> `uuidgen | tr A-Z a-z` is enough.

## Why templates and not generated code

A code generator that turned a single `consumer.toml` into all of the
above would be neat but adds a dependency on the generator's stability
and a debugging surface no one wants when a WiX warning surfaces three
months later. The templates here are deliberately **the same files as
the reference consumer ships** -- the only "magic" is that you're
told which strings to replace, and you can verify the result against a
known-good binary install.

## Keeping the skeleton in sync

When the skeleton's `release.yml` template gets a fix (e.g. a new
toolchain workaround), bump the submodule pointer in your consumer
and copy the patched template back over your `.github/workflows/`.
Diff first to see if you've made consumer-specific edits that need
preserving.
