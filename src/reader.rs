//! What a driver has to provide, so the skeleton stops needing to know
//! which filesystem it is hosting.
//!
//! # The problem this exists to fix
//!
//! `FsBackend` (in `lib.rs`) is four constants and `detect()`. That is
//! enough for the parts of a driver that only need to *name* the
//! filesystem: the service registration, the launcher class, the
//! right-click verb. It is not enough for `mount.rs`, which is where a
//! driver actually reads the volume — and so every driver's `mount.rs`
//! calls its own reader's API directly, at around thirty sites.
//!
//! That was measured rather than assumed. Copying `erofs-win-driver`
//! into an XFS driver and renaming every identifier produced a tree
//! eight compile errors from building, and **five of the eight were
//! this**: the readers do not merely differ in name, they differ in
//! *shape*.
//!
//! | | `am-fs-erofs` | `am-fs-xfs` |
//! |---|---|---|
//! | open | `open(dev)` | `mount(dev)` / `mount_rw(dev)` |
//! | read a file | `read_file(&inode, offset, &mut buf) -> Result<()>` | `read_file(&inode, raw: &[u8]) -> Result<Vec<u8>>` |
//! | read a directory | `read_dir(&inode)` | `read_dir(&inode, raw: &[u8])` |
//!
//! The extra `raw` is not a style difference to be normalised away. An
//! XFS inode carries its extents and inline data in the raw inode fork,
//! so the reader genuinely needs those bytes beside the parsed struct.
//! Any abstraction that refuses to carry them is wrong about XFS.
//!
//! # How the shape difference is absorbed
//!
//! [`FsReader::Node`] is an associated type. Whatever a reader needs to
//! carry between "I resolved this path" and "now read from it" goes in
//! there, and the skeleton never looks inside it. EROFS puts an inode
//! in it. XFS puts an inode *and* the raw fork bytes. A future reader
//! that needs a cursor, a lock guard or a cached extent list puts that
//! in it instead, and no code here changes.
//!
//! This is the division of labour with [`crate::translate`]: that
//! module absorbs differences in how a value is *represented* (a POSIX
//! mode versus Windows attribute bits, a Unix timestamp versus a
//! FILETIME). This one absorbs differences in the *shape of the calls*.
//! Together they are what a driver needs in order to stop being written
//! against one specific reader.
//!
//! # Why reading and writing are separate traits
//!
//! [`FsReader`] and [`FsWriter`] split because the drivers genuinely
//! do. `am-fs-erofs` mounts a read-only format and gets its writability
//! from an in-memory overlay; `am-fs-ext4` writes to the device. Folding
//! both into one trait would force the read-only driver to implement
//! mutating methods it can only answer with an error, which is how a
//! trait starts lying about what its implementors can do.

use std::sync::Arc;

use crate::translate::FileAttr;

