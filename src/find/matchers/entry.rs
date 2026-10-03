//! Paths encountered during a walk.

use std::cell::OnceCell;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt::{self, Display, Formatter};
use std::fs::{self, Metadata};
use std::io::{self, ErrorKind};
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use walkdir::DirEntry;

use super::Follow;

/// Wrapper for a directory entry.
#[derive(Debug)]
enum Entry {
    /// Wraps an explicit path and depth.
    Explicit(PathBuf, usize),
    /// Wraps a WalkDir entry.
    WalkDir(DirEntry),
}

/// File types.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FileType {
    Unknown,
    Fifo,
    CharDevice,
    Directory,
    BlockDevice,
    Regular,
    Symlink,
    Socket,
}

impl FileType {
    pub fn is_dir(self) -> bool {
        self == Self::Directory
    }

    pub fn is_file(self) -> bool {
        self == Self::Regular
    }

    pub fn is_symlink(self) -> bool {
        self == Self::Symlink
    }
}

impl From<fs::FileType> for FileType {
    fn from(t: fs::FileType) -> Self {
        if t.is_dir() {
            return Self::Directory;
        }
        if t.is_file() {
            return Self::Regular;
        }
        if t.is_symlink() {
            return Self::Symlink;
        }

        #[cfg(unix)]
        {
            if t.is_fifo() {
                return Self::Fifo;
            }
            if t.is_char_device() {
                return Self::CharDevice;
            }
            if t.is_block_device() {
                return Self::BlockDevice;
            }
            if t.is_socket() {
                return Self::Socket;
            }
        }

        Self::Unknown
    }
}

/// Metadata for an entry whose attributes came from a VFSI directory listing
/// rather than a kernel `stat`.
#[derive(Clone, Debug)]
pub struct VfsMeta {
    ptype: FileType,
    len: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u64,
    ino: u64,
    blocks: u64,
    atime: (i64, u32),
    mtime: (i64, u32),
    ctime: (i64, u32),
}

#[cfg(all(target_os = "linux", feature = "vnfs"))]
impl VfsMeta {
    pub fn from_metadata(attrs: &vnfs::Metadata) -> Self {
        Self {
            ptype: FileType::from(attrs.file_type()),
            len: attrs.len(),
            mode: attrs.mode().unwrap_or_default(),
            uid: attrs.uid().unwrap_or_default(),
            gid: attrs.gid().unwrap_or_default(),
            nlink: u64::from(attrs.nlink().unwrap_or_default()),
            ino: attrs.file_id().unwrap_or_default(),
            blocks: attrs.blocks().unwrap_or_default(),
            atime: attrs.accessed().map_or((0, 0), system_time_parts),
            mtime: attrs.modified().map_or((0, 0), system_time_parts),
            ctime: attrs.changed().map_or((0, 0), system_time_parts),
        }
    }
}

#[cfg(all(target_os = "linux", feature = "vnfs"))]
fn system_time_parts(time: std::time::SystemTime) -> (i64, u32) {
    use std::time::UNIX_EPOCH;
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs() as i64, duration.subsec_nanos()),
        Err(error) => {
            let duration = error.duration();
            let seconds = duration.as_secs() as i64;
            if duration.subsec_nanos() == 0 {
                (-seconds, 0)
            } else {
                (-seconds - 1, 1_000_000_000 - duration.subsec_nanos())
            }
        }
    }
}

#[cfg(all(target_os = "linux", feature = "vnfs"))]
impl From<vnfs::FileType> for FileType {
    fn from(t: vnfs::FileType) -> Self {
        match t {
            vnfs::FileType::Regular => Self::Regular,
            vnfs::FileType::Directory => Self::Directory,
            vnfs::FileType::Symlink => Self::Symlink,
            vnfs::FileType::BlockDevice => Self::BlockDevice,
            vnfs::FileType::CharDevice => Self::CharDevice,
            vnfs::FileType::Fifo => Self::Fifo,
            vnfs::FileType::Socket => Self::Socket,
            vnfs::FileType::Other(_) => Self::Unknown,
        }
    }
}

/// Convert an NFS `(seconds, nanoseconds)` timestamp to a `SystemTime`.
fn system_time(seconds: i64, nanos: u32) -> SystemTime {
    let base = if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(seconds.unsigned_abs()))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
    }
    .unwrap_or(UNIX_EPOCH);
    base.checked_add(Duration::from_nanos(u64::from(nanos)))
        .unwrap_or(base)
}

