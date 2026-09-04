//! Conversions between POSIX filesystem values and their Windows
//! equivalents.
//!
//! # Why this module exists
//!
//! Every consumer of this skeleton implements the same WinFsp callback
//! set over a different filesystem reader, and in doing so writes the
//! same conversions: a Windows path to a Unix one, a Unix timestamp to
//! a FILETIME, a Unix mode to a Windows attribute bitmap. None of that
//! is filesystem knowledge — it is Windows knowledge, identical
//! whether ext4, XFS or EROFS is underneath.
//!
//! Measured across the two drivers that existed when this module was
//! written: of ~1,220 lines of code in each `mount.rs`, **5–6% touched
//! the filesystem and 5–7% touched WinFsp types.** The remaining ~88%
//! was this — translation, written twice.
//!
//! # The duplication was not benign
//!
//! The two copies had already drifted, and one was worse:
//!
//! | | ext4-win-driver | erofs-win-driver |
//! |---|---|---|
//! | signature | `unix_to_filetime(secs: u32)` plus a second `_nsec` variant | `unix_to_filetime(secs: u64, nsec: u32)` |
//!
//! **The ext4 copy truncated seconds to `u32`.** Its own doc comment
//! admitted the problem — "ext4 timestamps fit in 32 bits (or 64 with
//! high-precision attrs)" — and took `u32` regardless, so any
//! timestamp past 2106 would have wrapped. The EROFS copy took `u64`
//! and folded the nanosecond case into one function.
//!
//! This module takes the EROFS shape. That is the general rule when
//! consolidating: the copies are not interchangeable, and picking one
//! at random preserves whichever bug happened to be in it.

/// Seconds between the FILETIME epoch (1601-01-01) and the Unix epoch
/// (1970-01-01).
///
/// Not a magic number to be re-derived: 369 years, 89 of them leap.
pub const FILETIME_EPOCH_OFFSET_SEC: u64 = 11_644_473_600;

/// 100-nanosecond ticks per second — FILETIME's unit.
pub const FILETIME_TICKS_PER_SEC: u64 = 10_000_000;

/// Convert a Unix timestamp to a Windows FILETIME (100-ns intervals
/// since 1601-01-01).
///
/// Takes **signed** 64-bit seconds. Two separate reasons, and the first
/// version of this got only one of them right:
///
/// - 64 bits, because several filesystems here store 64-bit timestamps
///   (ext4 with high-precision attributes, XFS v5, EROFS) and a `u32`
///   parameter silently truncates them.
/// - signed, because a Unix timestamp before 1970 is negative, and
///   FILETIME can represent it perfectly well — its epoch is **1601**,
///   which is 369 years EARLIER than Unix's. A negative Unix second is
///   still a positive FILETIME. An unsigned parameter cannot express
///   the input, so the loss happens at the call site, before this
///   function ever runs.
///
/// Passing `0` for `nsec` gives the whole-second conversion, so this is
/// the only entry point needed.
///
/// Saturating rather than wrapping: a timestamp far enough out to
/// overflow is corrupt or synthetic, and clamping to the end of
/// representable time beats wrapping to 1601. A time before 1601 is
/// likewise clamped to 0 — FILETIME has no way to say "earlier than my
/// epoch", and 0 is the value Windows itself uses for "not set".
///
/// FILETIME's resolution is 100 ns, so `nsec` is rounded **down** to
/// the nearest 100 — the same direction the filesystem itself would
/// round, and the direction that never reports a file as newer than it
/// is.
pub fn unix_to_filetime(secs: i64, nsec: u32) -> u64 {
    let since_filetime_epoch = secs.saturating_add(FILETIME_EPOCH_OFFSET_SEC as i64);
    if since_filetime_epoch < 0 {
        // Before 1601. Unreachable from any real filesystem timestamp —
        // ext4 bottoms out at 1901 — but a corrupt image can name one.
        return 0;
    }
    (since_filetime_epoch as u64)
        .saturating_mul(FILETIME_TICKS_PER_SEC)
        .saturating_add((nsec as u64) / 100)
}

