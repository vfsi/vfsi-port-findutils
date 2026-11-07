//! Incremental VFSI traversal. Pruning runs before reading a directory;
//! once matching has begun, failures never replay side effects via walkdir.
use super::matchers::WalkEntry;
use super::{Config, Follow};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use vnfs::directory::{Attributes, DepthLimit, WalkControl, WalkEventKind, WalkOptions};
use vnfs::files::{Vfsi, VfsiExt};
use vnfs::nfs::mount::NfsMountSession;
use vnfs::posix::Posix;

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
    on_error: impl FnMut(&Path, usize, &vnfs::Error),
) -> Option<vnfs::Result<()>> {
    let typed = Path::new(dir);
    let (base, vroot) = mount_operand(typed)?;
    match std::env::var("VNFS_IMPL").as_deref() {
        Ok("dummy") => Some(visit_backend(
            &Posix::new(&base).ok()?,
            typed,
            &vroot,
            config,
            fields,
            callback,
            on_error,
        )),
        Ok("nfs") => Some(visit_backend(
            NfsMountSession::from_mount(&base).ok()?.fs(),
            typed,
            &vroot,
            config,
            fields,
            callback,
            on_error,
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
    mut on_error: impl FnMut(&Path, usize, &vnfs::Error),
) -> vnfs::Result<()> {
    let limits = client.limits();
    let defaults = limits.walk_options();
    let options = if config.max_depth <= limits.max_walk_depth().get() {
        DepthLimit::new(config.max_depth).map_or(defaults, |depth| {
            defaults
                .with_max_depth(depth)
                .with_truncate_at_max_depth(true)
        })
    } else {
        defaults
    };
    let mut had_errors = false;
    client
        .listdir_with_errors(
            vroot,
            WalkOptions::new(options.with_fields(fields).with_recursive(true))
                .with_enter_leave(true)
                .with_sort_by_name(config.sorted_output),
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
                let path = render_path(typed, vroot, event.entry.path())?;
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
            |path, depth, error| {
                had_errors = true;
                let path = render_path(typed, vroot, path)?;
                on_error(&path, depth, &error);
                Ok(ControlFlow::Continue(()))
            },
        )
        .map(|completion| {
            if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
                match (completion, had_errors) {
                    (vnfs::directory::TraversalCompletion::Complete, false) => {
                        eprintln!("[profile] find_walk_completed");
                    }
                    (vnfs::directory::TraversalCompletion::Complete, true) => {
                        eprintln!("[profile] find_walk_completed_with_errors");
                    }
                    (vnfs::directory::TraversalCompletion::Stopped, _) => {
                        eprintln!("[profile] find_walk_stopped");
                    }
                }
            }
        })
}

fn render_path(typed: &Path, vroot: &Path, path: &Path) -> vnfs::Result<PathBuf> {
    let relative = path
        .strip_prefix(vroot)
        .map_err(|_| vnfs::Error::errno(0, 22))?;
    Ok(if relative.as_os_str().is_empty() {
        typed.to_path_buf()
    } else {
        typed.join(relative)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn unexpected_error(path: &Path, depth: usize, error: &vnfs::Error) {
        panic!(
            "unexpected traversal error at {} depth {depth}: {error}",
            path.display()
        );
    }
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
        let client = Posix::new(root.path()).unwrap();
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
            unexpected_error,
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
        assert_eq!(
            collect(
                &Config {
                    min_depth: 2,
                    max_depth: 1,
                    ..Config::default()
                },
                false
            ),
            Vec::<(String, usize)>::new()
        );
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
    fn client_depth_budget_is_preserved_with_explicit_unlimited_support() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("sub/deep")).unwrap();
        for (limit, requested, fails) in [
            (DepthLimit::new(0).unwrap(), usize::MAX, true),
            (DepthLimit::new(0).unwrap(), 0, false),
            (DepthLimit::unlimited(), usize::MAX, false),
        ] {
            let client = Posix::new(root.path())
                .unwrap()
                .with_limits(vnfs::files::ResourceLimits::new().with_max_walk_depth(limit));
            let mut depths = Vec::new();
            let result = visit_backend(
                &client,
                Path::new("typed"),
                Path::new("/"),
                &Config {
                    max_depth: requested,
                    ..Config::default()
                },
                Attributes::MODE,
                |entry| {
                    depths.push(entry.depth());
                    WalkControl::Continue
                },
                unexpected_error,
            );
            if fails {
                assert_eq!(result.unwrap_err().raw_code(), uucore::libc::EFBIG as u32);
            } else {
                result.unwrap();
                assert_eq!(
                    depths,
                    if requested == 0 {
                        vec![0]
                    } else {
                        vec![0, 1, 2]
                    }
                );
            }
        }
    }

    #[test]
    fn stop_is_immediate_and_symlink_root_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        let client = Posix::new(root.path()).unwrap();
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
            unexpected_error,
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
            unexpected_error,
        )
        .unwrap();
    }
    #[test]
    fn vanished_child_reports_typed_path_and_keeps_siblings_without_replay() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("a")).unwrap();
        std::fs::create_dir(root.path().join("b")).unwrap();
        std::fs::write(root.path().join("b/file"), b"data").unwrap();
        let client = Posix::new(root.path()).unwrap();
        let mut visited = Vec::new();
        let mut errors = Vec::new();
        visit_backend(
            &client,
            Path::new("./typed"),
            Path::new("/"),
            &Config {
                sorted_output: true,
                ..Config::default()
            },
            Attributes::MODE,
            |entry| {
                visited.push(entry.path().to_path_buf());
                if entry.path() == Path::new("./typed/a") {
                    std::fs::remove_dir(root.path().join("a")).unwrap();
                }
                WalkControl::Continue
            },
            |path, depth, error| {
                errors.push((path.to_path_buf(), depth, error.raw_code()));
            },
        )
        .unwrap();
        assert_eq!(
            errors,
            [(PathBuf::from("./typed/a"), 1, uucore::libc::ENOENT as u32)]
        );
        assert_eq!(
            visited,
            ["./typed", "./typed/a", "./typed/b", "./typed/b/file"].map(PathBuf::from)
        );
    }
}
