<div align='center'>
    <h3>MobFS</h3>
    <p>A no-local-code, mosh-like filesystem for remote workspaces</p>
    <br/>
    <br/>
</div>

MobFS is FUSE-first. The primary product path is a no-local-code, on-demand filesystem mount backed by a remote `mobfsd` daemon. Source files are read and written through the mount and are not mirrored into a durable local working tree. The local machine provides UI, editors, agents, and command entry; the remote machine owns the code and can run builds, tests, git, and other compute-heavy commands.

A durable local mirror still exists as `mobfs mirror` for explicit offline/cache workflows, but it is not the default.

## Features

- **No-local-code Mount**: `mobfs mount` creates a read-write FUSE filesystem without pulling the project into a local mirror
- **Remote-Owned Workspaces**: Remote source stays under the daemon root; local paths are mountpoints only
- **Resilient Operations**: Reconnects with retry/backoff, journals mutating FUSE metadata operations and write chunks, and replays them after remounts
- **Developer Tool Support**: Handles reads, writes, creates, truncates, renames, deletes, symlinks, chmod/mtime, flush, and fsync
- **macOS Noise Filtering**: Mount and mirror workflows ignore `.DS_Store` and AppleDouble `._*` sidecar files by default
- **Git and Agent Friendly**: Supports editor temp-file save patterns, agent-style temp writes, and git operations through mounted filesystems or `mobfs git`
- **Remote Command Execution**: `mobfs run` and `mobfs git` run on the remote workspace after syncing explicit mirror edits when used from mirror mode
- **Daemon Backend**: Uses an authenticated encrypted TCP protocol between `mobfs` and `mobfsd`
- **SSH Tunnel Mode**: Can connect to a daemon through `ssh -L` instead of exposing mobfsd directly
- **Safe Root Policy**: The daemon requires explicit `--allow-root` paths unless unsafe local testing is requested
- **Secret File Storage for Mirrors**: Mirror mode stores daemon tokens in `.mobfs/token` with `0600` permissions instead of plain `.mobfs.toml`
- **Benchmark Command**: Measures snapshot and daemon transfer throughput for real workspaces

## Install

```bash
git clone https://github.com/plyght/mobfs.git
cd mobfs
cargo build --release
sudo cp target/release/mobfs /usr/local/bin/
```

The default build enables FUSE support. On macOS, install macFUSE before using `mobfs mount`. If macFUSE was just installed, approve the system extension in System Settings and reboot if macOS asks. If your system does not have FUSE libraries available, build mirror-only mode:

```bash
cargo build --release --no-default-features
```

Before dogfooding the mount path on macOS, run:

```bash
mobfs mount-doctor /Volumes/app
```

Use `mobfs unmount /Volumes/app` to unmount and remove a clean mountpoint after testing.

## Quick Start

Start a daemon on the machine that owns the remote workspace:

```bash
export MOBFS_TOKEN="$(mobfs token)"
mobfs daemon --bind 127.0.0.1:7727 --allow-root /srv/projects --token "$MOBFS_TOKEN"
```

Mount a remote workspace locally without mirroring code:

```bash
mobfs mount nico@example.com:/srv/projects/app --name app --token "$MOBFS_TOKEN" --ssh-tunnel
cd /Volumes/app
```

On non-macOS systems, the default mount root is `~/MobFSMounts/<name>`. You can choose any mountpoint:

```bash
mobfs mount nico@example.com:/srv/projects/app --local ~/mnt/app --token "$MOBFS_TOKEN" --ssh-tunnel
```

Run remote commands from a configured mirror workspace or directly inside a no-local-code mount. Mount mode records a small user-cache registry outside the source tree so these helpers work without creating `.mobfs.toml` in the mount:

```bash
mobfs run cargo test
mobfs git status
```

For metadata-heavy commands such as `git status`, prefer `mobfs git` when you want remote-native behavior. Running `git` directly through FUSE works, but it is still slower than executing git on the machine that owns the repository.

Use mirror mode only when you intentionally want a durable local working tree:

```bash
mobfs mirror nico@example.com:/srv/projects/app --name app --token "$MOBFS_TOKEN" --ssh-tunnel
cd ~/MobFS/app
mobfs pull
mobfs push
mobfs sync
```

## Configuration

For a setup template, run:

```bash
mobfs setup /srv/projects --host example.com
```

For one-command SSH setup that creates the remote root and starts `mobfsd` on the remote host:

```bash
mobfs remote start nico@example.com --root ~/code
mobfs remote status nico@example.com
mobfs mount nico@example.com:~/code/app --ssh-tunnel
```

`remote start` records the remote daemon pid in `~/.mobfsd/daemon.pid` and logs to `~/.mobfsd/daemon.log`. Re-run it safely if the daemon is already up; use `mobfs remote restart ...` to stop the recorded pid and start a fresh daemon.