/// Metadata for a walked entry: either the kernel's `std::fs::Metadata` or the
/// fields carried by a VFSI/NFS directory listing.
#[derive(Clone, Debug)]
pub enum Meta {
    Std(Metadata),
    Vfs(VfsMeta),
}

impl Meta {
    pub fn file_type(&self) -> FileType {
        match self {
            Self::Std(m) => m.file_type().into(),
            Self::Vfs(v) => v.ptype,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }

    pub fn is_file(&self) -> bool {
        self.file_type().is_file()
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type().is_symlink()
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        match self {
            Self::Std(m) => m.len(),
            Self::Vfs(v) => v.len,
        }
    }

    /// `st_size`, mirroring `std::os::unix::fs::MetadataExt::size`.
    #[cfg(unix)]
    pub fn size(&self) -> u64 {
        self.len()
    }

    #[cfg(unix)]
    pub fn mode(&self) -> u32 {
        match self {
            Self::Std(m) => m.mode(),
            Self::Vfs(v) => v.mode,
        }
    }

    #[cfg(unix)]
    pub fn uid(&self) -> u32 {
        match self {
            Self::Std(m) => m.uid(),
            Self::Vfs(v) => v.uid,
        }
    }

    #[cfg(unix)]
    pub fn gid(&self) -> u32 {
        match self {
            Self::Std(m) => m.gid(),
            Self::Vfs(v) => v.gid,
        }
    }

    #[cfg(unix)]
    pub fn nlink(&self) -> u64 {
        match self {
            Self::Std(m) => m.nlink(),
            Self::Vfs(v) => v.nlink,
        }
    }

    #[cfg(unix)]
    pub fn ino(&self) -> u64 {
        match self {
            Self::Std(m) => m.ino(),
            Self::Vfs(v) => v.ino,
        }
    }

    #[cfg(unix)]
    pub fn dev(&self) -> u64 {
        match self {
            Self::Std(m) => m.dev(),
            // A single NFS export is one device.
            Self::Vfs(_) => 0,
        }
    }

    #[cfg(unix)]
    pub fn blocks(&self) -> u64 {
        match self {
            Self::Std(m) => m.blocks(),
            Self::Vfs(v) => v.blocks,
        }
    }

    #[cfg(unix)]
    pub fn atime(&self) -> i64 {
        match self {
            Self::Std(m) => m.atime(),
            Self::Vfs(v) => v.atime.0,
        }
    }

    #[cfg(unix)]
    pub fn atime_nsec(&self) -> i64 {
        match self {
            Self::Std(m) => m.atime_nsec(),
            Self::Vfs(v) => i64::from(v.atime.1),
        }
    }

    #[cfg(unix)]
    pub fn mtime(&self) -> i64 {
        match self {
            Self::Std(m) => m.mtime(),
            Self::Vfs(v) => v.mtime.0,
        }
    }

    #[cfg(unix)]
    pub fn mtime_nsec(&self) -> i64 {
        match self {
            Self::Std(m) => m.mtime_nsec(),
            Self::Vfs(v) => i64::from(v.mtime.1),
        }
    }

    #[cfg(unix)]
    pub fn ctime(&self) -> i64 {
        match self {
            Self::Std(m) => m.ctime(),
            Self::Vfs(v) => v.ctime.0,
        }
    }

    #[cfg(unix)]
    pub fn ctime_nsec(&self) -> i64 {
        match self {
            Self::Std(m) => m.ctime_nsec(),
            Self::Vfs(v) => i64::from(v.ctime.1),
        }
    }

    #[cfg(unix)]
    pub fn permissions(&self) -> Perms {
        match self {
            Self::Std(m) => Perms(m.permissions().mode() & 0o7777),
            Self::Vfs(v) => Perms(v.mode & 0o7777),
        }
    }

    #[cfg(windows)]
    pub fn file_attributes(&self) -> u32 {
        use std::os::windows::fs::MetadataExt;
        match self {
            Self::Std(m) => m.file_attributes(),
            Self::Vfs(_) => 0,
        }
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        match self {
            Self::Std(m) => m.modified(),
            Self::Vfs(v) => Ok(system_time(v.mtime.0, v.mtime.1)),
        }
    }

    pub fn accessed(&self) -> io::Result<SystemTime> {
        match self {
            Self::Std(m) => m.accessed(),
            Self::Vfs(v) => Ok(system_time(v.atime.0, v.atime.1)),
        }
    }

    pub fn created(&self) -> io::Result<SystemTime> {
        match self {
            Self::Std(m) => m.created(),
            // NFS does not expose a creation timestamp here.
            Self::Vfs(_) => Err(io::Error::new(
                ErrorKind::Unsupported,
                "creation time is not available",
            )),
        }
    }
}

/// Permission bits extracted from an entry's metadata.
#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
pub struct Perms(u32);

#[cfg(unix)]
impl Perms {
    pub fn mode(self) -> u32 {
        self.0
    }
}

/// An error encountered while walking a file system.
#[derive(Clone, Debug)]
pub struct WalkError {
    /// The path that caused the error, if known.
    path: Option<PathBuf>,
    /// The depth below the root path, if known.
    depth: Option<usize>,
    /// The io::Error::raw_os_error(), if known.
    raw: Option<i32>,
}

impl WalkError {
    /// Get the path this error occurred on, if known.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Get the traversal depth when this error occurred, if known.
    pub fn depth(&self) -> Option<usize> {
        self.depth
    }

