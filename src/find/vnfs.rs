//! Incremental VFSI traversal. Pruning runs before reading a directory;
//! once matching has begun, failures never replay side effects via walkdir.
use super::matchers::WalkEntry;
use super::{Config, Follow};
use std::path::{Path, PathBuf};
use vnfs::{Attributes, Mounted, Nfs, Vfsi, VfsiExt, WalkControl, WalkEventKind};

pub fn is_enabled() -> bool {
    matches!(std::env::var("VNFS_IMPL").as_deref(), Ok("dummy" | "nfs"))
}
pub fn supports(config: &Config) -> bool {
    config.follow == Follow::Never && !config.same_file_system
}

pub fn visit(
    dir: &str,
    config: &Config,
    fields: Attributes,
    callback: impl FnMut(WalkEntry) -> WalkControl,
) -> Option<vnfs::Result<()>> {
    let typed = Path::new(dir);
    let (base, vroot) = mount_operand(typed)?;
    match std::env::var("VNFS_IMPL").as_deref() {
        Ok("dummy") => Some(visit_backend(
            &Mounted::new(&base).ok()?,
            typed,
            &vroot,
            config,
            fields,
            callback,
        )),
        Ok("nfs") => Some(visit_backend(
            &Nfs::from_mount(&base).ok()?,
            typed,
            &vroot,
            config,
            fields,
            callback,
        )),
        _ => None,
    }
}

fn mount_operand(typed: &Path) -> Option<(PathBuf, PathBuf)> {
    // Canonicalize the parent, not the final symlink: -P must report the link.
    let (base, vroot) = if std::fs::symlink_metadata(typed).ok()?.is_dir() {
        // A mount-point operand must be discovered on that mount, not its
        // local parent. Only actual directories are canonicalized here.
        (typed.canonicalize().ok()?, PathBuf::from("/"))
    } else if let Some(name) = typed.file_name() {
        let parent = typed
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        (parent.canonicalize().ok()?, Path::new("/").join(name))
    } else {
        (typed.canonicalize().ok()?, PathBuf::from("/"))
    };
    Some((base, vroot))
}