/// The inverse: a Windows FILETIME back to Unix seconds and
/// nanoseconds.
///
/// `None` means **only** the zero sentinel, which is how WinFSP spells
/// "leave this timestamp unchanged" in `set_basic_info`. Every other
/// value converts.
///
/// That is the whole point, and the drivers' own copies got it wrong in
/// both directions: they returned `None` for any FILETIME predating the
/// Unix epoch — silently discarding a time the user asked for, since
/// the caller reads `None` as "leave unchanged" — and clamped anything
/// past 2106 to a nonsense value, because they returned `u32` seconds.
/// A negative Unix second is perfectly ordinary and the filesystems
/// store it, so the signed [`Timestamp`] is the honest return type.
///
/// Sub-second precision below 100 ns cannot survive the round trip;
/// FILETIME has no bits for it.
pub fn filetime_to_unix(filetime: u64) -> Option<Timestamp> {
    if filetime == 0 {
        return None;
    }
    let seconds_since_1601 = (filetime / FILETIME_TICKS_PER_SEC) as i64;
    let nsec = ((filetime % FILETIME_TICKS_PER_SEC) * 100) as u32;
    Some(Timestamp {
        secs: seconds_since_1601 - FILETIME_EPOCH_OFFSET_SEC as i64,
        nsec,
    })
}

/// `\foo\bar` → `/foo/bar`, and an empty path to `/`.
///
/// WinFsp hands paths in with backslashes and no leading slash for the
/// root; every reader in this family wants a Unix path. The empty case
/// is the root directory, which WinFsp represents as an empty string
/// and the readers as `/`.
///
/// This one was already byte-for-byte identical in both drivers — the
/// clearest possible case for lifting it rather than writing it a third
/// time.
pub fn winpath_to_unix(path: &str) -> String {
    if path.is_empty() {
        return "/".into();
    }
    path.replace('\\', "/")
}

/// Windows file-attribute bits this module sets.
///
/// Named rather than passed as literals because a bitmap of bare hex is
/// the single easiest thing to get subtly wrong, and because the values
/// are Windows constants a reader should be able to check against the
/// SDK without reverse-engineering them from usage.
pub mod attr {
    pub const READONLY: u32 = 0x0000_0001;
    pub const DIRECTORY: u32 = 0x0000_0010;
    pub const ARCHIVE: u32 = 0x0000_0020;
    pub const NORMAL: u32 = 0x0000_0080;
    pub const REPARSE_POINT: u32 = 0x0000_0400;
}

/// What kind of node an entry is.
///
/// **Explicit, rather than derived from a mode's type bits, and that is
/// the whole point of this type.** The three C ABIs disagree about
/// whether `mode` even carries a type:
///
/// | crate | `mode` contains |
/// |---|---|
/// | ext4 | permission bits **and** the `S_IFMT` type nibble |
/// | xfs | permission bits **and** the type nibble |
/// | erofs | permission bits **only** — the type is a separate field |
///
/// Shared code that inferred the type from `mode & S_IFMT` would
/// therefore classify every EROFS entry as a regular file, directories
/// included, because that nibble is always zero there. The first draft
/// of this module did exactly that. Taking the kind as a parameter
/// makes the caller — which knows its own filesystem's convention —
/// responsible for saying what the thing is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Dir,
    Symlink,
    /// Device nodes, FIFOs, sockets. Windows has no representation for
    /// these; they are surfaced as files so a listing can still show
    /// them rather than failing.
    Other,
}

/// A timestamp normalised across filesystems.
///
/// `secs` is signed and 64-bit because the C ABIs are not consistent:
/// XFS reports `i64`, ext4 and EROFS report `u32`. Signed 64-bit is the
/// only width that holds all of them without truncating XFS or
/// misreading a pre-1970 value as a far-future one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    pub secs: i64,
    pub nsec: u32,
}

