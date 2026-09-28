// This file is part of the uutils findutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! Opt-in VFSI/NFS traversal for `find`.
//!
//! When `VNFS_IMPL=dummy|nfs` and the search root lies on an NFS mount, the
//! tree is enumerated and attributes are read through the `vnfs` API, which
//! returns file attributes in the `READDIR` replies instead of issuing one
//! kernel `lstat` per entry. The resulting `WalkEntry` values carry that
//! metadata (`Meta::Vfs`), so the existing matchers run unchanged.
//!
//! Only the default `-P` (never follow symlinks) mode is handled; `-L`/`-H`
//! and `-x` fall back to `walkdir` in the parent module. Any backend failure
//! also falls back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use vnfs::{
    DirEntry, DirectoryListing, Metadata, MetadataFields, Mounted, Nfs, NfsClient, VfError, VfType,
    WalkOptions,
};

use super::matchers::{VfsMeta, WalkEntry};
use super::{Config, Follow};

enum Backend {
    Dummy(Mounted),
    Nfs(NfsClient),
}

impl Backend {
    fn lstat(&self, path: &Path, fields: MetadataFields) -> Result<Metadata, VfError> {
        match self {
            Self::Dummy(fs) => fs.symlink_metadata_with_fields(path, fields),
            Self::Nfs(fs) => fs.symlink_metadata_with_fields(path, fields),
        }
    }

    fn walk(
        &self,
        path: &Path,
        fields: MetadataFields,
        max_depth: usize,
    ) -> Result<Vec<DirectoryListing>, VfError> {
        let default = WalkOptions::new();
        let options = if max_depth <= default.depth_limit() {
            default.max_depth(max_depth).truncate_at_max_depth(true)
        } else {
            default
        };
        match self {
            Self::Dummy(fs) => fs.walk_with_options(path, fields, options),
            Self::Nfs(fs) => fs.walk_with_options(path, fields, options),
        }
    }
}

struct Mount {
    server: String,
    point: PathBuf,
}

fn find_mount(path: &Path) -> Option<Mount> {
    let text = std::fs::read_to_string("/proc/self/mounts").ok()?;
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let spec = fields.next()?;
            let point = PathBuf::from(fields.next()?);
            let fstype = fields.next()?;
            if (fstype != "nfs" && fstype != "nfs4") || !path.starts_with(&point) {
                return None;
            }
            let (server, _) = spec.rsplit_once(':')?;
            Some(Mount {
                server: server.trim_matches(['[', ']']).to_owned(),
                point,
            })
        })
        .max_by_key(|mount| mount.point.as_os_str().len())
}

/// Whether `VNFS_IMPL` selects a vectorized backend.
pub fn is_enabled() -> bool {
    matches!(std::env::var("VNFS_IMPL").as_deref(), Ok("dummy" | "nfs"))
}

/// Whether this traversal can reproduce `find` semantics for this config.
pub fn supports(config: &Config) -> bool {
    // Following symlinks needs walkdir's cycle-safe logic, and `-x` needs
    // device comparisons the NFS attribute set does not carry.
    config.follow == Follow::Never && !config.same_file_system
}

fn attr_mask() -> MetadataFields {
    MetadataFields::MODE
        | MetadataFields::SIZE
        | MetadataFields::NLINK
        | MetadataFields::FILEID
        | MetadataFields::BLOCKS
        | MetadataFields::UID
        | MetadataFields::GID
        | MetadataFields::ATIME
        | MetadataFields::MTIME
        | MetadataFields::CTIME
}

/// One deferred traversal step. `Emit` yields an entry; `Enter` expands a
/// directory into its children.
enum Action {
    Emit {
        path: PathBuf,
        depth: usize,
        attrs: Metadata,
    },
    Enter {
        path: PathBuf,
        depth: usize,
        attrs: Metadata,
    },
}

/// Enumerate the tree rooted at `dir` through VFSI. Returns `None` when the
/// root is not on an NFS mount, the backend is disabled, or the backend fails
/// (the caller then uses `walkdir`).
pub fn enumerate(dir: &str, config: &Config) -> Option<Vec<WalkEntry>> {
    let choice = std::env::var("VNFS_IMPL").ok()?;
    let typed_root = PathBuf::from(dir);
    let resolved = typed_root.canonicalize().ok()?;
    let mount = find_mount(&resolved)?;
    let relative = resolved.strip_prefix(&mount.point).ok()?;
    let vroot = Path::new("/").join(relative);

    let mut backend = match choice.as_str() {
        "dummy" => Backend::Dummy(Mounted::new(&mount.point).ok()?),
        "nfs" => Backend::Nfs(Nfs::connect(&mount.server).ok()?),
        _ => return None,
    };

    enumerate_backend(&mut backend, &typed_root, &vroot, config).ok()
}

