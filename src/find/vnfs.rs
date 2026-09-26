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

use vnfs::{AttrMask, DummyVecFs, NfsVecFs, VecFs, VfAttrs, VfFile, VfType};

use super::matchers::{VfsMeta, WalkEntry};
use super::{Config, Follow};

enum Backend {
    Dummy(DummyVecFs),
    Nfs(Box<NfsVecFs>),
}

impl Backend {
    fn lstat(&mut self, path: &Path, masks: AttrMask) -> Result<VfAttrs, vnfs::VfError> {
        let mut attrs = VfAttrs {
            file: VfFile::from_os_path(path),
            masks,
            ..VfAttrs::default()
        };
        match self {
            Self::Dummy(fs) => fs.lgetattrsv(std::slice::from_mut(&mut attrs))?,
            Self::Nfs(fs) => fs.lgetattrsv(std::slice::from_mut(&mut attrs))?,
        }
        Ok(attrs)
    }

    fn listdir(&mut self, path: &Path, masks: AttrMask) -> Result<Vec<VfAttrs>, vnfs::VfError> {
        match self {
            Self::Dummy(fs) => fs.listdir(path, masks, 0, true),
            Self::Nfs(fs) => fs.listdir(path, masks, 0, true),
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

fn attr_mask() -> AttrMask {
    AttrMask::MODE
        | AttrMask::SIZE
        | AttrMask::NLINK
        | AttrMask::FILEID
        | AttrMask::BLOCKS
        | AttrMask::UID
        | AttrMask::GID
        | AttrMask::ATIME
        | AttrMask::MTIME
        | AttrMask::CTIME
}

/// One deferred traversal step. `Emit` yields an entry; `Enter` expands a
/// directory into its children.
enum Action {
    Emit {
        path: PathBuf,
        depth: usize,
        attrs: VfAttrs,
    },
    Enter {
        path: PathBuf,
        depth: usize,
        attrs: VfAttrs,
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
        "dummy" => Backend::Dummy(DummyVecFs::try_new(mount.point.clone()).ok()?),
        "nfs" => Backend::Nfs(Box::new(NfsVecFs::connect(&mount.server).ok()?)),
        _ => return None,
    };

    enumerate_backend(&mut backend, &typed_root, &vroot, config).ok()
}

/// Enumerate `vroot` through `backend`, mapping emitted paths back onto
/// `typed_root`. Ordering matches `walkdir`: pre-order by default, post-order
/// with `-depth`, and each directory's children sorted with `-s`.
fn enumerate_backend(
    backend: &mut Backend,
    typed_root: &Path,
    vroot: &Path,
    config: &Config,
) -> Result<Vec<WalkEntry>, vnfs::VfError> {
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
    if root_attrs.ftype != VfType::Directory {
        return Ok(vec![WalkEntry::from_vfs(
            typed_root.to_path_buf(),
            0,
            Follow::Never,
            VfsMeta::from_attrs(&root_attrs),
        )]);
    }

    let entries = backend.listdir(vroot, masks)?;
    let mut children: HashMap<PathBuf, Vec<VfAttrs>> = HashMap::new();
    for entry in entries {
        if let Some(parent) = entry.file.path().and_then(Path::parent) {
            children
                .entry(parent.to_path_buf())
                .or_default()
                .push(entry);
        }
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
                        VfsMeta::from_attrs(&attrs),
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
                let mut kids: Vec<VfAttrs> = children.remove(&path).unwrap_or_default();
                if config.sorted_output {
                    kids.sort_by(|a, b| {
                        a.file
                            .path()
                            .and_then(Path::file_name)
                            .cmp(&b.file.path().and_then(Path::file_name))
                    });
                }
                let child_actions = kids.into_iter().map(|kid| {
                    let child_path = kid
                        .file
                        .path()
                        .map_or_else(|| path.clone(), Path::to_path_buf);
                    if kid.ftype == VfType::Directory {
                        Action::Enter {
                            path: child_path,
                            depth: depth + 1,
                            attrs: kid,
                        }
                    } else {
                        Action::Emit {
                            path: child_path,
                            depth: depth + 1,
                            attrs: kid,
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
        let mut backend =
            Backend::Dummy(DummyVecFs::try_new(root.to_path_buf()).expect("dummy root"));
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
