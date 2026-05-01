<div align='center'>
    <h3>mobfs</h3>
    <p>A resilient filesystem workspace with local editing, remote authority, and reconnectable sync</p>
    <br/>
    <br/>
</div>

The mosh-inspired filesystem layer for remote development. mobfs gives Finder, editors, git, build tools, and coding agents a normal local directory while a small remote daemon owns the canonical tree. When the network drops, the workspace remains usable; when the connection returns, mobfs reconciles local and remote changes without silently overwriting conflicts.

## Features

- **Visible Local Workspace**: Creates normal directories under `~/MobFS` or a custom path for editors, agents, shells, and desktop tools
- **Resilient Sync Loop**: Watches local changes, scans remote changes, reconnects after drops, and keeps both sides converged
- **Remote Authority**: Stores the canonical project tree on a remote machine through `mobfsd`
- **Remote Compute**: Syncs local edits, then runs commands such as builds, tests, and git operations on the machine that owns the code
- **Encrypted Protocol**: Authenticates with a shared token, performs an X25519 handshake, derives session keys with HKDF-SHA256, and encrypts chunked file-transfer frames with ChaCha20-Poly1305
- **Daemon Root Policy**: Restricts mobfsd to explicitly allowed canonical workspace roots with repeatable `--allow-root` flags
- **Conflict Safety**: Uses a saved snapshot as the sync base and writes conflict copies instead of clobbering divergent edits
- **Provider Backends**: Supports iCloud and Google Drive folder-backed workspaces through the provider's local sync client
- **Simple Configuration**: Stores workspace state in `.mobfs.toml` and `.mobfs/` beside the visible local tree

## Install

Build the same binary on the local and remote machines:

```bash
# From source
git clone https://github.com/plyght/mobfs.git
cd mobfs
cargo build --release
sudo cp target/release/mobfs /usr/local/bin/

# For local development
cargo install --path .
```

## Usage

Run the daemon on the machine that stores the canonical project tree:

```bash
MOBFS_TOKEN='change-me' mobfs daemon --bind 0.0.0.0:7727 --allow-root /srv/project
```

Create and serve a local workspace from your client machine:

```bash
mobfs start host:/srv/project --name project --token 'change-me'
cd ~/MobFS/project
```

`start` is the frictionless path: it mounts the workspace when given a remote, pulls the tree, opens Finder on macOS, then stays online with the resilient sync loop.

For separate steps:

```bash
# Create/open a visible local workspace and perform the initial pull
mobfs mount host:/srv/project --name project --token 'change-me'

# Keep an existing workspace reconciled
cd ~/MobFS/project
mobfs serve
```

## Commands

```bash
# Initialize .mobfs.toml in the current directory
mobfs init host:/absolute/path --token secret

# Mount if needed, then run the resilient sync loop
MOBFS_TOKEN=secret mobfs start host:/absolute/path --name app

# Create a workspace without entering the long-running sync loop
mobfs mount host:/absolute/path --name app --token secret

# Reconcile manually
mobfs pull
mobfs push
mobfs sync
mobfs status

# Sync local edits, then run remote commands from the workspace root
mobfs run cargo test
mobfs run --no-sync cargo test
mobfs git status

# Watch local changes and push them
mobfs watch

# Check workspace, token availability, backend capabilities, and daemon connectivity
mobfs doctor

# Open the workspace in Finder on macOS
mobfs open
```

Common aliases are available: `up` for `start`, `get` for `pull`, `put` for `push`, `s` for `sync`, `st` for `status`, `r` for `run`, `g` for `git`, and `o` for `open`.

## Configuration

mobfs writes `.mobfs.toml` into each workspace:

```toml
[remote]
backend = "daemon"
host = "host"
user = ""
path = "/srv/project"
port = 7727
token = "change-me"

[local]
root = "/Users/me/MobFS/project"

[sync]
ignore = [".mobfs", ".git", "target", "node_modules", ".mobfs.toml"]
connect_retries = 8
operation_retries = 5
```

Configuration is discovered by walking upward from the current directory until `.mobfs.toml` is found. The `.mobfs/` directory stores sync snapshots and local state.

## Storage Backends

The daemon backend is the primary path for live filesystem work and remote compute:

```bash
mobfs daemon --bind 0.0.0.0:7727 --allow-root /absolute/path
mobfs start host:/absolute/path --name app
```

mobfsd is safe-by-default: it refuses to start unless you pass one or more `--allow-root` values or explicitly opt into `--allow-any-root` for unsafe local testing. An allowed root also permits canonical descendants, so `--allow-root /srv` can serve `/srv/project`.

iCloud and Google Drive can be used as folder-backed canonical storage roots. They do not provide remote compute, so `mobfs run` requires the daemon backend.

```bash
mobfs start icloud:///Users/me/Library/Mobile Documents/com~apple~CloudDocs/MobFS/app --name app
mobfs start gdrive:///Users/me/Library/CloudStorage/GoogleDrive-me@example.com/My Drive/MobFS/app --name app
```

Provider-backed workspaces support pull, push, sync, status, watch, serve, conflict detection, and network-drop recovery through the provider's local sync client. Common provider noise such as `.icloud`, `.tmp.drivedownload`, `.DS_Store`, `.TemporaryItems`, and `.Trashes` is ignored.

The config schema also reserves `r2` and `s3` backend names for future object-store implementations.

## Architecture

- `cli.rs`: Command definitions, arguments, aliases, and environment-backed options
- `config.rs`: Workspace configuration, remote parsing, backend selection, and defaults
- `daemon.rs`: Remote daemon, filesystem operations, command execution, and metadata handling
- `remote.rs`: Client connection, protocol calls, retries, and remote operation wrappers
- `crypto.rs`: Token-authenticated handshake and encrypted stream framing
- `protocol.rs`: Versioned request and response messages over the encrypted chunked transport
- `snapshot.rs`: File metadata snapshots, diff planning, status output, and conflict decisions
- `local.rs`: Local tree scanning, ignore handling, hashing, and snapshot persistence
- `storage.rs`: Unified storage abstraction for daemon and provider-folder backends
- `sync.rs`: Mount, start, pull, push, sync, watch, serve, run, doctor, and open workflows
- `ui.rs`: Minimal command-line status, summaries, and spinners
- `main.rs`: CLI entrypoint and command dispatch

## Development

```bash
cargo build
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features
```

Requires Rust with edition 2024 support. Key dependencies include clap, notify, serde/toml, walkdir, indicatif, x25519-dalek, hkdf, sha2, and chacha20poly1305. Integration tests use tempfile.

## License

MIT License