/// Enumerate `vroot` through `backend`, mapping emitted paths back onto
/// `typed_root`. Ordering matches `walkdir`: pre-order by default, post-order
/// with `-depth`, and each directory's children sorted with `-s`.
fn enumerate_backend(
    backend: &Backend,
    typed_root: &Path,
    vroot: &Path,
    config: &Config,
) -> Result<Vec<WalkEntry>, VfError> {
    let masks = attr_mask();
    let root_attrs = backend.lstat(vroot, masks)?;

    let print_path = |backend_path: &Path| -> PathBuf {
        match backend_path.strip_prefix(vroot) {
            Ok(rest) if rest.as_os_str().is_empty() => typed_root.to_path_buf(),
            Ok(rest) => typed_root.join(rest),
            Err(_) => typed_root.to_path_buf(),
        }
    };

    // A non-directory root has no descendants.
    if root_attrs.file_type() != VfType::Directory {
        return Ok(vec![WalkEntry::from_vfs(
            typed_root.to_path_buf(),
            0,
            Follow::Never,
            VfsMeta::from_metadata(&root_attrs),
        )]);
    }

    let directories = backend.walk(vroot, masks, config.max_depth)?;
    let mut children: HashMap<PathBuf, Vec<DirEntry>> = HashMap::new();
    for directory in directories {
        children.insert(directory.path, directory.entries);
    }

    let mut out = Vec::new();
    let mut stack = vec![Action::Enter {
        path: vroot.to_path_buf(),
        depth: 0,
        attrs: root_attrs,
    }];
    while let Some(action) = stack.pop() {
        match action {
            Action::Emit { path, depth, attrs } => {
                if depth >= config.min_depth && depth <= config.max_depth {
                    out.push(WalkEntry::from_vfs(
                        print_path(&path),
                        depth,
                        Follow::Never,
                        VfsMeta::from_metadata(&attrs),
                    ));
                }
            }
            Action::Enter { path, depth, attrs } => {
                let dir = Action::Emit {
                    path: path.clone(),
                    depth,
                    attrs,
                };
                if depth >= config.max_depth {
                    // Do not descend past -maxdepth.
                    stack.push(dir);
                    continue;
                }
                let mut kids: Vec<DirEntry> = children.remove(&path).unwrap_or_default();
                if config.sorted_output {
                    kids.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
                }
                let child_actions = kids.into_iter().map(|kid| {
                    let child_path = kid.path().to_path_buf();
                    let attrs = kid.metadata().clone();
                    if kid.file_type() == VfType::Directory {
                        Action::Enter {
                            path: child_path,
                            depth: depth + 1,
                            attrs,
                        }
                    } else {
                        Action::Emit {
                            path: child_path,
                            depth: depth + 1,
                            attrs,
                        }
                    }
                });
                // The stack is LIFO: push in reverse execution order.
                if config.depth_first {
                    // Contents first, then the directory.
                    stack.push(dir);
                    for child in child_actions.collect::<Vec<_>>().into_iter().rev() {
                        stack.push(child);
                    }
                } else {
                    // Directory first, then its contents.
                    for child in child_actions.collect::<Vec<_>>().into_iter().rev() {
                        stack.push(child);
                    }
                    stack.push(dir);
                }
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(root: &Path) {
        for (path, body) in [
            ("a.txt", &b"a"[..]),
            ("sub/b.txt", b"b"),
            ("sub/deep/c.txt", b"c"),
        ] {
            let p = root.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
    }

    fn names(entries: &[WalkEntry], prefix: &Path) -> Vec<String> {
        entries
            .iter()
            .map(|e| {
                e.path()
                    .strip_prefix(prefix)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    fn enumerate_root(root: &Path, config: &Config) -> Vec<WalkEntry> {
        let mut backend = Backend::Dummy(Mounted::new(root).expect("dummy root"));
        enumerate_backend(&mut backend, root, Path::new("/"), config).expect("enumerate")
    }

    #[test]
    fn preorder_is_parent_before_child() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let entries = enumerate_root(dir.path(), &Config::default());
        assert_eq!(entries[0].path(), dir.path());
        assert_eq!(entries[0].depth(), 0);
        for (i, entry) in entries.iter().enumerate() {
            if entry.depth() == 0 {
                continue;
            }
            let parent = entry.path().parent().unwrap();
            assert!(
                entries[..i]
                    .iter()
                    .any(|e| e.path() == parent && e.depth() + 1 == entry.depth()),
                "parent of {:?} must appear first",
                entry.path()
            );
        }
    }

    #[test]
    fn depth_first_is_post_order() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let config = Config {
            depth_first: true,
            ..Config::default()
        };
        let entries = enumerate_root(dir.path(), &config);
        let got = names(&entries, dir.path());
        assert_eq!(got.last().unwrap(), "");
        let sub = got.iter().position(|n| n == "sub").unwrap();
        let deep = got.iter().position(|n| n == "sub/deep").unwrap();
        let c = got.iter().position(|n| n == "sub/deep/c.txt").unwrap();
        assert!(c < deep && deep < sub);
    }

    #[test]
    fn sorted_output_orders_children_by_name() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let config = Config {
            sorted_output: true,
            ..Config::default()
        };
        let entries = enumerate_root(dir.path(), &config);
        assert_eq!(
            names(&entries, dir.path()),
            [
                "",
                "a.txt",
                "sub",
                "sub/b.txt",
                "sub/deep",
                "sub/deep/c.txt"
            ]
        );
    }

    #[test]
    fn depth_bounds_are_respected() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let config = Config {
            max_depth: 1,
            ..Config::default()
        };
        let entries = enumerate_root(dir.path(), &config);
        assert!(entries.iter().all(|e| e.depth() <= 1));

        let config = Config {
            min_depth: 2,
            ..Config::default()
        };
        let entries = enumerate_root(dir.path(), &config);
        assert!(entries.iter().all(|e| e.depth() >= 2));
        assert!(!names(&entries, dir.path()).is_empty());
    }

    #[test]
    fn vfs_metadata_comes_from_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let entries = enumerate_root(dir.path(), &Config::default());
        let a = entries
            .iter()
            .find(|e| e.path().ends_with("a.txt"))
            .unwrap();
        assert!(a.file_type().is_file());
        assert_eq!(a.metadata().unwrap().len(), 1);
    }
}
