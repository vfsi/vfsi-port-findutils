# findutils

[![Crates.io](https://img.shields.io/crates/v/findutils.svg)](https://crates.io/crates/findutils)
[![Discord](https://img.shields.io/badge/discord-join-7289DA.svg?logo=discord&longCache=true&style=flat)](https://discord.gg/wQVJbvJ)
[![License](http://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/uutils/findutils/blob/main/LICENSE)
[![dependency status](https://deps.rs/repo/github/uutils/findutils/status.svg)](https://deps.rs/repo/github/uutils/findutils)
[![codecov](https://codecov.io/gh/uutils/findutils/branch/master/graph/badge.svg)](https://codecov.io/gh/uutils/findutils)

Rust implementation of [GNU findutils](https://www.gnu.org/software/findutils/): `xargs`, `find`, `locate` and `updatedb`.
The goal is to be a full drop-in replacement of the original commands.

## Run the GNU testsuite on rust/findutils:

```
bash util/build-gnu.sh

# To run a specific test:
bash util/build-gnu.sh tests/misc/help-version.sh
```

## Comparing with GNU

![Evolution over time - GNU testsuite](https://github.com/uutils/findutils-tracking/blob/main/gnu-results.svg?raw=true)
![Evolution over time - BFS testsuite](https://github.com/uutils/findutils-tracking/blob/main/bfs-results.svg?raw=true)

## Build/run with BFS

[bfs](https://github.com/tavianator/bfs) is a variant of the UNIX find command that operates breadth-first rather than depth-first.

```
bash util/build-bfs.sh

# To run a specific test:
bash util/build-bfs.sh posix/basic
```

For more details, see https://github.com/uutils/findutils-tracking/

## VFSI Port

This repository is the VFSI application port of
[`uutils/findutils`](https://github.com/uutils/findutils), maintained under
`vfsi/vfsi-port-findutils` on the `vfsi` integration branch. It adds an opt-in
vectorized traversal for `find` that talks to an NFSv4 server directly through
the [`vnfs`](https://crates.io/crates/vnfs) crate instead of issuing one kernel
`lstat` per entry. File attributes are returned in the `READDIR` replies, so a
metadata-heavy walk makes far fewer round trips.

At runtime, the `VNFS_IMPL` environment variable selects the backend:

- unset — the standard `std::fs`/`walkdir` traversal (default);
- `dummy` — the local filesystem through the VFSI API;
- `nfs` — the NFS server hosting the search root.

Only the default `-P` mode (do not follow symlinks) and searches that do not
cross filesystems (`-x`) use the VFSI path; every other mode, and any backend
failure, falls back to the standard traversal, so output is unchanged.

Build and run with the feature enabled:

```
cargo build --release --features vnfs
VNFS_IMPL=nfs target/release/find /path/on/nfs -type f
```

Maturity: experimental, tested against the pinned `vnfs` 0.0.13 ABI.
