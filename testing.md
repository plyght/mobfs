# Testing Notes

Date: 2026-05-01

This document records manual proof testing, not the Rust test suite. The user asked not to run the suite because it wastes time.

## Environment

- OS: macOS 26.5 Beta (25F5058e), Darwin 25.5.0 arm64
- macFUSE: installed
- Repo under test: `/Users/nicojaffer/wax`
- MobFS binary: `target/release/mobfs`
- Daemon root allowlist: `/Users/nicojaffer`
- Daemon bind: `127.0.0.1:7727`
- Mountpoint: `/tmp/mobfs-wax-proof`

## Commands used

```sh
cargo build --release

MOBFS_TOKEN=local-proof-token target/release/mobfs daemon \
  --bind 127.0.0.1:7727 \
  --allow-root /Users/nicojaffer \
  --token local-proof-token

MOBFS_TOKEN=local-proof-token target/release/mobfs mount \
  127.0.0.1:/Users/nicojaffer/wax \
  --local /tmp/mobfs-wax-proof \
  --port 7727 \
  --no-open \
  --cache-ttl-secs 0
```

## What passed

### Mount startup

The mount came up and listed `/Users/nicojaffer/wax` contents through `/tmp/mobfs-wax-proof`.

Observed top-level entries included:

- `.git`
- `Cargo.toml`
- `Cargo.lock`
- `README.md`
- `src`
- `tests`
- `target`

### No-local-code mount semantics

No `.mobfs`, `.mobfs.toml`, or similar MobFS workspace config was created at the mount root during mount-mode testing.

### Basic reads

Reading `README.md` through the mount worked immediately.

### Source search

`rg` against `src` worked through the mount and returned 147 matches in 0.19s.

### Git through FUSE

`git status --short` worked through the mount and reported the existing modified `README.md`.

`git diff -- README.md` worked and returned the expected diff output.

### Agent/editor write pattern

This passed through the FUSE mount:

1. Create a proof directory.
2. Write `.agent.tmp`.
3. Rename `.agent.tmp` to `agent.txt`.
4. Read back `agent.txt`.
5. Verify the remote-side file in `/Users/nicojaffer/wax` contained the same content.

### Metadata and namespace operations

The following worked through the mount and were reflected in the remote tree:

- `chmod +x`
- symlink creation
- file deletion
- directory cleanup

### Daemon restart recovery

After intentionally killing the daemon and starting it again on the same port, the existing mount recovered on the next write. Writing `mobfs-recovery-proof.txt` through the mount succeeded and appeared in `/Users/nicojaffer/wax`.

### Remote command workflows

From an explicit mirror workspace, these worked:

```sh
mobfs run pwd
mobfs git status --short
```

`mobfs run pwd` executed in `/Users/nicojaffer/wax`.

`mobfs git status --short` reported the existing modified `README.md`.

## What failed or is not ready

### Large writes over FUSE

A 32 MiB zero-filled write through the mount timed out after 120s. The remote file reached roughly 7.6 MiB and the mounted view showed roughly 8.0 MiB at timeout.

This is not acceptable for the "fast as fuck" target. Likely fix direction: batch or stream larger write chunks through a long-lived upload path instead of many small synchronous FUSE write RPCs.

### Full tree traversal

A full `find` over the mounted repo did not complete within 180s.

`du -sh` over the mounted repo also did not complete within 180s.

This suggests metadata-heavy scans are currently too slow or can hang on large ignored/build directories like `target`. Likely fix direction: add directory-entry/stat prefetching, stronger metadata caching, default heavy-directory ignores, or remote-side fast paths for recursive walk/size workloads.

### `mobfs run` / `mobfs git` directly from mount root

Running `mobfs run` or `mobfs git` directly from the no-local-code mount root failed because those commands currently expect `.mobfs.toml` mirror/workspace config.

This is a product mismatch: if mount mode is primary, users will reasonably expect remote command helpers to understand the active mount. Likely fix direction: persist a small non-source mount registry keyed by mountpoint so `mobfs run` and `mobfs git` can recover the remote target without creating project files in the mount.

## Cleanup performed

- Removed proof files/directories from `/Users/nicojaffer/wax`.
- Force-unmounted `/tmp/mobfs-wax-proof`.
- Stopped the local daemon.

Final observed Wax git status still only showed the pre-existing modified `README.md`.

## Readiness conclusion

MobFS is ready for controlled personal dogfooding. It is not ready for general users as a polished "mosh for filesystems" yet.

The core semantics are promising, but the FUSE path needs focused work on:

- large write throughput
- full-tree traversal performance
- metadata/stat-heavy workloads
- direct `mobfs run`/`mobfs git` support from mount mode
- clearer performance guidance around ignored directories like `target`