/// Errors the skeleton can act on without knowing the filesystem.
///
/// Each driver's reader has its own error type, with variants that are
/// meaningful only for that format. The skeleton cannot act on those —
/// it has to answer WinFSP with an NTSTATUS — so a driver maps its
/// error into this small set and keeps the detail in [`Self::Other`]
/// for the log.
#[derive(Debug)]
pub enum FsError {
    /// No such path.
    NotFound,
    /// The path exists but is not the kind of thing the caller wanted —
    /// reading a directory as a file, or the reverse.
    NotExpectedKind,
    /// The volume is mounted read-only, or the operation would write to
    /// a format this driver only reads.
    ReadOnly,
    /// The image is malformed. Distinct from [`Self::Other`] because it
    /// says the *volume* is at fault rather than the request.
    Corrupt(String),
    /// Underlying device I/O failed.
    Io(std::io::Error),
    /// Anything else, carrying the driver's own message. The skeleton
    /// turns this into a generic failure status and logs the text.
    Other(String),
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no such file or directory"),
            Self::NotExpectedKind => write!(f, "not the expected kind of file"),
            Self::ReadOnly => write!(f, "read-only"),
            Self::Corrupt(why) => write!(f, "corrupt filesystem: {why}"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Other(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for FsError {}

impl From<std::io::Error> for FsError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Result alias for the trait methods below.
pub type FsResult<T> = Result<T, FsError>;

/// One entry of a directory listing.
///
/// Deliberately not "the reader's directory entry type". The skeleton
/// turns this straight into a WinFSP `FSP_FSCTL_DIR_INFO`, and needs
/// exactly a name and the attributes to fill that in. A reader whose
/// own entry type carries more — a hash, an on-disk offset, a record
/// number — keeps it; the skeleton has nothing to do with it.
pub struct DirEntry {
    /// Entry name, not a path. `.` and `..` are the reader's to include
    /// or omit: WinFSP wants them at the start of a listing, and
    /// [`FsReader::read_dir`] documents which convention applies.
    pub name: String,
    /// What the entry is and how big, already converted. Filling this
    /// during the directory walk avoids a second lookup per entry,
    /// which on a large directory is the difference between one pass
    /// and N.
    pub attr: FileAttr,
}

/// Read access to a mounted volume.
///
/// The skeleton owns one of these per mount and calls it from WinFSP's
/// callbacks. Implementations must be `Send + Sync`: WinFSP dispatches
/// callbacks from a thread pool, so several may be in flight at once.
pub trait FsReader: Send + Sync + 'static {
    /// Whatever this reader needs to carry from a resolved path to a
    /// subsequent read. See the module docs — this associated type is
    /// the entire reason the trait fits more than one reader.
    ///
    /// The skeleton treats it as opaque: it holds one per open handle
    /// and hands it back. It never inspects, compares or serialises it.
    type Node: Send + Sync;

    /// Mount the volume on `device`.
    ///
    /// `writable` asks for a read-write mount. A reader whose format
    /// this driver only reads should ignore it and let [`FsWriter`]'s
    /// absence speak, rather than failing the mount — a read-only mount
    /// of a writable request is still useful, and the skeleton checks
    /// writability separately.
    ///
    /// The device arrives as the skeleton's own [`BlockSource`], not a
    /// reader's device type. The readers all take `am-fs-core`'s
    /// `BlockRead`, and adapting between the two is the driver's job:
    /// it is one adapter per driver, against the alternative of this
    /// crate taking a dependency on `am-fs-core` purely to name a type
    /// in a signature.
    ///
    /// [`BlockSource`]: crate::device::BlockSource
    fn mount(device: Arc<dyn crate::device::BlockSource>, writable: bool) -> FsResult<Self>
    where
        Self: Sized;

    /// Resolve a Unix-style absolute path to a node.
    ///
    /// The path arrives already converted from the Windows form by
    /// [`crate::translate::winpath_to_unix`], so implementations get
    /// `/a/b/c` and never a backslash.
    fn lookup(&self, path: &str) -> FsResult<Self::Node>;

    /// Everything WinFSP needs to describe `node`.
    ///
    /// Infallible: by the time a node exists, its attributes have been
    /// read. A reader that would need further I/O here should do that
    /// work in [`Self::lookup`] and carry the result in `Node`.
    fn attr(&self, node: &Self::Node) -> FileAttr;

    /// Read up to `buf.len()` bytes from `offset`, returning how many
    /// were filled.
    ///
    /// A short return means end of file, not an error — WinFSP asks for
    /// a full buffer at the tail of a file as a matter of course.
    fn read_file(&self, node: &Self::Node, offset: u64, buf: &mut [u8]) -> FsResult<usize>;

    /// List a directory.
    ///
    /// Whether `.` and `..` appear is the reader's choice; the skeleton
    /// synthesises whichever are missing, because WinFSP requires both
    /// at the head of a listing and not every format stores them.
    fn read_dir(&self, node: &Self::Node) -> FsResult<Vec<DirEntry>>;

    /// The target of a symbolic link, as stored — which may be
    /// relative. Resolution is the caller's business.
    fn readlink(&self, node: &Self::Node) -> FsResult<String>;

    /// Volume label, if the format records one.
    fn volume_label(&self) -> Option<String> {
        None
    }

    /// Total and free bytes, for WinFSP's volume info.
    ///
    /// A read-only format reports free space of zero, which is honest:
    /// nothing can be written there, whatever the medium has spare.
    fn volume_size(&self) -> (u64, u64);
}

/// Write access, for the drivers whose reader has a write path.
///
/// Separate from [`FsReader`] so a read-only driver simply does not
/// implement it, rather than implementing mutating methods that can
/// only return an error. The skeleton uses the presence of an
/// implementation, not a runtime flag, to decide what it can offer.
pub trait FsWriter: FsReader {
    /// Write `data` at `offset`, returning how many bytes were stored.
    fn write_file(&self, node: &Self::Node, offset: u64, data: &[u8]) -> FsResult<usize>;

    /// Change a file's length, growing with zeroes or discarding the
    /// tail.
    fn set_size(&self, node: &Self::Node, size: u64) -> FsResult<()>;

    /// Create a file or directory at `path` and return its node.
    fn create(
        &self,
        path: &str,
        kind: crate::translate::NodeKind,
        perms: u16,
    ) -> FsResult<Self::Node>;

    /// Remove a file, symlink or empty directory.
    fn remove(&self, path: &str) -> FsResult<()>;

    /// Move `from` to `to` within the same volume.
    fn rename(&self, from: &str, to: &str) -> FsResult<()>;

    /// Apply whichever of these are `Some`, leaving the rest alone.
    fn set_attr(
        &self,
        node: &Self::Node,
        perms: Option<u16>,
        mtime: Option<crate::translate::Timestamp>,
        atime: Option<crate::translate::Timestamp>,
    ) -> FsResult<()>;

    /// Push pending writes to the device.
    fn flush(&self) -> FsResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translate::{NodeKind, Timestamp};

    /// A reader whose `Node` carries more than an identifier, which is
    /// the case the associated type exists for. If this stops
    /// compiling, the trait has grown an assumption about what a node
    /// is — the exact assumption that made the XFS port fail.
    struct NodeCarriesBytes {
        content: Vec<u8>,
    }

    struct BytesNode {
        offset: usize,
        len: usize,
        /// The part that matters: a reader is free to carry borrowed-
        /// shaped state — XFS carries the raw inode fork here.
        raw: Vec<u8>,
    }

    impl FsReader for NodeCarriesBytes {
        type Node = BytesNode;

        fn mount(_device: Arc<dyn crate::device::BlockSource>, _writable: bool) -> FsResult<Self> {
            Ok(Self {
                content: b"hello world".to_vec(),
            })
        }

        fn lookup(&self, path: &str) -> FsResult<Self::Node> {
            if path != "/hello" {
                return Err(FsError::NotFound);
            }
            Ok(BytesNode {
                offset: 0,
                len: self.content.len(),
                raw: self.content.clone(),
            })
        }

        fn attr(&self, node: &Self::Node) -> FileAttr {
            FileAttr {
                kind: NodeKind::File,
                perms: 0o644,
                size: node.len as u64,
                inode: 1,
                link_count: 1,
                mtime: Timestamp { secs: 0, nsec: 0 },
                atime: None,
                ctime: None,
                crtime: None,
            }
        }

        fn read_file(&self, node: &Self::Node, offset: u64, buf: &mut [u8]) -> FsResult<usize> {
            // Reads out of the node's OWN bytes, not the filesystem's —
            // proving the skeleton never had to look inside.
            let start = (node.offset as u64 + offset) as usize;
            if start >= node.raw.len() {
                return Ok(0);
            }
            let n = buf.len().min(node.raw.len() - start);
            buf[..n].copy_from_slice(&node.raw[start..start + n]);
            Ok(n)
        }

        fn read_dir(&self, _node: &Self::Node) -> FsResult<Vec<DirEntry>> {
            Err(FsError::NotExpectedKind)
        }

        fn readlink(&self, _node: &Self::Node) -> FsResult<String> {
            Err(FsError::NotExpectedKind)
        }

        fn volume_size(&self) -> (u64, u64) {
            (self.content.len() as u64, 0)
        }
    }

    /// Exercise the trait through a generic function, which is how the
    /// skeleton will use it. If the trait needed to know the concrete
    /// reader, this would not compile.
    fn read_whole<R: FsReader>(fs: &R, path: &str) -> FsResult<Vec<u8>> {
        let node = fs.lookup(path)?;
        let size = fs.attr(&node).size as usize;
        let mut out = vec![0u8; size];
        let mut done = 0;
        while done < size {
            let n = fs.read_file(&node, done as u64, &mut out[done..])?;
            if n == 0 {
                break;
            }
            done += n;
        }
        out.truncate(done);
        Ok(out)
    }

    #[test]
    fn a_node_may_carry_state_the_skeleton_never_sees() {
        let fs = NodeCarriesBytes {
            content: b"hello world".to_vec(),
        };
        assert_eq!(read_whole(&fs, "/hello").unwrap(), b"hello world");
    }

    #[test]
    fn a_missing_path_is_not_found() {
        let fs = NodeCarriesBytes {
            content: b"x".to_vec(),
        };
        assert!(matches!(
            read_whole(&fs, "/nope").unwrap_err(),
            FsError::NotFound
        ));
    }

    /// A short read means end of file, not failure. Getting this wrong
    /// is how a driver truncates the last block of every file it serves.
    #[test]
    fn reading_past_the_end_returns_zero_not_an_error() {
        let fs = NodeCarriesBytes {
            content: b"abc".to_vec(),
        };
        let node = fs.lookup("/hello").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(fs.read_file(&node, 99, &mut buf).unwrap(), 0);
    }

    /// The error text reaches a log, so it has to say something.
    #[test]
    fn errors_render_usefully() {
        assert_eq!(FsError::NotFound.to_string(), "no such file or directory");
        assert_eq!(
            FsError::Corrupt("bad magic".into()).to_string(),
            "corrupt filesystem: bad magic"
        );
        assert_eq!(FsError::Other("boom".into()).to_string(), "boom");
    }
}
