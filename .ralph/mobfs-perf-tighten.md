Goal: keep fixing, manually testing, and documenting MobFS performance until it is materially better for no-local-code FUSE workflows.

Checklist per iteration:
- Identify one concrete bottleneck from benchmarks/testing docs.
- Implement a focused production-grade fix.
- Run relevant manual proof tests, not broad unnecessary test suites unless needed.
- Update benchmarks.md/testing.md with current measured results.
- Stop when results are good enough to honestly describe as strong for dogfooding and improved toward native-like behavior, or when next work needs larger architecture changes.

## Iteration 1 progress

- Bottleneck: repeated directory listings and stat-like metadata calls still dominated `find`, `du`, and warm traversal workloads.
- Implemented: TTL-scoped directory-entry cache in `src/mountfs.rs`, plus earlier TTL metadata cache for lookup/getattr.
- Validated: `cargo fmt --check`, `cargo check --all-features`, release build, mounted Wax with `--cache-ttl-secs 1`, measured `git status`, `find`, `du`, `rg`, 32 MiB write, and editor-style write flow.
- Updated docs: `benchmarks.md` and `testing.md` with latest measurements.
- Latest measurements: warm `git status` 0.65-0.74s, `find` 0.18-0.24s, `du` 0.20s, `rg` 0.06s, 32 MiB write 1.38s.

## Iteration 2 progress

- Bottleneck: daemon `Stat` and `ListDir` computed full file SHA-256 hashes even when FUSE only needs size, mode, kind, mtime, and symlink target.
- Implemented: metadata-only `entry_meta_fast` path for daemon `Stat` and `ListDir`; snapshot generation still keeps full hashes for mirror/sync correctness.
- Validated: `cargo fmt --check`, `cargo check --all-features`, release build, mounted Wax with `--cache-ttl-secs 1`, measured `git status`, `find`, `du`, `rg`, 32 MiB write, and editor-style write flow.
- Updated docs: `benchmarks.md` and `testing.md` with latest measurements.
- Latest measurements: cold `git status` 1.92s, warm `git status` 0.67-0.68s, `find` 0.21-0.24s, `du` 0.12s, `rg` 0.06s, 32 MiB write 1.40s.

## Reflection after iteration 2

1. Accomplished: removed the worst FUSE write and traversal blockers. Large writes, `find`, and `du` moved from timeouts to sub-second/low-second behavior on the Wax fixture. Metadata caching and daemon metadata-only responses improved warm traversal substantially.
2. Working well: small focused changes produce measurable wins, and manual testing on the same fixture catches regressions quickly. Keeping `--cache-ttl-secs 0` fresh while optimizing nonzero TTL gives a useful dogfooding/debug split.
3. Not working/blocking: `git status` is still far from native, and editor/agent namespace operations are slower than they should be because mkdir/symlink/rename/remove still refreshed the whole snapshot before this iteration. Real-network behavior is still untested.
4. Approach adjustment: continue targeting full-snapshot refreshes and unnecessary content/hash work first. Avoid broad architecture changes until local proof bottlenecks are exhausted.
5. Next priorities: remove remaining snapshot refreshes from namespace mutations, retest editor temp-write and git/status/traversal, then decide whether next work should be mount registry support or deeper git/stat batching.

## Iteration 3 progress

- Bottleneck: namespace mutations (`mkdir`, `symlink`, `rename`, `unlink`, `rmdir`) still refreshed the entire remote snapshot after successful daemon operations.
- Implemented: local metadata updates for directory creation, symlink creation, recursive rename metadata movement, and recursive remove metadata deletion instead of full snapshot refreshes.
- Validated: `cargo fmt --check`, `cargo check --all-features`, `cargo clippy --all-targets --all-features -- -D warnings`, release build, mounted Wax with `--cache-ttl-secs 1`, measured repeated namespace/editor flows, `git status`, `find`, `du`, and 32 MiB write.
- Updated docs: `benchmarks.md` and `testing.md` with latest measurements.
- Latest measurements: editor/agent namespace pattern 0.22-0.27s including symlink/cleanup, cold `git status` 1.54s, warm `git status` 0.23s, `find` 0.27s, `du` 0.11s, 32 MiB write 1.40s.

