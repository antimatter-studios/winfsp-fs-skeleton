//! MBR + GPT partition table parsing.
//!
//! Pure logic, no platform-specific calls — `list(path)` opens a regular
//! `File` for reading and parses the first few sectors. Used by the CLI's
//! `parts` subcommand and (later) by the partition selector when mounting
//! a whole-disk image / `\\.\PhysicalDriveN`.
//!
//! Sector size is fixed at 512 here. Real 4Kn drives need detection
//! (Windows: `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`). That lands together with
//! the Win32 raw-device `BlockDevice` impl.

use crate::device::{BlockSource, FileSource};
use anyhow::{bail, Context, Result};
use std::path::Path;

const SECTOR: u64 = 512;
const MBR_SIG_OFF: usize = 510;
const MBR_PART_OFF: usize = 446;
const GPT_SIG: &[u8] = b"EFI PART";

/// One partition entry, normalised across MBR and GPT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// First LBA of the partition (sector index, sector size = 512).
    pub start_lba: u64,
    /// Length in sectors.
    pub num_sectors: u64,
    /// Short human-readable kind: `GPT:linux`, `GPT:efi`, `GPT:msbasic`,
    /// `GPT:swap`, `GPT:<hex-guid>`, `MBR:0x83` etc.
    pub kind: String,
    /// GPT only — UTF-16LE-decoded partition name. `None` for MBR.
    pub name: Option<String>,
}

/// Parse the partition table at `path`. Convenience wrapper over
/// [`list_from_source`] that opens a [`FileSource`] internally.
pub fn list(path: &Path) -> Result<Vec<Partition>> {
    let src = FileSource::open(path)?;
    list_from_source(&src)
}

/// Parse the partition table from any [`BlockSource`]. Detects GPT (via
/// 0xEE protective MBR entry) and falls back to plain MBR.
pub fn list_from_source(src: &dyn BlockSource) -> Result<Vec<Partition>> {
    let mut mbr = [0u8; 512];
    src.read_at(0, &mut mbr).context("reading MBR sector")?;

    if mbr[MBR_SIG_OFF] != 0x55 || mbr[MBR_SIG_OFF + 1] != 0xAA {
        bail!("no MBR signature at offset 510 — not a partitioned device");
    }

    if has_gpt_protective(&mbr) {
        parse_gpt(src).context("parsing GPT")
    } else {
        let mut out = Vec::new();
        let mut extended = Vec::new();
        for p in parse_mbr(&mbr) {
            if is_extended(&p.kind) {
                extended.push(p);
            } else {
                out.push(p);
            }
        }
        for ext in &extended {
            walk_ebr_chain(src, ext, &mut out);
        }
        Ok(out)
    }
}

/// MBR type bytes that mark an extended partition: CHS (0x05), LBA (0x0F)
/// and Linux's own (0x85). They hold an EBR chain, not a filesystem.
const EXTENDED_KINDS: [&str; 3] = ["MBR:0x05", "MBR:0x0f", "MBR:0x85"];

/// Upper bound on EBRs followed in one chain, so a corrupt chain cannot
/// keep the walk going. Far above any real disk's logical count.
const MAX_LOGICALS: usize = 128;

fn is_extended(kind: &str) -> bool {
    EXTENDED_KINDS.contains(&kind)
}

/// Follow the EBR chain inside the extended partition `ext`, appending
/// each logical partition to `out`.
///
/// In every EBR, slot 0 is the logical partition, with its start relative
/// to that EBR's own sector; slot 1 links to the next EBR, with its start
/// relative to the first sector of the extended partition. The walk stops
/// at the end of the chain, and also — without failing the listing, so
/// the primaries still come through — at an unreadable EBR, a missing
/// boot signature, a link outside the extended partition, an EBR already
/// visited, or after [`MAX_LOGICALS`] hops.
fn walk_ebr_chain(src: &dyn BlockSource, ext: &Partition, out: &mut Vec<Partition>) {
    let ext_end = ext.start_lba + ext.num_sectors;
    let mut visited = Vec::new();
    let mut ebr_lba = ext.start_lba;
    while visited.len() < MAX_LOGICALS && !visited.contains(&ebr_lba) {
        visited.push(ebr_lba);
        let mut ebr = [0u8; 512];
        if src.read_at(ebr_lba * SECTOR, &mut ebr).is_err()
            || ebr[MBR_SIG_OFF] != 0x55
            || ebr[MBR_SIG_OFF + 1] != 0xAA
        {
            return;
        }
        let slots = parse_mbr(&ebr);
        let mut next = None;
        for slot in slots.into_iter().take(2) {
            if is_extended(&slot.kind) {
                if next.is_none() {
                    next = Some(ext.start_lba + slot.start_lba);
                }
            } else if slot.start_lba != 0 {
                out.push(Partition {
                    start_lba: ebr_lba + slot.start_lba,
                    ..slot
                });
            }
        }
        match next {
            Some(lba) if lba > ext.start_lba && lba < ext_end => ebr_lba = lba,
            _ => return,
        }
    }
}

