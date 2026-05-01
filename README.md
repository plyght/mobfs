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
- **Resilient Operations**: Reconnects with retry/backoff, journals mutating FUSE operations, and replays them after drops
- **Developer Tool Support**: Handles reads, writes, creates, truncates, renames, deletes, symlinks, chmod/mtime, flush, and fsync
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

The default build enables FUSE support. On macOS, install macFUSE before using `mobfs mount`. If your system does not have FUSE libraries available, build mirror-only mode:

```bash
cargo build --release --no-default-features
```

## Quick Start

Start a daemon on the machine that owns the remote workspace:

```bash
export MOBFS_TOKEN="$(mobfs token)"
mobfs daemon --bind 127.0.0.1:7727 --allow-root /srv/projects --token "$MOBFS_TOKEN"
```

Mount a remote workspace locally without mirroring code:

```bash
mobfs mount example.com:/srv/projects/app --name app --token "$MOBFS_TOKEN" --ssh-tunnel
cd /Volumes/app
```

On non-macOS systems, the default mount root is `~/MobFSMounts/<name>`. You can choose any mountpoint:

```bash
mobfs mount example.com:/srv/projects/app --local ~/mnt/app --token "$MOBFS_TOKEN" --ssh-tunnel
```

Run remote commands from a configured mirror workspace, or run tools directly in the no-local-code mount when the tool can operate over FUSE:

```bash
mobfs run cargo test
mobfs git status
```

Use mirror mode only when you intentionally want a durable local working tree:

```bash
mobfs mirror example.com:/srv/projects/app --name app --token "$MOBFS_TOKEN" --ssh-tunnel
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
ignore = [".mobfs", "target", "node_modules", ".mobfs.toml"]
connect_retries = 8
operation_retries = 5
```

Remote targets use one of these forms:

```bash
mobfs mount host:/absolute/path
mobfs mirror host:/absolute/path
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

# Daemon and setup
mobfs token
mobfs setup /srv/projects --host example.com
mobfs daemon --bind 127.0.0.1:7727 --allow-root /srv/projects --token "$MOBFS_TOKEN"
mobfs doctor
mobfs bench --iterations 5 --mib 64
```

Useful aliases in mirror mode: `start` as `up`, `pull` as `get`, `push` as `put`, `sync` as `s`, `status` as `st`, `run` as `r`, `git` as `g`, and `open` as `o`.

## No-Local-Code Semantics

`mobfs mount` does not create `.mobfs.toml`, `.mobfs/token`, snapshots, or a project mirror under the mountpoint. Reads are fetched from the daemon on demand. Writes are sent to the daemon immediately. The only local persistence used by FUSE mode is a small operation journal in the system temp directory so interrupted metadata and namespace operations can be replayed after reconnect.

This means local tools may still create their own caches outside the mount, and operating systems may keep normal kernel/page-cache data while the mount is active. MobFS does not intentionally mirror project source into a durable local workspace in mount mode.

## Development

```bash
cargo build
cargo test
cargo test --no-default-features
cargo clippy --all-targets --all-features -- -D warnings
```

Recommended benchmark fixtures:

- a Linux-kernel-sized many-file tree
- a medium JavaScript app with `node_modules` ignored in mirror mode
- a Rust repo with `target` ignored in mirror mode
- editor atomic-save workloads
- git status/add/diff/commit workloads over FUSE

## Architecture

- `cli.rs`: Command definitions and argument parsing
- `config.rs`: Workspace configuration, remote target parsing, and defaults
- `crypto.rs`: Authenticated encrypted client/server stream setup
- `daemon.rs`: Remote filesystem server and root access policy
- `protocol.rs`: Wire protocol requests, responses, and streaming command output
- `remote.rs`: Daemon client, retry handling, SSH tunneling, resumable transfers, and remote operations
- `storage.rs`: Backend abstraction for daemon and folder-backed remotes
- `snapshot.rs`: File metadata snapshots, planning, drift detection, and conflict logic
- `local.rs`: Local tree scanning, ignore handling, and snapshot persistence for mirror mode
- `journal.rs`: Local operation journal for mirror transfers
- `sync.rs`: User-facing workflows for mount, mirror, pull, push, sync, watch, run, and doctor
- `mountfs.rs`: No-local-code FUSE filesystem implementation
- `ui.rs`: Minimal terminal status output

## License

MIT License
