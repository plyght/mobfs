# Code Context

## Files Retrieved
1. `src/remote.rs` (lines 1-568) - client connect/retry, upload/download, SSH tunnel, and reconnect paths.
2. `src/daemon.rs` (lines 1-568) - server request handling, path policy, file ops, and journal replay.
3. `src/sync.rs` (lines 1-655) - sync/pull/push orchestration, conflict handling, and local/remote snapshot flow.
4. `src/mountfs.rs` (lines 1-900-ish; full file read in chunks to 568 lines) - FUSE read/write/create/rename/remove paths plus journal persistence.
5. `tests/daemon.rs` (lines 1-568) - end-to-end coverage for mount/push/pull/run, symlinks, conflicts, and reconnect behavior.

## Key Code
- `RemoteClient::op()` retries on any error and reconnects, but only for client-side operations.
- `RemoteClient::download_file()` writes to a temp path, then renames and applies metadata.
- `daemon::handle_request()` enforces `safe_join()` and `RootPolicy::check()` for all filesystem ops.
- `sync::resilient_sync_once()` writes conflict artifacts and then returns `Ok(())` instead of failing.
- `sync::apply_plan()` is simple and sequential; it assumes snapshot metadata stays valid during the run.
- `MobfsFuse` uses in-memory path/inode maps plus a journal file to replay interrupted metadata ops.
- `mountfs` treats most server failures as `EIO` and often refreshes the full snapshot after writes.

## Architecture
- `sync.rs` is the orchestration layer: it loads snapshots, computes plans, and delegates file transfer to `StorageClient`.
- `remote.rs` is the transport/client layer: it speaks the protocol to `daemon.rs` and retries/reconnects on transient failures.
- `daemon.rs` is the authority for remote filesystem access and root allowlist enforcement.
- `mountfs.rs` exposes the remote tree via FUSE and journals write-side mutations so it can recover after interruptions.
- Tests in `tests/daemon.rs` exercise the major happy paths and a few key failure modes (conflicts, allowlist rejection, reconnect).

## Failure Modes / High-Value Quick Changes
1. **MountFS journaling is not obviously durable across process crash between `record()` and remote success.**
   - Current replay helps, but journal writes use plain `std::fs::write`, so there is a small window where metadata ops can be lost or partially persisted.
   - Quick win: make journal appends atomic/flush-aware (write temp + rename or append+sync) and add a focused regression test.

2. **`sync::resilient_sync_once()` reports conflict artifacts as success.**
   - This can hide a failed sync from callers/automation even though the workspace is left in a conflict state.
   - Quick win: return a non-zero/err result after writing artifacts (or at least distinguish “conflict handled” from “sync succeeded”).

3. **`start_ssh_tunnel()` has a fixed 150ms sleep and no liveness check.**
   - Slow SSH startups can cause flaky connect failures that look like daemon/transport problems.
   - Quick win: poll the local tunnel port until connect succeeds or timeout, reusing existing backoff style.

4. **`RemoteClient::download_file()` may leave stale temp files on failure.**
   - If a chunk fetch or rename fails, the temp file can remain and later confuse retries/manual inspection.
   - Quick win: wrap temp cleanup in a guard or explicit error cleanup path.

5. **`daemon::WriteFileChunk` opens the upload temp file without `create(true)`.**
   - If `WriteFileStart` is missed or racey, chunk writes fail hard with `NotFound`.
   - Quick win: make chunk handling create/validate temp files more defensively and add a test for restart/replay edge cases.

6. **`mountfs` collapses many remote/client failures into `EIO`.**
   - This makes user-visible failures hard to diagnose and can mask permission/path errors.
   - Quick win: map a few common errors (`ENOENT`, `EACCES`, `EINVAL`) more precisely where easy.

## Start Here
Open `src/mountfs.rs` first. It has the highest failure-risk surface because it turns remote ops into user-facing filesystem mutations and already contains the most recovery logic.

# Project Context

## /Users/nicojaffer/.pi/agent/AGENTS.md
Process for every task:
1) Analyze the problem. Break it into steps to ensure full context. If your platform disallows exposing chain-of-thought, provide only a brief "Reasoning Path" summary.
1b) Contradiction/Ambiguity Gate ...

(See repo instructions for the rest; no contradictions found in the inspected files.)

# Reasoning Path
Inspected the requested source files plus the integration tests, then focused on places where failures currently become silent success, generic EIO, or fragile retry behavior.