## Iteration 4 progress

- Bottleneck/product mismatch: `mobfs run` and `mobfs git` did not work from a no-local-code mount root because those commands required `.mobfs.toml`; raw FUSE `git status` is much slower than native, while remote helper commands can be near-native if available from mounts.
- Implemented: non-source mount registry under the user cache directory. `mobfs mount` records the active mount config before entering FUSE, and `mobfs run`/`mobfs git` fall back to the registry when no mirror config exists. Registry lookup canonicalizes paths so `/tmp` and `/private/tmp` mount aliases work on macOS.
- Validated: `cargo fmt --check`, `cargo check --all-features`, `cargo clippy --all-targets --all-features -- -D warnings`, release build, mounted Wax, verified no `.mobfs*` files in mount root, ran `mobfs run pwd` and `mobfs git status --short` directly from mount root.
- Updated docs: `benchmarks.md` and `testing.md` with mount-root helper results.
- Latest measurements: `mobfs run pwd` from mount root 0.31s, `mobfs git status --short` from mount root 0.04s, raw FUSE cold `git status` 1.67s, `find` 0.21s, `du` 0.11s.

## Reflection after iteration 4

1. Accomplished: the no-local-code path is now practical. Large writes no longer time out, metadata-heavy traversals are fast when TTL caching is enabled, editor namespace operations are low-latency, and `mobfs run`/`mobfs git` now work from mount roots without writing project config into source.
2. Working well: the best wins came from eliminating whole-snapshot refreshes, avoiding content hashing on metadata RPCs, and using existing snapshot knowledge locally. Manual measurements give immediate feedback.
3. Not working/blocking: cold raw-FUSE `git status` still pays avoidable setup/listing costs, while `mobfs git` is already near-native. Real remote-network performance remains unmeasured.
4. Approach adjustment: prioritize cold-start/cache-warm behavior by precomputing cache structures from the initial snapshot, then stop local micro-optimizing unless a new local bottleneck is obvious.
5. Next priorities: seed directory cache from the initial snapshot at mount startup, retest cold `git status`/`find`/`du`, update docs, then decide whether the loop can stop before larger real-network tests.

## Iteration 5 progress

- Bottleneck: cold raw-FUSE traversal still called daemon `ListDir` even though mount startup already has a full ignored snapshot.
- Implemented: initial TTL directory cache seeded from the mount startup snapshot.
- Validated: `cargo fmt --check`, `cargo check --all-features`, `cargo clippy --all-targets --all-features -- -D warnings`, release build, mounted Wax with `--cache-ttl-secs 1`, measured cold/warm `git status`, `find`, `du`, mount-root `mobfs git`, editor namespace flow, and 32 MiB write.
- Updated docs: `benchmarks.md` and `testing.md` with latest measurements.
- Latest measurements: cold raw-FUSE `git status` 1.49s, warm raw-FUSE `git status` 0.24-0.25s, `find` 0.21-0.25s, `du` 0.11s, mount-root `mobfs git status --short` 0.13s, editor namespace pattern 0.25s, 32 MiB write 1.37s.
- Assessment: local proof path is now strong for dogfooding. Further meaningful work is likely deeper cold git/stat batching or real-network validation rather than another small local tweak.

## Iteration 6 progress

- Bottleneck/decision point: local proof numbers are now strong enough for dogfooding; the main remaining risks are documentation accuracy and larger validation outside local TCP.
- Implemented: README performance guidance and no-local-code command-helper docs. The README now states that mount mode writes no source config, uses a user-cache mount registry for `mobfs run`/`mobfs git`, recommends default TTL for normal workflows, and advises `mobfs git` for remote-native git latency.
- Validated: `cargo fmt --check`, `cargo check --all-features`, `cargo clippy --all-targets --all-features -- -D warnings`.
- Updated docs: README guidance now matches `benchmarks.md` and `testing.md` results.
- Assessment: stop local micro-optimization here. Next meaningful steps require either a real remote-network benchmark or a larger cold raw-FUSE git/status batching design; current local path is good enough to honestly call strong for controlled dogfooding.