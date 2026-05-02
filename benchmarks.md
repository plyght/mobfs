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
| Temporary branch checkout cycle through raw FUSE | success | 1.11s |
| `mobfs run cargo check` directly from no-local-code mount root | success | 8.51s including crate downloads |
| AppleDouble `._*` sidecar write through mount | blocked, no remote sidecar created | immediate |
| 128 MiB write with daemon killed mid-write | failed quickly instead of hanging; remount recovery passed | writer exited in about 8s |

## Results after snapshot metadata fast path and journal sync optimization

| Operation | Result | Wall time |
| --- | ---: | ---: |
| Release build after journal optimization | success | 3.82s |
| Raw FUSE `git status --short` with `--cache-ttl-secs 0` | showed `M README.md` | 0.12-0.14s warm; one earlier cold sample 0.19s |
| Agent/editor namespace pattern, 5 repeated samples | success | stable 0.25-0.26s |
| Clean `find /tmp/mobfs-wax-proof -type f \| wc -l` with `--cache-ttl-secs 0` | 1057 files | 0.30s |
| Clean `du -sh /tmp/mobfs-wax-proof` with `--cache-ttl-secs 0` | 61M | 0.23s |
| 32 MiB zero-filled write through FUSE mount | success | 0.84s |

## Remote-network proof over private mesh VPN

An anonymized Raspberry Pi 5 class ARM Linux host was tested over a private mesh VPN from macOS on a low-latency Wi-Fi link. The daemon listened only on localhost on the remote host and the client connected through `mobfs mount --ssh-tunnel`. The test workspace was a small Git repository created only for this proof.

| Operation | Result | Wall time |
| --- | ---: | ---: |
| ICMP round trip before test | reachable | roughly 20-36ms |
| Remote release build on ARM Linux | success | 2m56s |
| No-local-code mount over SSH tunnel | success | mounted locally |
| Read small README through mount | success | 0.01s |
| `rg` through mount on tiny repo | 2 matches | 0.83s |
| Raw FUSE `git status --short` | success | 3.88s |
| Agent temp-write plus rename | success | 0.83s |
| 32 MiB write through FUSE | success | 156.49s |
| `find` over tiny repo through mount | 32 files | 1.19s |
| `mobfs run pwd` from mount | remote workspace path | 0.37s |
| `mobfs git status --short` from mount | success | 0.36s |

Interpretation: remote command offload is already responsive over the real remote link, but the raw FUSE write path was not acceptable for large writes over network latency. The local proof fixture had 32 MiB writes under one second, while the remote SSH-tunneled path took over two minutes. The likely bottleneck was per-write-chunk synchronous local journaling, short socket timeouts, and request/response round trips rather than raw link bandwidth alone. This result should be treated as the first remote-network baseline, not a product claim.

## Remote-network proof after write buffering and timeout tuning

The first optimization pass removed per-chunk durable mount journaling for successful write calls, buffered contiguous FUSE writes per open handle up to 4 MiB, flushed buffered data on `flush`/`fsync`, and increased daemon socket read/write timeouts from 1s to 15s. A 1s timeout preserved fast failure during earlier local chaos tests, but was too low for multi-megabyte encrypted frames over a real SSH-tunneled remote link.

| Operation | Result | Wall time |
| --- | ---: | ---: |
| 32 MiB write through FUSE before timeout tuning | failed at 8 MiB | 4.06s |
| 32 MiB write through FUSE after timeout tuning | success | 12.15s |
| `rg` through mount on tiny repo | 2 matches | 0.93s |
| Raw FUSE `git status --short` | success | 3.79s |
| Agent temp-write plus rename | success | 0.84s |
| `find` over tiny repo through mount | 34 files | 1.26s |
| `mobfs git status --short` from mount | success | 0.29s |

This is a roughly 12.9x improvement for the 32 MiB remote write path versus the initial 156.49s baseline, but it is still much slower than the local-TCP fixture. Remote command offload remains the recommended path for Git and build/test workloads over real links. The next write-path target is reducing request/response serialization and encryption overhead for large writes without weakening recovery semantics too much.

A follow-up test raised the FUSE write buffer from 4 MiB to 16 MiB. The same 32 MiB write slowed to 16.84s, so 4 MiB remains the better measured buffer size on this link. Larger encrypted JSON frames appear to add enough serialization/encryption or tunnel overhead to offset fewer round trips.

## Remote-network proof after binary write payloads

The next pass added a `WriteFileAtBinary` request. Metadata still uses the normal encrypted JSON protocol frame, but the file bytes are sent as a separate encrypted binary frame instead of being serialized as a JSON `Vec<u8>`.

