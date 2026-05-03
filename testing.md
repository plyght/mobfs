# Testing Notes

Date: 2026-05-01

This document records manual proof testing. The first pass intentionally avoided the Rust test suite; a later accidental `cargo test` invocation was stopped and the manual proof flow below was used instead.

## Environment

- OS: macOS 26.5 Beta (25F5058e), Darwin 25.5.0 arm64
- macFUSE: installed
- Repo under test: `/Users/plyght/wax`
- MobFS binary: `target/release/mobfs`
- Daemon root allowlist: `/Users/plyght`
- Daemon bind: `127.0.0.1:7727`
- Mountpoint: `/tmp/mobfs-wax-proof`

## Commands used

```sh
cargo build --release

MOBFS_TOKEN=local-proof-token target/release/mobfs daemon \
  --bind 127.0.0.1:7727 \
  --allow-root /Users/plyght \
  --token local-proof-token

MOBFS_TOKEN=local-proof-token target/release/mobfs mount \
  127.0.0.1:/Users/plyght/wax \
  --local /tmp/mobfs-wax-proof \
  --port 7727 \
  --no-open \
  --cache-ttl-secs 0
```

The latest performance passes tested both `--cache-ttl-secs 1` and `--cache-ttl-secs 0`. MobFS now serves known lookup/getattr metadata from the mounted snapshot even in TTL 0 mode, so common metadata-heavy tools no longer remote-stat every known path. Directory listings are seeded from the initial snapshot even in TTL 0 mode and refreshed after remote listings. Daemon `stat` and `list_dir` use metadata-only responses instead of hashing file contents. Small files are cached as whole-file reads, which helps Git/editor workloads that repeatedly read many small files. The default ignore list now blocks macOS AppleDouble `._*` sidecar files as well as `.DS_Store`.

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

The mount came up and listed `/Users/plyght/wax` contents through `/tmp/mobfs-wax-proof`.

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

`git status --short` worked through the mount in `0.24-0.25s` warm with a 1s cache TTL and directory-entry caching, after a `1.49s` cold run, and reported the existing modified `README.md`. After the snapshot metadata fast path, repeated raw FUSE `git status --short` runs with `--cache-ttl-secs 0` measured `0.14s`, `0.13s`, and `0.12s`. After the TTL 0 directory reuse and small-file read-cache pass, raw FUSE `git status --short` measured `0.11s` cold and `0.09s` warm on the same fixture.

`git diff -- README.md` worked in `0.02-0.08s` and returned the expected 248-byte diff output.

### Agent/editor write pattern

This passed through the FUSE mount. The latest measured flow, including symlink creation and cleanup, was stable around `0.25-0.27s`:

1. Create a proof directory.
2. Write `.agent.tmp`.
3. Rename `.agent.tmp` to `agent.txt`.
4. Read back `agent.txt`.
5. Verify the remote-side file in `/Users/plyght/wax` contained the same content.

### Metadata and namespace operations

The following worked through the mount and were reflected in the remote tree:

- `chmod +x`
- symlink creation
- file deletion
- directory cleanup

### Daemon restart recovery

After intentionally killing the daemon and starting it again on the same port, the existing mount recovered on the next write. Writing `mobfs-recovery-proof.txt` through the mount succeeded in `0.58s` in the earlier pass and `0.06s` in the latest pass, and appeared in `/Users/plyght/wax`.

### Large writes over FUSE

A 32 MiB zero-filled write through the mount completed in `1.37s` in the first optimized pass, `0.84s` after the later metadata/journal work, and `0.92s` in the latest verification pass. The file appeared as 32 MiB from both the mounted path and the remote source path.

This fixes the prior failure where a 32 MiB write timed out after 120s at roughly 8 MiB.

### Full tree traversal

A full `find` over the mounted repo completed in `0.21-0.25s` with a 1s cache TTL, initial snapshot seeding, and directory-entry caching and counted 1057 files. A later clean TTL 0 run after the metadata fast path completed in `0.30s` and counted 1057 files. After TTL 0 directory reuse, a latest TTL 0 verification pass counted 1057 files in `0.36s`.

`du -sh` over the mounted repo completed in `0.11s` with TTL 1 and reported 61M. A later clean TTL 0 run completed in `0.23s` and reported 61M. The latest TTL 0 verification pass reported 61M in `0.15s`.