    /// Get the kind of I/O error.
    pub fn kind(&self) -> ErrorKind {
        io::Error::from(self).kind()
    }

    /// Check for ErrorKind::{NotFound,NotADirectory}.
    pub fn is_not_found(&self) -> bool {
        if self.kind() == ErrorKind::NotFound {
            return true;
        }

        // NotADirectory is nightly-only
        #[cfg(unix)]
        {
            if self.raw == Some(uucore::libc::ENOTDIR) {
                return true;
            }
        }

        false
    }

    /// Check for ErrorKind::FilesystemLoop.
    pub fn is_loop(&self) -> bool {
        #[cfg(unix)]
        return self.raw == Some(uucore::libc::ELOOP);

        #[cfg(not(unix))]
        return false;
    }
}

impl Display for WalkError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        let ioe = io::Error::from(self);
        if let Some(path) = &self.path {
            write!(f, "{}: {}", path.display(), ioe)
        } else {
            write!(f, "{}", ioe)
        }
    }
}

impl Error for WalkError {}

impl From<io::Error> for WalkError {
    fn from(e: io::Error) -> Self {
        Self::from(&e)
    }
}

impl From<&io::Error> for WalkError {
    fn from(e: &io::Error) -> Self {
        Self {
            path: None,
            depth: None,
            raw: e.raw_os_error(),
        }
    }
}

impl From<walkdir::Error> for WalkError {
    fn from(e: walkdir::Error) -> Self {
        Self::from(&e)
    }
}

impl From<&walkdir::Error> for WalkError {
    fn from(e: &walkdir::Error) -> Self {
        Self {
            path: e.path().map(std::borrow::ToOwned::to_owned),
            depth: Some(e.depth()),
            raw: e.io_error().and_then(std::io::Error::raw_os_error),
        }
    }
}

impl From<WalkError> for io::Error {
    fn from(e: WalkError) -> Self {
        Self::from(&e)
    }
}

impl From<&WalkError> for io::Error {
    fn from(e: &WalkError) -> Self {
        e.raw
            .map_or_else(|| ErrorKind::Other.into(), Self::from_raw_os_error)
    }
}

/// A path encountered while walking a file system.
#[derive(Debug)]
pub struct WalkEntry {
    /// The wrapped path/dirent.
    inner: Entry,
    /// Whether to follow symlinks.
    follow: Follow,
    /// Cached metadata.
    meta: OnceCell<Result<Meta, WalkError>>,
}

impl WalkEntry {
    /// Create a new WalkEntry for a specific file.
    pub fn new(path: impl Into<PathBuf>, depth: usize, follow: Follow) -> Self {
        Self {
            inner: Entry::Explicit(path.into(), depth),
            follow,
            meta: OnceCell::new(),
        }
    }