| Operation | Result | Wall time |
| --- | ---: | ---: |
| 32 MiB write through FUSE with JSON payloads | success | 12.15s |
| 32 MiB write through FUSE with binary payloads | success | 8.81s |
| `rg` through mount on tiny repo | 2 matches | 0.91s |
| Raw FUSE `git status --short` | success | 3.85s |
| `mobfs git status --short` from mount | success | 0.32s |

This is a roughly 1.38x improvement over the buffered JSON write path and roughly 17.8x faster than the initial 156.49s remote baseline. The remaining overhead is likely encrypted-frame copying, request/response waiting per buffered write, FUSE/macOS writeback behavior, SSH tunnel overhead, and remote filesystem sync/write costs. The next likely improvement is a true streaming bulk-write mode that opens a remote write session and sends multiple binary chunks before waiting for final acknowledgement.

## Remote-network proof after streaming binary writes

The next pass added `WriteFileAtStream`. The client sends one encrypted JSON control frame with path/offset/length, then streams encrypted binary chunks without waiting for per-chunk acknowledgements. The daemon writes chunks as they arrive and sends one final acknowledgement after the full payload is written. The FUSE write buffer was raised to 32 MiB for this streaming path so a 32 MiB sequential write can cross the link as one write session.

| Operation | Result | Wall time |
| --- | ---: | ---: |
| 32 MiB write through FUSE with binary request/response payloads | success | 8.81s |
| 32 MiB write through FUSE with streaming binary chunks | success | 4.61s |
| Small write plus read-back through mount | success | 0.26s |
| `rg` through mount on tiny repo | 2 matches | 0.90s |
| Raw FUSE `git status --short` | success | 3.96s |
| `mobfs git status --short` from mount | success | 0.41s |

This is roughly 1.91x faster than the binary request/response write path, 2.64x faster than the buffered JSON path, and 33.9x faster than the initial 156.49s remote baseline. Large sequential writes over the real SSH-tunneled remote link are now in the usable range, though still slower than local TCP. Raw metadata-heavy Git over FUSE remains slow; remote-native `mobfs git` is still the right path for Git.

## Remote-network proof after same-mount recovery fix

A later Raspberry Pi remote proof used `nico@100.74.238.62:/home/nico/wax` over `--ssh-tunnel` after adding `user@host` parsing and restoring mount operation retries. The daemon was killed and restarted while the mount stayed active. A normal buffered write through the existing mount completed in `0.80s` and read back successfully.

Interpretation: same-mount daemon restart recovery now works for normal buffered writes on the real remote link. This is closer to the mosh-style goal, but it is not yet a complete spotty-network claim; sleep/wake, IP changes, long partitions, and hard mid-stream large-write failures still need dedicated chaos tests.

## Remote-network transport comparison after streaming writes

A direct daemon bind on the private mesh VPN address was tested to isolate SSH tunnel overhead. This exposes `mobfsd` on the private VPN interface rather than listening only on localhost behind `ssh -L`, so it is a performance experiment rather than the default security posture.

| Operation | Transport | Result | Wall time |
| --- | --- | ---: | ---: |
| 32 MiB streaming FUSE write | SSH tunnel | success | 4.61s |
| 32 MiB streaming FUSE write | direct private VPN TCP | success | 6.74s |
| 32 MiB streaming FUSE write with 64 MiB FUSE buffer | direct private VPN TCP | success | 8.58s |
| 32 MiB streaming FUSE write with TCP_NODELAY | direct private VPN TCP | success | 7.30s |
| 32 MiB streaming FUSE write with frame flush removed | direct private VPN TCP | success | 7.24s |

The SSH tunnel was faster than direct private VPN TCP in this run. That means the current bottleneck is not simply SSH tunnel overhead; link route selection, VPN TCP behavior, userspace copies, encryption framing, and FUSE/macOS behavior all matter. The 32 MiB FUSE buffer remains better than 64 MiB, and TCP_NODELAY/frame-flush tuning did not improve this bulk-write case.

WebSockets are not a good fit for the hot filesystem path. They are useful for browser clients and HTTP infrastructure, but MobFS is a native daemon moving encrypted binary filesystem frames. Raw TCP avoids WebSocket HTTP framing/masking/upgrade overhead and keeps the protocol simpler. WebSockets may be useful later for a browser dashboard or hosted control plane, not for the mount data plane.

## Remote-network proof after small-file prefetch