This fixes the prior failure where both commands timed out after 180s. The improvement depends on mount directory listings respecting configured heavy-directory ignores such as `target` and `node_modules`.

### Git checkout and remote command workflows

A temporary branch checkout cycle through the raw FUSE mount passed on `~/wax`:

```sh
git checkout -b mobfs-proof-branch
git checkout master
git branch -D mobfs-proof-branch
```

Observed wall time was `1.11s`.

A 20-file agent-style bulk write exposed macOS AppleDouble `._*` sidecar creation on the first pass. MobFS now treats ignore entries ending in `*` as path-segment prefixes and includes `._*` in the default ignore list. A follow-up mount validation confirmed that writing `/tmp/mobfs-polish-mount/._mobfs-polish-sidecar` is blocked with `Permission denied`, does not create a remote sidecar file, and normal file create/delete still works through the mount.

A mid-write daemon kill was tested with a 128 MiB `dd` write through the mount. With mount-mode retries disabled and 1s socket timeouts, the writer failed in about `8s` instead of hanging for over a minute. The partial file stayed remote-side, remount replay completed, and a normal post-remount create/delete succeeded. Existing mounted handles can still return `EIO` after the hard daemon kill; the reliable recovery path is unmount/remount, which is acceptable for this local chaos pass but still below true mosh-style seamlessness.

### Remote command workflows

From an explicit mirror workspace, these worked:

```sh
mobfs run pwd
mobfs git status --short
```

`mobfs run pwd` executed in `/Users/plyght/wax` in `0.61s`.

`mobfs git status --short` reported the existing modified `README.md` in `0.72s`.

## Latest Raspberry Pi remote proof after recovery fix

A follow-up proof used `plyght@100.74.238.62:/home/plyght/wax` to validate user-aware SSH targets and same-mount daemon restart recovery.

What passed:

- `mobfs mount plyght@100.74.238.62:/home/plyght/wax --ssh-tunnel` mounted successfully without a manual tunnel
- killing and restarting the remote daemon no longer required unmount/remount for a normal buffered write
- post-restart write through the existing mount completed in `0.80s` and read back `recovery-ok`

This verifies the immediate blocker found in the first Raspberry Pi run: same-mount recovery after daemon restart for normal writes. Broader flaky-network, sleep/wake, and mid-stream large-write chaos testing is still needed before claiming full mosh-style behavior.

## What is still not ready

### Near-native filesystem performance

MobFS is much faster after the latest changes, but it is not fully native-like for all metadata-heavy workloads. Native local baselines on the same fixture were roughly:

- `rg` over `src`: `0.01-0.02s` native vs `0.04-0.06s` through FUSE
- `git status --short`: `0.02-0.04s` native vs `0.11s` cold and `0.09s` warm through raw FUSE after the small-file cache pass
- `find` with `target` pruned: `0.01-0.04s` native vs `0.30-0.36s` through raw FUSE TTL 0, or `0.21-0.25s` with TTL 1
- `du` with `target` ignored: effectively instant native vs `0.15-0.23s` through raw FUSE TTL 0, or `0.11s` with TTL 1

The current state is good enough for dogfooding and targeted coding workflows, but not yet good enough to claim native-like general filesystem performance.

### `mobfs run` / `mobfs git` directly from mount root

`mobfs run` and `mobfs git` now work directly from the no-local-code mount root through a non-source mount registry in the user cache directory.

Measured results:

```sh
cd /tmp/mobfs-wax-proof
mobfs run pwd
mobfs git status --short
```

`mobfs run pwd` executed in `/Users/plyght/wax` in `0.02-0.31s`.

`mobfs git status --short` reported the existing modified `README.md` in `0.12-0.13s`.

The mount root still did not contain `.mobfs`, `.mobfs.toml`, or similar project config files.

## Cleanup performed

- Removed proof files/directories from `/Users/plyght/wax`.
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

- keep narrowing `git status` and other metadata-heavy FUSE workloads against native on larger repositories
- improve arbitrary filesystem scans such as `find` and `du`
- test over real remote network conditions
- improve same-mount recovery after a hard mid-write daemon kill so users do not need to unmount/remount
- define honest performance guidance: fast for remote coding workflows, not yet native-like for arbitrary filesystem scans