impl Timestamp {
    /// Convert to a Windows FILETIME.
    ///
    /// A negative timestamp — before 1970 — is representable in
    /// FILETIME, which counts from 1601, so it is converted rather than
    /// clamped to the epoch.
    pub fn to_filetime(self) -> u64 {
        unix_to_filetime(self.secs, self.nsec)
    }
}

/// One entry's attributes, normalised.
///
/// Each driver fills this from its own C `*_attr_t`, which is where the
/// per-filesystem differences belong — widths (`inode` is `u32` in
/// ext4 and `u64` elsewhere; `link_count` is `u16` in ext4 and `u32`
/// elsewhere), signedness, and **presence**: EROFS reports only
/// `mtime`, while ext4 and XFS report four timestamps.
///
/// Absent times are `None` rather than zero. Zero is a real time —
/// 1970-01-01 — and reporting it as though the filesystem had recorded
/// it is a lie Windows will display.
#[derive(Debug, Clone, Copy)]
pub struct FileAttr {
    pub kind: NodeKind,
    /// Permission bits only — no type nibble. See [`NodeKind`].
    pub perms: u16,
    pub size: u64,
    pub inode: u64,
    pub link_count: u32,
    pub mtime: Timestamp,
    /// `None` where the filesystem does not record it. Callers should
    /// fall back to `mtime`, which every filesystem here has.
    pub atime: Option<Timestamp>,
    pub ctime: Option<Timestamp>,
    pub crtime: Option<Timestamp>,
}

impl FileAttr {
    /// Access time if recorded, else the modification time.
    ///
    /// Windows always wants a value; this makes the substitution
    /// explicit rather than leaving each caller to invent one.
    pub fn atime_or_mtime(&self) -> Timestamp {
        self.atime.unwrap_or(self.mtime)
    }

    pub fn ctime_or_mtime(&self) -> Timestamp {
        self.ctime.unwrap_or(self.mtime)
    }

    pub fn crtime_or_mtime(&self) -> Timestamp {
        self.crtime.unwrap_or(self.mtime)
    }
}

/// Any write bit — owner, group or other.
const ANY_WRITE: u16 = 0o222;

