# Benchmarks

Date: 2026-05-01

Host: macOS 26.5 Beta (25F5058e), Darwin 25.5.0 arm64 on `MacBook-Pro-2.local`

MobFS build: `target/release/mobfs` from this checkout.

Fixture: `/Users/nicojaffer/wax`

- Apparent fixture size: `11G`
- Mounted traversal size after default ignores: `61M`
- Files traversed through mount after default ignores: `1057`
- Files within max depth 3: `113`
- Git state before testing: `README.md` modified
- Daemon: local TCP daemon bound to `127.0.0.1:7727`
- Mountpoint: `/tmp/mobfs-wax-proof`
- Mount command: `mobfs mount 127.0.0.1:/Users/nicojaffer/wax --local /tmp/mobfs-wax-proof --port 7727 --no-open --cache-ttl-secs 0`
- Token: local throwaway token

## Results after FUSE write/traversal optimization

| Operation | Result | Wall time |
| --- | ---: | ---: |
| Release build | success | 3.79s |
| Read first 5 lines of `README.md` through mount | success | 0.00s |
| `rg -n "struct\|enum\|impl" /tmp/mobfs-wax-proof/src` | 147 lines | 0.06s |
| `git status --short` through mount with `--cache-ttl-secs 1` | showed `M README.md` | warm 0.24-0.25s, cold 1.49s |
| `git diff -- README.md` through mount | 248 bytes | 0.08s |
| Agent/editor namespace pattern: mkdir, write `.agent.tmp`, rename to `agent.txt`, symlink, read back, cleanup | success | 0.25s |
| Write recovery after daemon restart | success | 0.58s |
| 32 MiB zero-filled write through FUSE mount | success | 1.37s |
| `find /tmp/mobfs-wax-proof -type f \| wc -l` with `--cache-ttl-secs 1` | 1057 files | 0.21-0.25s |
| `du -sh /tmp/mobfs-wax-proof` | 61M | 0.11s |
| `mobfs run pwd` from mirror workspace | `/Users/nicojaffer/wax` | 0.61s |
| `mobfs git status --short` from mirror workspace | showed `M README.md` | 0.72s |
| `mobfs run pwd` directly from no-local-code mount root | `/Users/nicojaffer/wax` | 0.31s |
| `mobfs git status --short` directly from no-local-code mount root | showed `M README.md` | 0.13s |

## Native baseline on same fixture

These commands ran directly against `/Users/nicojaffer/wax` on the same machine. The `find` and `du` comparisons pruned/ignored `target` to match the mount's default heavy-directory ignore behavior.

| Operation | Result | Wall time |
| --- | ---: | ---: |
| `rg -n "struct\|enum\|impl" /Users/nicojaffer/wax/src` | 147 lines | 0.02s |
| `git status --short` | showed `M README.md` | 0.04s |
| `git diff -- README.md` | 248 bytes | 0.01s |
| `find /Users/nicojaffer/wax -path /Users/nicojaffer/wax/target -prune -o -type f -print \| wc -l` | completed | 0.04s |
| `du -sh -I target /Users/nicojaffer/wax` | completed | 0.00s |

## Before/after summary

| Workload | Before | After |
| --- | ---: | ---: |
| 32 MiB write through FUSE | timed out after 120s at roughly 8 MiB | completed in 1.37s |
| Full `find` through FUSE | timed out after 180s | completed in 0.21-0.25s |
| `du -sh` through FUSE | timed out after 180s | completed in 0.11s |
| Agent/editor temp-write pattern | 3.64s | 0.25s including symlink and cleanup |
| Daemon restart write recovery | 2.31s | 0.58s |
| `git status --short` through FUSE | 4.63s | warm 0.24-0.25s with 1s TTL |

## Interpretation

The optimization fixed the obvious showstoppers from the first proof run. Large sequential writes no longer refresh the entire remote snapshot after every FUSE write chunk, broad tree traversal now avoids configured heavy directories such as `target` and `node_modules` through the mount path, nonzero cache TTL mode now serves known lookup/getattr metadata from the local snapshot instead of round-tripping for every stat, readdir calls are seeded from the initial snapshot and cached for the TTL window, daemon stat/list-dir metadata now avoids hashing file contents, and namespace mutations update local metadata instead of refreshing the full snapshot.

MobFS is now fast enough for controlled personal dogfooding of the FUSE path: targeted source reads, source search, editor-style atomic writes, medium sequential writes, daemon restart recovery, and ignored-directory tree walks all complete quickly on a real Rust repo.

It is closer, but still not near-native for metadata-heavy workloads. Native local operations are still materially faster, especially `git status`, `find`, and `du`. The right performance claim after this run is: fast enough for common remote coding workflows and improving toward native-like behavior, but not native-like for arbitrary filesystem scans yet.

Remaining performance work:

- make cold `git status` through raw FUSE closer to native or steer users toward `mobfs git`
- add stronger directory-entry/stat prefetch for cold metadata-heavy workloads
- test over a real remote network link, not just local TCP