fn visit_backend<C: Vfsi>(
    client: &C,
    typed: &Path,
    vroot: &Path,
    config: &Config,
    fields: Attributes,
    mut callback: impl FnMut(WalkEntry) -> WalkControl,
) -> vnfs::Result<()> {
    let defaults = client.limits().walk_options();
    let options = if config.max_depth <= defaults.depth_limit() {
        defaults
            .max_depth(config.max_depth)
            .truncate_at_max_depth(true)
    } else {
        defaults
    };
    client
        .listdir(
            vroot,
            options
                .fields(fields)
                .recursive(true)
                .enter_leave(true)
                .sort_by_name(config.sorted_output),
            |event| {
                let emit = match event.kind {
                    WalkEventKind::Entry => true,
                    WalkEventKind::Enter => !config.depth_first,
                    WalkEventKind::Leave => config.depth_first,
                };
                // listdir limits directory descent; find's -maxdepth also
                // limits emitted files. Prune at the boundary, before listing.
                let boundary =
                    event.kind == WalkEventKind::Enter && event.depth >= config.max_depth;
                if !emit || event.depth < config.min_depth || event.depth > config.max_depth {
                    return Ok(if boundary {
                        WalkControl::SkipSubtree
                    } else {
                        WalkControl::Continue
                    });
                }
                let relative = event
                    .entry
                    .path()
                    .strip_prefix(vroot)
                    .map_err(|_| vnfs::Error::client(0, 22))?;
                let path = if relative.as_os_str().is_empty() {
                    typed.to_path_buf()
                } else {
                    typed.join(relative)
                };
                let entry = WalkEntry::from_vfs(
                    path,
                    event.depth,
                    Follow::Never,
                    event.entry.attrs().clone(),
                );
                Ok(match callback(entry) {
                    WalkControl::Continue if boundary => WalkControl::SkipSubtree,
                    decision => decision,
                })
            },
        )
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_operand_discovers_its_mount_but_symlink_stays_on_parent() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("mounted");
        std::fs::create_dir(&directory).unwrap();
        assert_eq!(
            mount_operand(&directory).unwrap(),
            (directory.canonicalize().unwrap(), PathBuf::from("/"))
        );
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&directory, &link).unwrap();
        assert_eq!(
            mount_operand(&link).unwrap(),
            (root.path().canonicalize().unwrap(), PathBuf::from("/link"))
        );
    }
    fn collect(config: &Config, prune: bool) -> Vec<(String, usize)> {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("sub/deep")).unwrap();
        std::fs::write(root.path().join("a.txt"), b"a").unwrap();
        std::fs::write(root.path().join("sub/b.txt"), b"b").unwrap();
        std::fs::write(root.path().join("sub/deep/c.txt"), b"c").unwrap();
        let client = Mounted::new(root.path()).unwrap();
        let mut out = Vec::new();
        visit_backend(
            &client,
            Path::new("typed"),
            Path::new("/"),
            config,
            Attributes::stat(),
            |entry| {
                let name = entry
                    .path()
                    .strip_prefix("typed")
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let control = if prune && name == "sub" {
                    WalkControl::SkipSubtree
                } else {
                    WalkControl::Continue
                };
                out.push((name, entry.depth()));
                control
            },
        )
        .unwrap();
        out
    }
    #[test]
    fn sorted_preorder_and_depth_are_preserved() {
        assert_eq!(
            collect(
                &Config {
                    sorted_output: true,
                    ..Config::default()
                },
                false
            )
            .iter()
            .map(|x| x.0.as_str())
            .collect::<Vec<_>>(),
            [
                "",
                "a.txt",
                "sub",
                "sub/b.txt",
                "sub/deep",
                "sub/deep/c.txt"
            ]
        );
        assert!(collect(
            &Config {
                max_depth: 1,
                ..Config::default()
            },
            false
        )
        .iter()
        .all(|x| x.1 <= 1));
        assert!(collect(
            &Config {
                min_depth: 2,
                ..Config::default()
            },
            false
        )
        .iter()
        .all(|x| x.1 >= 2));
    }
    #[test]
    fn maxdepth_prunes_even_with_mindepth_and_postorder() {
        assert_eq!(
            collect(
                &Config {
                    max_depth: 0,
                    ..Config::default()
                },
                false
            ),
            [(String::new(), 0)]
        );
        assert!(collect(
            &Config {
                min_depth: 2,
                max_depth: 1,
                ..Config::default()
            },
            false
        )
        .is_empty());
        let out = collect(
            &Config {
                depth_first: true,
                max_depth: 1,
                ..Config::default()
            },
            false,
        );
        assert!(out.iter().all(|entry| entry.1 <= 1));
        assert_eq!(out.last().unwrap().0, "");
    }

    #[test]
    fn postorder_and_prune_are_preserved() {
        let out = collect(
            &Config {
                depth_first: true,
                ..Config::default()
            },
            false,
        );
        assert_eq!(out.last().unwrap().0, "");
        let index = |name: &str| out.iter().position(|x| x.0 == name).unwrap();
        assert!(index("sub/deep/c.txt") < index("sub/deep") && index("sub/deep") < index("sub"));
        assert!(collect(&Config::default(), true)
            .iter()
            .all(|x| !x.0.starts_with("sub/")));
    }
    #[test]
    fn stop_is_immediate_and_symlink_root_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        let client = Mounted::new(root.path()).unwrap();
        let mut calls = 0;
        visit_backend(
            &client,
            Path::new("typed"),
            Path::new("/"),
            &Config::default(),
            Attributes::MODE,
            |_| {
                calls += 1;
                WalkControl::Stop
            },
        )
        .unwrap();
        assert_eq!(calls, 1);
        std::os::unix::fs::symlink("sub", root.path().join("link")).unwrap();
        visit_backend(
            &client,
            Path::new("typed/link"),
            Path::new("/link"),
            &Config::default(),
            Attributes::MODE,
            |entry| {
                assert!(entry.file_type().is_symlink());
                WalkControl::Continue
            },
        )
        .unwrap();
    }
}