/// Translate a normalised entry into a Windows file-attribute bitmap.
///
/// Takes a [`NodeKind`] rather than digging the type out of a mode —
/// see that type for why.
///
/// `volume_read_only` is separate from the permission bits because they
/// are different questions: a file can be writable while the volume is
/// mounted read-only, and Windows must be told the *effective* answer
/// or Explorer offers edits that then fail.
///
/// A symlink becomes `REPARSE_POINT` and **not** also `DIRECTORY`, even
/// when it points at one: WinFsp resolves the reparse point and asks
/// again, so claiming both makes Explorer treat the link itself as a
/// folder.
///
/// `NORMAL` means "no other attributes", so it is only ever returned
/// alone.
pub fn attributes_for(kind: NodeKind, perms: u16, volume_read_only: bool) -> u32 {
    let mut a = match kind {
        NodeKind::Dir => attr::DIRECTORY,
        NodeKind::Symlink => attr::REPARSE_POINT,
        NodeKind::File | NodeKind::Other => attr::ARCHIVE,
    };
    if volume_read_only || (perms & ANY_WRITE) == 0 {
        a |= attr::READONLY;
    }
    if a == 0 {
        a = attr::NORMAL;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The epoch offset is the value every Windows timestamp
    /// conversion rests on, so it is checked against its derivation
    /// rather than restated.
    ///
    /// 1601-01-01 to 1970-01-01 is 369 years. In the proleptic
    /// Gregorian calendar that span contains 89 leap days: 369/4 = 92
    /// four-year marks, minus 3 century years that are not leap (1700,
    /// 1800, 1900) — 1600 and 2000 are leap but fall outside the span's
    /// interior.
    #[test]
    fn the_filetime_epoch_offset_is_369_years_of_seconds() {
        let days = 369 * 365 + 89;
        assert_eq!(FILETIME_EPOCH_OFFSET_SEC, days as u64 * 86_400);
    }

    /// The Unix epoch itself maps to the offset, in ticks.
    #[test]
    fn the_unix_epoch_converts_to_the_offset() {
        assert_eq!(
            unix_to_filetime(0, 0),
            FILETIME_EPOCH_OFFSET_SEC * FILETIME_TICKS_PER_SEC
        );
    }

    /// **The regression this module exists to prevent.**
    ///
    /// A timestamp past 2106 does not fit in `u32` seconds. The ext4
    /// driver's copy took `u32`, so a value like this arrived already
    /// truncated by the caller's cast and produced a date in the past.
    /// Taking `u64` is the fix, and this pins it.
    #[test]
    fn a_timestamp_beyond_the_u32_range_survives() {
        // 2106-02-07T06:28:16Z — one second past u32::MAX seconds.
        let past_2106 = u32::MAX as u64 + 1;
        let got = unix_to_filetime(past_2106 as i64, 0);
        let at_u32_max = unix_to_filetime(i64::from(u32::MAX), 0);
        assert!(
            got > at_u32_max,
            "a timestamp past 2106 must be later than one at the u32 ceiling, \
             not wrapped: got {got}, u32 ceiling {at_u32_max}"
        );
    }

    /// Nanoseconds round down to FILETIME's 100-ns resolution.
    #[test]
    fn sub_second_precision_rounds_down_to_100ns() {
        let base = unix_to_filetime(1, 0);
        assert_eq!(unix_to_filetime(1, 99), base, "under 100 ns adds nothing");
        assert_eq!(unix_to_filetime(1, 100), base + 1);
        assert_eq!(unix_to_filetime(1, 199), base + 1, "rounds down, not up");
        assert_eq!(unix_to_filetime(1, 999_999_999), base + 9_999_999);
    }

    /// Saturating, not wrapping — a corrupt far-future timestamp must
    /// not come back as 1601.
    #[test]
    fn an_absurd_timestamp_saturates_rather_than_wrapping() {
        assert_eq!(unix_to_filetime(i64::MAX, 0), u64::MAX);
    }

    /// A time before 1970 is negative in Unix and perfectly ordinary in
    /// FILETIME, whose epoch is 1601 — 369 years earlier. The `u64`
    /// parameter this replaced could not express the input at all, so
    /// the loss happened at the call site.
    #[test]
    fn a_pre_1970_time_converts_rather_than_exploding() {
        // 1960-01-01. 1601 -> 1960 is 11_644_473_600 - 315_619_200
        // seconds, and FILETIME counts 100ns ticks.
        let expected = (FILETIME_EPOCH_OFFSET_SEC - 315_619_200) * FILETIME_TICKS_PER_SEC;
        assert_eq!(unix_to_filetime(-315_619_200, 0), expected);
        // Strictly before the epoch, and strictly after 1601.
        assert!(unix_to_filetime(-315_619_200, 0) < unix_to_filetime(0, 0));
        assert!(unix_to_filetime(-315_619_200, 0) > 0);
    }

    /// The oldest time ext4 can store: the floor of a signed 32-bit
    /// second count, 1901-12-13.
    #[test]
    fn the_oldest_representable_filesystem_time_converts() {
        let ft = unix_to_filetime(i64::from(i32::MIN), 0);
        assert!(ft > 0, "1901 is after FILETIME's 1601 epoch");
        assert!(ft < unix_to_filetime(0, 0));
    }

    /// Earlier than FILETIME itself can express. Clamped to 0, which is
    /// what Windows uses for "not set" — not wrapped to a far-future
    /// time, which is what an unchecked cast would produce.
    #[test]
    fn a_time_before_1601_clamps_to_zero() {
        assert_eq!(unix_to_filetime(i64::MIN, 0), 0);
        assert_eq!(
            unix_to_filetime(-(FILETIME_EPOCH_OFFSET_SEC as i64) - 1, 0),
            0
        );
    }

    /// `Timestamp::to_filetime` used `self.secs as u64`, which turned a
    /// negative second into an enormous positive one and saturated to a
    /// far-future FILETIME. The cast is gone; this pins it.
    #[test]
    fn a_negative_timestamp_does_not_become_a_future_one() {
        let past = Timestamp {
            secs: -315_619_200,
            nsec: 0,
        };
        let epoch = Timestamp { secs: 0, nsec: 0 };
        assert!(
            past.to_filetime() < epoch.to_filetime(),
            "a 1960 timestamp must convert to a FILETIME before 1970's"
        );
    }

    #[test]
    fn windows_paths_become_unix_paths() {
        assert_eq!(winpath_to_unix(""), "/", "the empty path is the root");
        assert_eq!(winpath_to_unix("\\"), "/");
        assert_eq!(winpath_to_unix("\\foo\\bar"), "/foo/bar");
        assert_eq!(winpath_to_unix("\\a b\\c.txt"), "/a b/c.txt");
    }

    #[test]
    fn a_directory_is_marked_as_one() {
        let a = attributes_for(NodeKind::Dir, 0o755, false);
        assert_ne!(a & attr::DIRECTORY, 0);
        assert_eq!(a & attr::READONLY, 0, "0755 has write bits");
    }

    #[test]
    fn a_file_with_no_write_bit_is_readonly() {
        assert_ne!(
            attributes_for(NodeKind::File, 0o444, false) & attr::READONLY,
            0
        );
        assert_eq!(
            attributes_for(NodeKind::File, 0o644, false) & attr::READONLY,
            0
        );
    }

    /// **The trap this design exists to avoid.**
    ///
    /// EROFS's `mode` carries no type bits, so a helper that inferred
    /// the kind from `mode & S_IFMT` saw zero and called every entry a
    /// regular file — directories included. Passing the kind
    /// explicitly means a directory is a directory whatever the mode
    /// happens to contain, which is what this asserts: identical
    /// permission bits, different kinds, different answers.
    #[test]
    fn the_kind_decides_the_type_not_the_mode_bits() {
        let perms = 0o755; // no type nibble at all — the EROFS shape
        assert_ne!(
            attributes_for(NodeKind::Dir, perms, false) & attr::DIRECTORY,
            0,
            "a directory must be marked as one even when the mode has no type bits"
        );
        assert_eq!(
            attributes_for(NodeKind::File, perms, false) & attr::DIRECTORY,
            0
        );
        assert_ne!(
            attributes_for(NodeKind::Symlink, perms, false) & attr::REPARSE_POINT,
            0
        );
    }

    /// A time the filesystem does not record falls back to mtime
    /// rather than being reported as 1970.
    #[test]
    fn an_absent_timestamp_falls_back_to_mtime() {
        let m = Timestamp {
            secs: 1_700_000_000,
            nsec: 0,
        };
        let a = FileAttr {
            kind: NodeKind::File,
            perms: 0o644,
            size: 0,
            inode: 1,
            link_count: 1,
            mtime: m,
            atime: None,
            ctime: None,
            crtime: None,
        };
        assert_eq!(a.atime_or_mtime(), m, "EROFS records only mtime");
        assert_eq!(a.ctime_or_mtime(), m);
        assert_eq!(a.crtime_or_mtime(), m);
    }

    /// The 2106 truncation, checked through `Timestamp` as well as
    /// through `unix_to_filetime`.
    ///
    /// Added because a mutation showed the direct test did not cover
    /// this path: narrowing `secs` to `u32` inside `to_filetime` broke
    /// nothing. Both routes reach the same conversion, and both are
    /// now pinned.
    #[test]
    fn a_timestamp_past_2106_survives_the_wrapper_too() {
        let ceiling = Timestamp {
            secs: u32::MAX as i64,
            nsec: 0,
        }
        .to_filetime();
        let past = Timestamp {
            secs: u32::MAX as i64 + 1,
            nsec: 0,
        }
        .to_filetime();
        assert!(
            past > ceiling,
            "past 2106 must be later than the u32 ceiling, not wrapped"
        );
    }

    /// A pre-1970 timestamp is representable in FILETIME (which counts
    /// from 1601), so it converts rather than clamping to the epoch.
    #[test]
    fn a_pre_1970_timestamp_converts_rather_than_clamping() {
        let epoch = Timestamp { secs: 0, nsec: 0 }.to_filetime();
        let before = Timestamp {
            secs: -86_400,
            nsec: 0,
        }
        .to_filetime();
        assert!(before < epoch, "1969 must be earlier than 1970");
        assert_eq!(before, epoch - 86_400 * FILETIME_TICKS_PER_SEC);
    }

    /// A read-only *volume* makes everything read-only, whatever the
    /// mode says — Windows must not offer an edit that will fail.
    #[test]
    fn a_readonly_volume_overrides_a_writable_mode() {
        assert_ne!(
            attributes_for(NodeKind::File, 0o777, true) & attr::READONLY,
            0
        );
    }

    /// A symlink is a reparse point and **not** also a directory, even
    /// when it points at one — WinFsp resolves it and asks again.
    #[test]
    fn a_symlink_is_a_reparse_point_and_not_a_directory() {
        let a = attributes_for(NodeKind::Symlink, 0o777, false);
        assert_ne!(a & attr::REPARSE_POINT, 0);
        assert_eq!(a & attr::DIRECTORY, 0);
    }

    /// `NORMAL` means "no other attributes", so it must never appear
    /// alongside one.
    #[test]
    fn normal_is_never_combined_with_another_attribute() {
        for kind in [
            NodeKind::Dir,
            NodeKind::File,
            NodeKind::Symlink,
            NodeKind::Other,
        ] {
            for ro in [true, false] {
                let a = attributes_for(kind, 0o644, ro);
                if a & attr::NORMAL != 0 {
                    assert_eq!(a, attr::NORMAL, "NORMAL must stand alone, got {a:#x}");
                }
            }
        }
    }
}

#[cfg(test)]
mod filetime_inverse_tests {
    use super::*;

    /// Zero is WinFSP's "leave unchanged" sentinel, and the ONLY input
    /// that has no timestamp. The drivers' own copies also returned
    /// `None` for pre-1970 times, which reads to the caller as "leave
    /// unchanged" and silently discards what the user asked for.
    #[test]
    fn only_zero_means_no_timestamp() {
        assert!(filetime_to_unix(0).is_none());
        assert!(filetime_to_unix(1).is_some());
        // A FILETIME well before 1970 — 1601 plus one second.
        assert!(filetime_to_unix(FILETIME_TICKS_PER_SEC).is_some());
    }

    /// Round-trips across the boundaries that the narrower copies could
    /// not express: before 1970, past 2038, and past 2106 where a `u32`
    /// return would clamp.
    #[test]
    fn conversion_round_trips_in_both_directions() {
        for secs in [
            i64::from(i32::MIN), // 1901-12-13, ext4's floor
            -315_619_200,        // 1960
            -1,                  // one second before the epoch
            0,
            946_684_800,             // 2000
            i64::from(i32::MAX),     // 2038-01-19
            i64::from(u32::MAX) + 1, // past where a u32 second count ends
            4_102_444_800,           // 2100
        ] {
            let ft = unix_to_filetime(secs, 0);
            let back = filetime_to_unix(ft).expect("only zero has no timestamp");
            assert_eq!(back.secs, secs, "seconds did not round trip for {secs}");
            assert_eq!(back.nsec, 0);
        }
    }

    /// Sub-second precision survives to FILETIME's own resolution of
    /// 100 ns, and no further — the remainder is lost in the format,
    /// not in this code.
    #[test]
    fn sub_second_precision_survives_to_a_hundred_nanoseconds() {
        let ft = unix_to_filetime(1, 999_999_900);
        let back = filetime_to_unix(ft).unwrap();
        assert_eq!(back.secs, 1);
        assert_eq!(back.nsec, 999_999_900);

        // Below 100 ns there is nowhere to put it.
        let rounded = filetime_to_unix(unix_to_filetime(1, 99)).unwrap();
        assert_eq!(rounded.nsec, 0);
    }
}