fn has_gpt_protective(mbr: &[u8; 512]) -> bool {
    for i in 0..4 {
        let off = MBR_PART_OFF + i * 16;
        if mbr[off + 4] == 0xEE {
            return true;
        }
    }
    false
}

fn parse_mbr(mbr: &[u8; 512]) -> Vec<Partition> {
    let mut out = Vec::new();
    for i in 0..4 {
        let off = MBR_PART_OFF + i * 16;
        let kind_byte = mbr[off + 4];
        let lba = u32::from_le_bytes(mbr[off + 8..off + 12].try_into().unwrap()) as u64;
        let len = u32::from_le_bytes(mbr[off + 12..off + 16].try_into().unwrap()) as u64;
        if kind_byte == 0 || len == 0 {
            continue;
        }
        out.push(Partition {
            start_lba: lba,
            num_sectors: len,
            kind: format!("MBR:0x{kind_byte:02x}"),
            name: None,
        });
    }
    out
}

fn parse_gpt(src: &dyn BlockSource) -> Result<Vec<Partition>> {
    let mut hdr = [0u8; 512];
    src.read_at(SECTOR, &mut hdr)
        .context("reading GPT header sector")?;
    if &hdr[0..8] != GPT_SIG {
        bail!("GPT header signature missing (expected \"EFI PART\")");
    }
    let header_size = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
    if !(92..=512).contains(&header_size) {
        bail!("implausible GPT header size: {header_size}");
    }
    let part_array_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
    let n_entries = u32::from_le_bytes(hdr[80..84].try_into().unwrap());
    let entry_size = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
    if !(128..=4096).contains(&entry_size) {
        bail!("implausible GPT entry size: {entry_size}");
    }
    if n_entries > 4096 {
        bail!("implausible GPT entry count: {n_entries}");
    }

    let total = n_entries as usize * entry_size;
    let mut buf = vec![0u8; total];
    src.read_at(part_array_lba * SECTOR, &mut buf)
        .context("reading GPT entries")?;

    let mut out = Vec::new();
    for i in 0..n_entries as usize {
        let e = &buf[i * entry_size..(i + 1) * entry_size];
        let type_guid: [u8; 16] = e[0..16].try_into().unwrap();
        if type_guid == [0u8; 16] {
            continue;
        }
        let start = u64::from_le_bytes(e[32..40].try_into().unwrap());
        let end = u64::from_le_bytes(e[40..48].try_into().unwrap());
        if end < start {
            continue;
        }
        let name = decode_utf16le(&e[56..128]);
        out.push(Partition {
            start_lba: start,
            num_sectors: end - start + 1,
            kind: classify_gpt_guid(&type_guid),
            name: if name.is_empty() { None } else { Some(name) },
        });
    }
    Ok(out)
}

fn decode_utf16le(buf: &[u8]) -> String {
    let mut out = String::new();
    for chunk in buf.chunks_exact(2) {
        let cu = u16::from_le_bytes(chunk.try_into().unwrap());
        if cu == 0 {
            break;
        }
        if let Some(c) = char::from_u32(cu as u32) {
            out.push(c);
        }
    }
    out
}

// Type GUIDs as on-disk bytes (mixed-endian: first 4 + next 2 + next 2 are LE,
// final 8 are big-endian/byte-by-byte).
const GUID_LINUX_FS: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];
const GUID_EFI_SYSTEM: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];
const GUID_MS_BASIC: [u8; 16] = [
    0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
];
const GUID_LINUX_SWAP: [u8; 16] = [
    0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F,
];

fn classify_gpt_guid(g: &[u8; 16]) -> String {
    if *g == GUID_LINUX_FS {
        "GPT:linux".into()
    } else if *g == GUID_EFI_SYSTEM {
        "GPT:efi".into()
    } else if *g == GUID_MS_BASIC {
        "GPT:msbasic".into()
    } else if *g == GUID_LINUX_SWAP {
        "GPT:swap".into()
    } else {
        format!("GPT:{}", format_guid(g))
    }
}