The next pass added `ReadSmallFiles`. During mount startup, the client asks the daemon for small files from the initial snapshot in one request and seeds the whole-file cache. The current limit is files up to 64 KiB with a total prefetch cap of 16 MiB. This targets Git, search, editor, and agent workloads that repeatedly read many small files.

| Operation | Before small-file prefetch | After small-file prefetch |
| --- | ---: | ---: |
| Raw FUSE `git status --short` over SSH tunnel | 3.85-3.96s | 0.94s cold, 0.52s warm |
| `rg` through mount on tiny repo | 0.90-0.91s | 0.12s |
| `mobfs git status --short` from mount | 0.32-0.41s | 0.17s |
| 32 MiB streaming FUSE write over SSH tunnel | 4.61s | 5.38s |

The metadata-heavy path improved substantially because Git and search avoid many small remote reads after mount. The 32 MiB write remained in the same rough range; the 5.38s sample is slower than the best 4.61s sample but still close enough to treat as normal remote-link variance unless repeated tests prove a regression.

## Results after TTL 0 directory reuse and small-file read cache

| Operation | Result | Wall time |
| --- | ---: | ---: |
| Release build after small-file cache optimization | success | 3.47s |
| Raw FUSE `git status --short` with `--cache-ttl-secs 0` | showed `M README.md` | cold 0.11s; warm 0.09s |
| `find /tmp/mobfs-wax-proof -type f \| wc -l` with `--cache-ttl-secs 0` | 1057 files | 0.36s |
| `du -sh /tmp/mobfs-wax-proof` with `--cache-ttl-secs 0` | 61M | 0.15s |
| 32 MiB zero-filled write through FUSE mount | success | 0.92s |

## Native baseline on same fixture

These commands ran directly against `/Users/nicojaffer/wax` on the same machine. The `find` and `du` comparisons pruned/ignored `target` to match the mount's default heavy-directory ignore behavior.

| Operation | Result | Wall time |
| --- | ---: | ---: |
| `rg -n "struct\|enum\|impl" /Users/nicojaffer/wax/src` | 147 lines | 0.01-0.02s |
| `git status --short` | showed `M README.md` | 0.02-0.04s |
| `git diff -- README.md` | 248 bytes | 0.01s |
| `find /Users/nicojaffer/wax -path /Users/nicojaffer/wax/target -prune -o -type f -print \| wc -l` | completed | 0.01-0.04s |
| `du -sh -I target /Users/nicojaffer/wax` | completed | 0.00s |

## Before/after summary

| Workload | Before | After |
| --- | ---: | ---: |
| 32 MiB write through FUSE | timed out after 120s at roughly 8 MiB | completed in 0.84-0.92s |
| Full `find` through FUSE | timed out after 180s | completed in 0.21-0.36s |
| `du -sh` through FUSE | timed out after 180s | completed in 0.11-0.15s |
| Agent/editor temp-write pattern | 3.64s | 0.25-0.27s including symlink and cleanup |
| Daemon restart write recovery | 2.31s | 0.06s latest, 0.58s earlier |
| `git status --short` through FUSE | 4.63s | cold 0.11s, warm 0.09s with TTL 0 small-file cache |

## Interpretation

The optimization fixed the obvious showstoppers from the first proof run. Large sequential writes no longer refresh the entire remote snapshot after every FUSE write chunk, broad tree traversal now avoids configured heavy directories such as `target` and `node_modules` through the mount path, known lookup/getattr metadata is served from the local snapshot instead of round-tripping for every stat, readdir calls are seeded from the initial snapshot even in TTL 0 mode, daemon stat/list-dir metadata now avoids hashing file contents, small files are cached as whole-file reads for Git/editor workloads, and namespace mutations update local metadata instead of refreshing the full snapshot.

MobFS is now fast enough for controlled personal dogfooding of the FUSE path: targeted source reads, source search, editor-style atomic writes, medium sequential writes, daemon restart recovery, and ignored-directory tree walks all complete quickly on a real Rust repo.

It is closer, and raw FUSE `git status` is now in the same rough range as the command-workflow path on the local TCP fixture. Native local operations are still faster, especially broad filesystem scans such as `find` and `du`. The right performance claim after this run is: fast enough for common remote coding workflows and improving toward native-like behavior, but not native-like for arbitrary filesystem scans yet.

Remaining performance work:

- keep narrowing raw FUSE `git status` against native on larger repositories
- improve broad traversal performance for `find`, `du`, and similar arbitrary filesystem scans
- improve same-mount recovery after a hard mid-write daemon kill so users do not need to unmount/remount
- test over a real remote network link, not just local TCP