Mirror mode stores workspace configuration in `.mobfs.toml`:

```toml
[remote]
backend = "daemon"
host = "example.com"
user = ""
path = "/srv/projects/app"
port = 7727
ssh_tunnel = true

[local]
root = "/Users/alex/MobFS/app"

[sync]
ignore = [".mobfs", "target", "node_modules", ".mobfs.toml", ".DS_Store", "._*", ".mobfs-mountfs-journal.jsonl"]
connect_retries = 8
operation_retries = 5
```

Remote targets use one of these forms. Include `user@` when the SSH login user differs from the local username:

```bash
mobfs mount host:/absolute/path
mobfs mount user@host:/absolute/path
mobfs mirror host:/absolute/path
mobfs mirror user@host:/absolute/path
```

Folder-style backends are available for explicit mirror workflows:

```bash
mobfs mirror icloud:///absolute/path
mobfs mirror gdrive:///absolute/path
```

R2 and S3 are represented in configuration, but are not implemented yet.

## Commands

```bash
# No-local-code primary path
mobfs mount host:/absolute/path --name app
mobfs add host:/absolute/path --name app
mobfs mountfs host:/absolute/path /Volumes/app

# Durable mirror path
mobfs mirror host:/absolute/path --name app
mobfs init host:/absolute/path
mobfs start host:/absolute/path --name app
mobfs pull
mobfs push
mobfs push --dry-run
mobfs sync
mobfs sync --dry-run
mobfs status
mobfs watch

# Remote commands
mobfs run <command> [args...]
mobfs git <args...>
mobfs build --on builder@example.com -- cargo build --release
mobfs build --on builder@example.com --artifact target/release/app --out ./app -- cargo build --release

# Daemon, setup, security, and FUSE UX
mobfs token
mobfs setup /srv/projects --host example.com
mobfs remote start nico@example.com --root ~/code
mobfs remote status nico@example.com
mobfs remote restart nico@example.com --root ~/code
mobfs daemon --bind 127.0.0.1:7727 --allow-root /srv/projects --token "$MOBFS_TOKEN"
mobfs doctor
mobfs mount-doctor /Volumes/app
mobfs unmount /Volumes/app
mobfs rm /Volumes/app
mobfs security
mobfs bench --iterations 5 --mib 64
mobfs bench --scale-files 50000 --iterations 3
```

Useful aliases: `start` as `up`, `pull` as `get`, `push` as `put`, `sync` as `s`, `status` as `st`, `run` as `r`, `git` as `g`, and `open` as `o`.

`mobfs build --on <builder>` is for the case where the code-owning machine is not the fast machine. The builder connects back to the configured MobFS daemon, mounts the workspace into a temporary directory, runs the build command there, and cleans up afterward. Add `--mirror` to use an ephemeral mirror on the builder when a build tool needs native local filesystem behavior. Add `--artifact <path> --out <path>` to copy a build output back after the command succeeds.

## No-Local-Code Semantics

`mobfs mount` does not create `.mobfs.toml`, `.mobfs/token`, snapshots, or a project mirror under the mountpoint. Reads are fetched from the daemon on demand. Writes are sent to the daemon immediately. FUSE mode uses a small operation journal in the system temp directory so interrupted metadata and namespace operations can be replayed after reconnect. It also records active mounts in the user cache directory so `mobfs run` and `mobfs git` can resolve the remote workspace from inside a mount without writing config into source.

## Recovery and Conflict UX

Run `mobfs status` from inside a mount or mirror to see connection state and pending journal operations. If the daemon restarts or the network drops, normal metadata operations and buffered writes retry; any pending mount journal entries replay on the next successful reconnect or remount. If a hard failure happens mid-stream during a large write, MobFS is designed to fail quickly instead of hanging forever. After that kind of failure, run `mobfs status`, restart the remote daemon if needed, and remount if existing file handles keep returning `EIO`.

Mirror-mode conflicts stop the sync before clobbering either side. MobFS writes sibling conflict files named `.mobfs-conflict-local` and `.mobfs-conflict-remote`; choose one or merge them, replace the original file with the resolved content, delete the conflict copies, then run `mobfs sync` again.

## Safe Usage Model

Treat MobFS as a remote workspace mount, not as a backup system. Keep authoritative code in Git on the remote machine, commit before risky bulk edits, and prefer `mobfs git ...` and `mobfs run ...` for repository operations. Use raw FUSE paths for editors, agents, search, and focused file edits. Avoid running broad tools across unignored build/cache directories; keep `target`, `node_modules`, and similar generated trees ignored. For first dogfooding, use non-critical repositories until your sleep/wake, network, and editor workflows have passed.