fn format_guid(g: &[u8; 16]) -> String {
    // Display in canonical 8-4-4-4-12 form, accounting for the
    // mixed-endian on-disk layout.
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        g[3], g[2], g[1], g[0],
        g[5], g[4],
        g[7], g[6],
        g[8], g[9],
        g[10], g[11], g[12], g[13], g[14], g[15],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ext4_win_driver_{tag}_{}_{n}.bin",
            std::process::id()
        ));
        p
    }

    fn write_image(path: &std::path::Path, data: &[u8]) {
        let mut f = File::create(path).unwrap();
        f.write_all(data).unwrap();
    }

    fn build_mbr_with(parts: &[(u8, u32, u32)]) -> [u8; 512] {
        let mut mbr = [0u8; 512];
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        for (i, &(kind, lba, len)) in parts.iter().enumerate() {
            let off = MBR_PART_OFF + i * 16;
            mbr[off + 4] = kind;
            mbr[off + 8..off + 12].copy_from_slice(&lba.to_le_bytes());
            mbr[off + 12..off + 16].copy_from_slice(&len.to_le_bytes());
        }
        mbr
    }

    #[test]
    fn rejects_missing_signature() {
        let path = tmp_path("nosig");
        write_image(&path, &[0u8; 1024]);
        let err = list(&path).unwrap_err();
        assert!(err.to_string().contains("no MBR signature"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parses_plain_mbr() {
        let mbr = build_mbr_with(&[(0x83, 2048, 1_000_000), (0x07, 1_002_048, 500_000)]);
        let path = tmp_path("mbr");
        write_image(&path, &mbr);
        let parts = list(&path).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].start_lba, 2048);
        assert_eq!(parts[0].num_sectors, 1_000_000);
        assert_eq!(parts[0].kind, "MBR:0x83");
        assert_eq!(parts[1].kind, "MBR:0x07");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn skips_empty_mbr_slots() {
        let mbr = build_mbr_with(&[(0x83, 2048, 1000)]);
        let path = tmp_path("mbr_sparse");
        write_image(&path, &mbr);
        let parts = list(&path).unwrap();
        assert_eq!(parts.len(), 1);
        std::fs::remove_file(&path).ok();
    }

    /// Write one 16-byte MBR/EBR table slot into `sector` (a 512-byte
    /// sector image) and stamp the 0x55AA boot signature.
    fn put_slot(sector: &mut [u8], slot: usize, kind: u8, lba: u32, len: u32) {
        sector[510] = 0x55;
        sector[511] = 0xAA;
        let off = MBR_PART_OFF + slot * 16;
        sector[off + 4] = kind;
        sector[off + 8..off + 12].copy_from_slice(&lba.to_le_bytes());
        sector[off + 12..off + 16].copy_from_slice(&len.to_le_bytes());
    }

    fn sector_mut(img: &mut [u8], lba: usize) -> &mut [u8] {
        &mut img[lba * 512..(lba + 1) * 512]
    }

    #[test]
    fn parses_logical_partitions_in_extended_chain() {
        // The layout from ext4-win-driver#11, shrunk: a primary, then an
        // extended partition holding ext4 + swap + ext4 as logicals.
        //
        //   LBA   0: MBR  [0x07 @2 len 90] [0x05 @100 len 300]
        //   LBA 100: EBR  [0x83 @+1 len 50] [0x05 @ext+100 len 100]
        //   LBA 200: EBR  [0x82 @+1 len 50] [0x05 @ext+200 len 100]
        //   LBA 300: EBR  [0x83 @+1 len 50] (end of chain)
        //
        // A logical's start is relative to its own EBR; the link to the
        // next EBR is relative to the start of the extended partition.
        let mut img = vec![0u8; 512 * 352];
        put_slot(sector_mut(&mut img, 0), 0, 0x07, 2, 90);
        put_slot(sector_mut(&mut img, 0), 1, 0x05, 100, 300);
        put_slot(sector_mut(&mut img, 100), 0, 0x83, 1, 50);
        put_slot(sector_mut(&mut img, 100), 1, 0x05, 100, 100);
        put_slot(sector_mut(&mut img, 200), 0, 0x82, 1, 50);
        put_slot(sector_mut(&mut img, 200), 1, 0x05, 200, 100);
        put_slot(sector_mut(&mut img, 300), 0, 0x83, 1, 50);

        let path = tmp_path("mbr_logical");
        write_image(&path, &img);
        let parts = list(&path).unwrap();
        std::fs::remove_file(&path).ok();

        let got: Vec<(u64, u64, &str)> = parts
            .iter()
            .map(|p| (p.start_lba, p.num_sectors, p.kind.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (2, 90, "MBR:0x07"),
                (101, 50, "MBR:0x83"),
                (201, 50, "MBR:0x82"),
                (301, 50, "MBR:0x83"),
            ],
            "the extended container is replaced by the logicals it holds"
        );
    }

    #[test]
    fn ebr_chain_that_loops_back_terminates() {
        // A corrupt chain whose link points at its own EBR must not spin.
        let mut img = vec![0u8; 512 * 160];
        put_slot(sector_mut(&mut img, 0), 0, 0x0F, 100, 60);
        put_slot(sector_mut(&mut img, 100), 0, 0x83, 1, 50);
        put_slot(sector_mut(&mut img, 100), 1, 0x05, 0, 60);

        let path = tmp_path("mbr_ebr_loop");
        write_image(&path, &img);
        let parts = list(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(parts.len(), 1, "got {parts:?}");
        assert_eq!(parts[0].start_lba, 101);
        assert_eq!(parts[0].kind, "MBR:0x83");
    }

    #[test]
    fn parses_gpt() {
        // Layout:
        //   LBA 0: protective MBR (0xEE entry)
        //   LBA 1: GPT header
        //   LBA 2..: partition entry array (we put it at LBA 2)
        let mut img = vec![0u8; SECTOR as usize * 64];

        // Protective MBR.
        img[510] = 0x55;
        img[511] = 0xAA;
        img[MBR_PART_OFF + 4] = 0xEE;
        let total_sectors = (img.len() / 512) as u32 - 1;
        img[MBR_PART_OFF + 8..MBR_PART_OFF + 12].copy_from_slice(&1u32.to_le_bytes());
        img[MBR_PART_OFF + 12..MBR_PART_OFF + 16].copy_from_slice(&total_sectors.to_le_bytes());

        // GPT header.
        let hdr_off = SECTOR as usize;
        img[hdr_off..hdr_off + 8].copy_from_slice(GPT_SIG);
        img[hdr_off + 12..hdr_off + 16].copy_from_slice(&92u32.to_le_bytes()); // header size
        img[hdr_off + 72..hdr_off + 80].copy_from_slice(&2u64.to_le_bytes()); // entry array LBA
        img[hdr_off + 80..hdr_off + 84].copy_from_slice(&3u32.to_le_bytes()); // n entries
        img[hdr_off + 84..hdr_off + 88].copy_from_slice(&128u32.to_le_bytes()); // entry size

        // Entries: [linux fs, EFI system, empty]
        let arr_off = SECTOR as usize * 2;
        // Entry 0: Linux fs at LBA 2048..=4047 (2000 sectors).
        img[arr_off..arr_off + 16].copy_from_slice(&GUID_LINUX_FS);
        img[arr_off + 32..arr_off + 40].copy_from_slice(&2048u64.to_le_bytes());
        img[arr_off + 40..arr_off + 48].copy_from_slice(&4047u64.to_le_bytes());
        // Name "rootfs" UTF-16LE.
        for (i, c) in "rootfs".encode_utf16().enumerate() {
            img[arr_off + 56 + i * 2..arr_off + 56 + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }

        // Entry 1: EFI system at LBA 4048..=5047.
        let e1 = arr_off + 128;
        img[e1..e1 + 16].copy_from_slice(&GUID_EFI_SYSTEM);
        img[e1 + 32..e1 + 40].copy_from_slice(&4048u64.to_le_bytes());
        img[e1 + 40..e1 + 48].copy_from_slice(&5047u64.to_le_bytes());

        let path = tmp_path("gpt");
        write_image(&path, &img);
        let parts = list(&path).unwrap();
        assert_eq!(parts.len(), 2, "got {parts:?}");
        assert_eq!(parts[0].kind, "GPT:linux");
        assert_eq!(parts[0].start_lba, 2048);
        assert_eq!(parts[0].num_sectors, 2000);
        assert_eq!(parts[0].name.as_deref(), Some("rootfs"));
        assert_eq!(parts[1].kind, "GPT:efi");
        assert_eq!(parts[1].start_lba, 4048);
        assert_eq!(parts[1].num_sectors, 1000);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unknown_gpt_guid_falls_back_to_hex() {
        let g = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
            0xFF, 0x00,
        ];
        let s = classify_gpt_guid(&g);
        assert!(s.starts_with("GPT:"));
        assert!(s.contains('-'));
    }
}
