<div align='center'>
    <h3>MobFS</h3>
    <p>A resilient workspace filesystem for editing remote projects as if they were local</p>
    <br/>
    <br/>
</div>

MobFS gives remote workspaces a local, Finder-visible working tree with explicit sync, watch mode, remote command execution, and an optional FUSE mount. It is built for unstable links and mobile workflows where a full SSH session or network filesystem is too brittle, while still keeping file changes inspectable and recoverable.

## Features

- **Local Workspace Mirror**: Mounts a remote directory into a normal local folder, defaulting to `~/MobFS/<name>` when no path is provided
- **Resilient Sync Loop**: Watches local edits, scans remote changes, and reconciles both sides with retry-aware daemon communication
- **Explicit Pull/Push/Sync**: Supports one-way updates, dry-run planning, resumable uploads, operation journaling, and safe bidirectional reconciliation with conflict detection
- **Remote Command Execution**: Runs commands and git operations on the remote after syncing local edits
- **Daemon Backend**: Uses an authenticated encrypted TCP protocol between `mobfs` and `mobfsd`
- **Cloud Folder Backends**: Supports local folder-style backends for iCloud Drive and Google Drive paths
- **Optional FUSE Mount**: Provides an on-demand read-write filesystem mount when built with FUSE support
- **Portable Metadata Tracking**: Preserves file modes, mtimes, symlinks, and SHA-256 snapshots for drift detection
- **SSH Tunnel Mode**: Can connect to a daemon through `ssh -L` instead of exposing mobfsd directly
- **Secret File Storage**: Stores daemon tokens in `.mobfs/token` with `0600` permissions instead of plain `.mobfs.toml`
- **Benchmark Command**: Measures snapshot and daemon transfer throughput for real workspaces

## Install

```bash
# From source
git clone https://github.com/plyght/mobfs.git
cd mobfs
cargo build --release
sudo cp target/release/mobfs /usr/local/bin/
```

MobFS is a Rust project. The default build enables FUSE support; if your system does not have FUSE libraries available, build without default features:

```bash
cargo build --release --no-default-features
```

## Usage

Start a daemon on the machine that owns the remote workspace:

```bash
export MOBFS_TOKEN='shared-secret'
mobfs daemon --bind 0.0.0.0:7727 --allow-root /srv/projects --token "$MOBFS_TOKEN"
```

Mount a remote workspace locally:

```bash
mobfs mount example.com:/srv/projects/app --name app --token "$MOBFS_TOKEN" --ssh-tunnel
```

Work from the local folder, then sync explicitly:

```bash
cd ~/MobFS/app
mobfs status
mobfs pull
mobfs push
mobfs sync
mobfs sync --dry-run
```

Run a resilient sync loop:

```bash
mobfs start example.com:/srv/projects/app --name app --token "$MOBFS_TOKEN"
```

Run commands remotely from the workspace root:

```bash
mobfs run cargo test
mobfs git status
```

Create an optional FUSE mount:

```bash
mobfs mountfs example.com:/srv/projects/app /Volumes/app --token "$MOBFS_TOKEN"
```

## Configuration

MobFS stores workspace configuration in `.mobfs.toml`:

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

Configuration is discovered by walking upward from the current directory until `.mobfs.toml` is found.

Remote targets use one of these forms:

```bash
# Daemon backend
mobfs mount host:/absolute/path

# Folder-style backend
mobfs mount icloud:///absolute/path
mobfs mount gdrive:///absolute/path
```

R2 and S3 are represented in configuration, but are not implemented yet.

## Commands

```bash
# Create .mobfs.toml in the local workspace
mobfs init host:/absolute/path

# Mount and perform the initial pull
mobfs mount host:/absolute/path --name app

# Mount if needed, then run continuous sync
mobfs start host:/absolute/path --name app

# One-way sync operations
mobfs pull
mobfs push
mobfs push --dry-run

# Bidirectional sync with conflict detection
mobfs sync
mobfs sync --dry-run

# Show local and remote drift
mobfs status

# Run remote commands after syncing local edits
mobfs run <command> [args...]
mobfs git <args...>

# Watch local edits and push changes
mobfs watch

# Run the remote daemon
mobfs daemon --bind 0.0.0.0:7727 --allow-root /srv/projects --token "$MOBFS_TOKEN"

# Check workspace and daemon connectivity
mobfs doctor

# Benchmark snapshot and transfer performance
mobfs bench --iterations 5 --mib 64
```

Useful aliases: `start` as `up`, `pull` as `get`, `push` as `put`, `sync` as `s`, `status` as `st`, `run` as `r`, `git` as `g`, and `open` as `o`.

## Architecture

- `cli.rs`: Command definitions and argument parsing
- `config.rs`: Workspace configuration, remote target parsing, and defaults
- `crypto.rs`: Authenticated encrypted client/server stream setup
- `daemon.rs`: Remote filesystem server and root access policy
- `protocol.rs`: Wire protocol requests, responses, and streaming command output
- `remote.rs`: Daemon client, retry handling, SSH tunneling, resumable transfers, and remote operations
- `storage.rs`: Backend abstraction for daemon and folder-backed remotes
- `snapshot.rs`: File metadata snapshots, planning, drift detection, and conflict logic
- `local.rs`: Local tree scanning, ignore handling, and snapshot persistence
- `journal.rs`: Local operation journal for resumable/recoverable transfers
- `sync.rs`: User-facing workflows for mount, pull, push, sync, watch, run, and doctor
- `mountfs.rs`: Optional FUSE filesystem implementation
- `ui.rs`: Minimal terminal status output

## Development

```bash
cargo build
cargo test
cargo test --no-default-features
```

Requires Rust 2024 edition support. Key dependencies include clap, serde/toml, notify, chacha20poly1305, x25519-dalek, walkdir, indicatif, and optional fuser/libc for FUSE mounts.

## License

MIT License