    /// Create a WalkEntry whose metadata comes from a VFSI/NFS directory
    /// listing instead of a kernel `stat`.
    #[cfg(all(target_os = "linux", feature = "vnfs"))]
    pub fn from_vfs(path: impl Into<PathBuf>, depth: usize, follow: Follow, meta: VfsMeta) -> Self {
        Self {
            inner: Entry::Explicit(path.into(), depth),
            follow,
            meta: Ok(Meta::Vfs(meta)).into(),
        }
    }

    /// Convert a [walkdir::DirEntry] to a [WalkEntry].  Errors due to broken symbolic links will be
    /// converted to valid entries, but other errors will be propagated.
    pub fn from_walkdir(
        result: walkdir::Result<DirEntry>,
        follow: Follow,
    ) -> Result<Self, WalkError> {
        let result = result.map_err(WalkError::from);

        match result {
            Ok(entry) => {
                let ret = if entry.depth() == 0 && follow != Follow::Never {
                    // DirEntry::file_type() is wrong for root symlinks when follow_root_links is set
                    Self::new(entry.path(), 0, follow)
                } else {
                    Self {
                        inner: Entry::WalkDir(entry),
                        follow,
                        meta: OnceCell::new(),
                    }
                };
                Ok(ret)
            }
            Err(e) if e.is_not_found() => {
                // Detect broken symlinks and replace them with explicit entries
                if let (Some(path), Some(depth)) = (e.path(), e.depth()) {
                    if let Ok(meta) = path.symlink_metadata() {
                        return Ok(Self {
                            inner: Entry::Explicit(path.into(), depth),
                            follow: Follow::Never,
                            meta: Ok(Meta::Std(meta)).into(),
                        });
                    }
                }

                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// Get the path to this entry.
    pub fn path(&self) -> &Path {
        match &self.inner {
            Entry::Explicit(path, _) => path.as_path(),
            Entry::WalkDir(ent) => ent.path(),
        }
    }

    /// Get the path to this entry.
    pub fn into_path(self) -> PathBuf {
        match self.inner {
            Entry::Explicit(path, _) => path,
            Entry::WalkDir(ent) => ent.into_path(),
        }
    }

    /// Get the name of this entry.
    pub fn file_name(&self) -> &OsStr {
        match &self.inner {
            Entry::Explicit(path, _) => {
                // Path::file_name() only works if the last component is normal
                path.components()
                    .next_back()
                    .map_or_else(|| path.as_os_str(), std::path::Component::as_os_str)
            }
            Entry::WalkDir(ent) => ent.file_name(),
        }
    }

    /// Get the depth of this entry below the root.
    pub fn depth(&self) -> usize {
        match &self.inner {
            Entry::Explicit(_, depth) => *depth,
            Entry::WalkDir(ent) => ent.depth(),
        }
    }

    /// Get whether symbolic links are followed for this entry.
    pub fn follow(&self) -> bool {
        self.follow.follow_at_depth(self.depth())
    }

    /// Get the metadata on a cache miss.
    fn get_metadata(&self) -> Result<Meta, WalkError> {
        self.follow.metadata_at_depth(self.path(), self.depth())
    }

    /// Get the metadata for this entry, following symbolic links if appropriate.
    /// Multiple calls to this function will cache and re-use the same metadata.
    pub fn metadata(&self) -> Result<&Meta, WalkError> {
        let result = self.meta.get_or_init(|| match &self.inner {
            Entry::Explicit(_, _) => self.get_metadata(),
            Entry::WalkDir(ent) => Ok(Meta::Std(ent.metadata()?)),
        });
        result.as_ref().map_err(std::clone::Clone::clone)
    }

    /// Get the file type of this entry.
    pub fn file_type(&self) -> FileType {
        match &self.inner {
            Entry::Explicit(_, _) => self.metadata().map_or(FileType::Unknown, Meta::file_type),
            Entry::WalkDir(ent) => ent.file_type().into(),
        }
    }

    /// Check whether this entry is a symbolic link, regardless of whether links
    /// are being followed.
    pub fn path_is_symlink(&self) -> bool {
        match &self.inner {
            Entry::Explicit(path, _) => {
                if self.follow() {
                    path.symlink_metadata()
                        .is_ok_and(|m| m.file_type().is_symlink())
                } else {
                    self.file_type().is_symlink()
                }
            }
            Entry::WalkDir(ent) => ent.path_is_symlink(),
        }
    }
}
