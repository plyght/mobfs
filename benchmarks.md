# Benchmarks

Date: 2026-05-01

Host: macOS 26.5 Beta (25F5058e), Darwin 25.5.0 arm64 on `MacBook-Pro-2.local`

MobFS build: `target/release/mobfs` from this checkout.

Fixture: `/Users/nicojaffer/wax`

- Apparent fixture size: `11G`
- Files within max depth 3: `113`
- Git state before testing: `README.md` modified
- Daemon: local TCP daemon bound to `127.0.0.1:7727`
- Mountpoint: `/tmp/mobfs-wax-proof`
- Mount command: `mobfs mount 127.0.0.1:/Users/nicojaffer/wax --local /tmp/mobfs-wax-proof --port 7727 --no-open --cache-ttl-secs 0`
- Token: local throwaway token

## Results

| Operation | Result | Wall time |
| --- | ---: | ---: |
| Release build | success | 4.47s |
| Read first 5 lines of `README.md` through mount | success | 0.00s |
| `rg -n "struct\|enum\|impl" /tmp/mobfs-wax-proof/src` | 147 lines | 0.19s |
| `git status --short` through mount | showed `M README.md` | 4.63s |
| `git diff -- README.md` through mount | 248 bytes | 0.07s |
| Agent/editor temp-write pattern: mkdir, write `.agent.tmp`, rename to `agent.txt`, read back | success | 3.64s |
| Write recovery after daemon restart | success | 2.31s |
| `mobfs run pwd` from mirror workspace | `/Users/nicojaffer/wax` | 0.60s |
| `mobfs git status --short` from mirror workspace | showed `M README.md` | 0.71s |

## Negative/performance findings

- A 32 MiB write through the FUSE mount did not complete within 120s. It reached roughly 7.6-8.0 MiB before the command timed out. Likely fix direction: batch or stream larger write chunks through a long-lived upload path instead of many small synchronous FUSE write RPCs.
- `find /tmp/mobfs-wax-proof -type f | wc -l` did not complete within 180s. Likely fix direction: add directory-entry/stat prefetching, stronger negative/positive metadata caching, and default ignores for heavy build/cache trees during broad traversal.
- `du -sh /tmp/mobfs-wax-proof` did not complete within 180s. Likely fix direction: avoid recursively hydrating cold metadata one path at a time; provide cached aggregate metadata or fast remote-side traversal for size/walk workloads.

These are important because broad tree walks, full-directory scans, and large writes are common in developer tooling. The current mount path is usable for targeted source reads, small edits, git status, and remote-command workflows, but it is not yet consistently fast for large writes or exhaustive filesystem traversal.

## Interpretation

The local proof supports the core idea: MobFS can mount a real Rust repo, read source on demand, support git queries, perform editor-style atomic writes, and reconnect after daemon restart. The faster path for git-like workflows is currently `mobfs git`/`mobfs run` from mirror/config mode, not raw FUSE traversal.

The main performance bottleneck appears to be metadata-heavy or whole-tree workloads over FUSE. Large sequential writes also need optimization before the project can honestly claim "fast as fuck" for general coding workloads.
