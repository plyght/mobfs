# Testing Notes

Date: 2026-05-01

This document records manual proof testing. The first pass intentionally avoided the Rust test suite; a later accidental `cargo test` invocation was stopped and the manual proof flow below was used instead.

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

The latest performance pass also tested `--cache-ttl-secs 1`, which lets MobFS serve known lookup/getattr metadata from its local snapshot and cache repeated directory listings inside the TTL window instead of round-tripping to the daemon for every stat-like operation. Daemon `stat` and `list_dir` now use metadata-only responses instead of hashing file contents.

Additional manual probes:

```sh
head -n 5 /tmp/mobfs-wax-proof/README.md
rg -n 'struct|enum|impl' /tmp/mobfs-wax-proof/src
git -C /tmp/mobfs-wax-proof status --short
git -C /tmp/mobfs-wax-proof diff -- README.md

dd if=/dev/zero of=/tmp/mobfs-wax-proof/mobfs-large-write.bin bs=1m count=32 status=none
find /tmp/mobfs-wax-proof -type f | wc -l
du -sh /tmp/mobfs-wax-proof
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

Configured heavy ignored directories such as `target` are now filtered from mount directory listings.

### No-local-code mount semantics

No `.mobfs`, `.mobfs.toml`, or similar MobFS workspace config was created at the mount root during mount-mode testing.

### Basic reads

Reading `README.md` through the mount worked immediately in `0.00s`.

### Source search

`rg` against `src` worked through the mount and returned 147 matches in `0.06s` with a 1s cache TTL and directory-entry caching.

### Git through FUSE

`git status --short` worked through the mount in `0.24-0.25s` warm with a 1s cache TTL and directory-entry caching, after a `1.49s` cold run, and reported the existing modified `README.md`.

`git diff -- README.md` worked in `0.08s` and returned the expected 248-byte diff output.

### Agent/editor write pattern

This passed through the FUSE mount. The latest measured flow, including symlink creation and cleanup, took `0.25s`:

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

After intentionally killing the daemon and starting it again on the same port, the existing mount recovered on the next write. Writing `mobfs-recovery-proof.txt` through the mount succeeded in `0.58s` and appeared in `/Users/nicojaffer/wax`.

### Large writes over FUSE

A 32 MiB zero-filled write through the mount completed in `1.37s`. The file appeared as 32 MiB from both the mounted path and the remote source path.

This fixes the prior failure where a 32 MiB write timed out after 120s at roughly 8 MiB.

### Full tree traversal

A full `find` over the mounted repo completed in `0.21-0.25s` with a 1s cache TTL, initial snapshot seeding, and directory-entry caching and counted 1057 files.

`du -sh` over the mounted repo completed in `0.11s` and reported 61M.

This fixes the prior failure where both commands timed out after 180s. The improvement depends on mount directory listings respecting configured heavy-directory ignores such as `target` and `node_modules`.

### Remote command workflows

From an explicit mirror workspace, these worked:

```sh
mobfs run pwd
mobfs git status --short
```

`mobfs run pwd` executed in `/Users/nicojaffer/wax` in `0.61s`.

`mobfs git status --short` reported the existing modified `README.md` in `0.72s`.

## What is still not ready

### Near-native filesystem performance

MobFS is much faster after the latest changes, but it is not near-native for metadata-heavy workloads. Native local baselines on the same fixture were roughly:

- `rg` over `src`: `0.02s` native vs `0.06s` through FUSE
- `git status --short`: `0.04s` native vs `0.24-0.25s` warm through FUSE
- `find` with `target` pruned: `0.04s` native vs `0.21-0.25s` through FUSE
- `du` with `target` ignored: effectively instant native vs `0.11s` through FUSE

The current state is good enough for dogfooding and targeted coding workflows, but not yet good enough to claim native-like general filesystem performance.

### `mobfs run` / `mobfs git` directly from mount root

`mobfs run` and `mobfs git` now work directly from the no-local-code mount root through a non-source mount registry in the user cache directory.

Measured results:

```sh
cd /tmp/mobfs-wax-proof
mobfs run pwd
mobfs git status --short
```

`mobfs run pwd` executed in `/Users/nicojaffer/wax` in `0.31s`.

`mobfs git status --short` reported the existing modified `README.md` in `0.13s`.

The mount root still did not contain `.mobfs`, `.mobfs.toml`, or similar project config files.

## Cleanup performed

- Removed proof files/directories from `/Users/nicojaffer/wax`.
- Removed `/tmp/mobfs-mirror-proof`.
- Force-unmounted `/tmp/mobfs-wax-proof`.
- Stopped the local daemon.

Final observed Wax git status still only showed the pre-existing modified `README.md`.

## Readiness conclusion

MobFS is ready for controlled personal dogfooding of the no-local-code mount path.

The previous blockers are fixed:

- large write throughput is now acceptable on the local proof fixture
- full-tree traversal completes when configured heavy directories are ignored
- metadata/stat-heavy traversal no longer hangs on this fixture
- editor/agent namespace mutation patterns no longer refresh the full snapshot after each operation

The remaining focus should be:

- bring `git status` and other metadata-heavy FUSE workloads closer to native
- improve cold raw-FUSE `git status`
- test over real remote network conditions
- define honest performance guidance: fast for remote coding workflows, not yet native-like for arbitrary filesystem scans