Metadata lookups and directory entries use a short kernel cache TTL. The default is 1 second and can be changed with `mobfs mount --cache-ttl-secs <seconds>`. Use `0` while dogfooding remote-side edits from another shell, and use a small nonzero value for normal editor, agent, git, search, and traversal workflows. With a nonzero TTL, MobFS serves known metadata and directory entries from the initial snapshot and local mutation cache where safe. Direct reads go to the daemon first and only fall back to the last successful chunk if the daemon is temporarily unavailable.

Ignore entries match full path segments by default. Entries ending in `*` are treated as segment prefixes, so the default `._*` blocks macOS AppleDouble sidecar files from being created or listed through the mount.

This means local tools may still create their own caches outside the mount, and operating systems may keep normal kernel/page-cache data while the mount is active. MobFS does not intentionally mirror project source into a durable local workspace in mount mode.

## Performance Guidance

Current local proof testing on a real Rust repo shows the mount path is ready for controlled dogfooding: targeted source reads, `rg`, editor atomic-save patterns, symlinks, deletes, daemon restart recovery, 32 MiB writes, ignored-directory `find`, `du`, temporary branch checkout, and remote `cargo check` all complete successfully. It is still not a native local filesystem replacement for arbitrary metadata-heavy scans.

Use these rules of thumb:

- use default `--cache-ttl-secs 1` for normal editing, agent work, search, and git workflows
- use `--cache-ttl-secs 0` only when you need immediate visibility of remote-side edits made outside MobFS
- use `mobfs git ...` and `mobfs run ...` from inside the mount for remote-native command latency
- avoid broad raw-FUSE scans of unignored build/cache directories; keep heavy directories such as `target` and `node_modules` ignored

See `benchmarks.md` and `testing.md` for current measured proof results.

## Development

```bash
cargo build
cargo test
cargo test --no-default-features
cargo clippy --all-targets --all-features -- -D warnings
```

Recommended benchmark fixtures:

- `scripts/chaos.sh /Volumes/app`
- `mobfs bench --scale-files 50000 --iterations 3`
- `mobfs bench --scale-files 300000 --files-per-dir 1000 --iterations 3`
- a Linux-kernel-sized many-file tree
- a medium JavaScript app with `node_modules` ignored in mirror mode
- a Rust repo with `target` ignored in mirror mode
- editor atomic-save workloads
- git status/add/diff/commit workloads over FUSE
- IDE indexing, `rg`, language server startup, and `git status` over FUSE

## Architecture

- `cli.rs`: Command definitions and argument parsing
- `config.rs`: Workspace configuration, remote target parsing, and defaults
- `crypto.rs`: Authenticated encrypted client/server stream setup
- `daemon.rs`: Remote filesystem server and root access policy
- `protocol.rs`: Wire protocol requests, responses, and streaming command output
- `remote.rs`: Daemon client, retry handling, user-aware SSH tunneling, resumable transfers, and remote operations
- `storage.rs`: Backend abstraction for daemon and folder-backed remotes
- `snapshot.rs`: File metadata snapshots, planning, drift detection, and conflict logic
- `local.rs`: Local tree scanning, ignore handling, and snapshot persistence for mirror mode
- `journal.rs`: Local operation journal for mirror transfers
- `sync.rs`: User-facing workflows for mount, mirror, pull, push, sync, watch, run, and doctor
- `mountfs.rs`: No-local-code FUSE filesystem implementation
- `ui.rs`: Minimal terminal status output

## Security Hardening

Run `mobfs security` for the short operational checklist.

- Bind `mobfsd` to `127.0.0.1` and connect with `--ssh-tunnel` unless the daemon is on a trusted private network.
- Rotate tokens with `mobfs token`, restart the daemon with the new token, and update clients through `MOBFS_TOKEN` or the workspace token file.
- Prefer one token and one `--allow-root` per workspace or team boundary.
- Do not use `--allow-any-root` outside local tests.
- The daemon rejects absolute paths, `..`, and other non-normal relative path components before joining client paths to allowed roots.

## Crash Recovery Dogfooding

Abuse-test the FUSE path before release by killing the client and daemon during large writes, atomic editor saves, renames, `git add`, and `git status`. After restart, verify checksums for large files, verify no `.mobfs-upload-*` temp file replaced a final path, and run the project test suite remotely with `mobfs run`.

Current local and Raspberry Pi remote chaos testing confirms daemon restart recovery on an existing mount for normal buffered writes. Mount-mode writes are journaled before remote flush and replayed after reconnect. Hard mid-write daemon death should keep failing quickly instead of hanging, but still needs broader flaky-network and sleep/wake testing before MobFS can honestly claim full mosh-style seamlessness.

## License

MIT License
