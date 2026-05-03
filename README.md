<div align='center'>
    <h3>MobFS</h3>
    <p>A no-local-code filesystem for remote-owned development workspaces</p>
    <br/>
    <br/>
</div>

MobFS gives local editors, GUI tools, and coding agents a normal filesystem path for code that still lives on a remote machine. The mount is the interface; the remote host owns the source tree, dependencies, secrets, builds, tests, and Git state.

It is not a generic SSHFS replacement. SSHFS is the better tool for a simple remote folder mount. MobFS is for development workspaces where raw file access is only half the workflow: source files should open locally, but repository commands and compute-heavy tasks should run where the code actually lives.

## Features

- **No-Local-Code Mount**: Mounts a remote workspace without creating a durable local checkout
- **Remote-Owned Commands**: Runs `mobfs run` and `mobfs git` on the code-owning machine from inside the mount
- **Developer-First FUSE Path**: Supports editor atomic saves, temp writes, renames, deletes, symlinks, chmod/mtime, flush, and fsync
- **Source-Tree-Aware Caching**: Prefetches small files, reuses directory metadata, and ignores heavy generated trees such as `target` and `node_modules`
- **Recovery-Oriented Writes**: Buffers sequential writes, streams large binary payloads, retries reconnectable operations, and journals mutating metadata operations
- **Encrypted Daemon Protocol**: Uses an authenticated encrypted TCP protocol between `mobfs` and `mobfsd`
- **SSH Tunnel Mode**: Connects through `ssh -L` so `mobfsd` can stay bound to localhost on the remote host
- **Mirror Mode**: Provides explicit `pull`, `push`, and `sync` workflows when a durable local copy is actually wanted
- **Operational Guardrails**: Requires allowed daemon roots, blocks unsafe relative paths, stores mirror tokens with `0600` permissions, and includes doctor/security checks

## Install

```bash
# From source
git clone https://github.com/plyght/mobfs.git
cd mobfs
cargo build --release
sudo cp target/release/mobfs /usr/local/bin/

# Mirror-only build for systems without FUSE libraries
cargo build --release --no-default-features
```

The default build enables FUSE support. On macOS, install macFUSE before using `mobfs mount`, approve the system extension if prompted, and run a mount doctor before first dogfooding:

```bash
mobfs mount-doctor /Volumes/app
```

## Usage

Start the remote daemon and mount the workspace with one command:

```bash
mobfs connect plyght@example.com:/srv/projects/app --name app
cd /Volumes/app
```

Use the mounted path for editors, search, agents, and focused file edits:

```bash
code .
rg "struct|enum|impl" src
```

Use MobFS commands for repository and build/test work:

```bash
mobfs git status
mobfs git diff
mobfs run cargo test
```

Running `git` directly through the FUSE mount works for normal cases, but metadata-heavy commands are faster and more reliable when executed with `mobfs git` on the remote host.

Use mirror mode only when you intentionally want a durable local working tree:

```bash
mobfs mirror plyght@example.com:/srv/projects/app --name app --token "$MOBFS_TOKEN" --ssh-tunnel
cd ~/MobFS/app
mobfs pull
mobfs push
mobfs sync
```

## Commands

```bash
# No-local-code mount path
mobfs connect user@host:/absolute/path --name app
mobfs mount host:/absolute/path --name app
mobfs mount user@host:/absolute/path --local ~/mnt/app --ssh-tunnel
mobfs unmount /Volumes/app

# Remote commands
mobfs run <command> [args...]
mobfs git <args...>
mobfs build --on builder@example.com -- cargo build --release
mobfs build --here --artifact build/App.app --remote-artifact artifacts/App.app -- brisk build

# Mirror mode
mobfs mirror host:/absolute/path --name app
mobfs pull
mobfs push
mobfs sync
mobfs status
mobfs watch

# Daemon and setup
mobfs token
mobfs connect plyght@example.com:/srv/projects/app --name app
mobfs setup /srv/projects --host example.com
mobfs remote start plyght@example.com --root ~/code
mobfs remote status plyght@example.com
mobfs remote restart plyght@example.com --root ~/code
mobfs daemon --bind 127.0.0.1:7727 --allow-root /srv/projects --token "$MOBFS_TOKEN"
mobfs doctor
mobfs security
mobfs bench --iterations 5 --mib 64
```

Useful aliases: `start` as `up`, `pull` as `get`, `push` as `put`, `sync` as `s`, `status` as `st`, `run` as `r`, `git` as `g`, and `open` as `o`.

## Configuration

For one-command SSH setup that creates the remote root, starts `mobfsd`, creates a token, opens an SSH tunnel, and mounts the workspace:

```bash
mobfs connect plyght@example.com:/srv/projects/app --name app
```

If you want to manage the daemon separately:

```bash
export MOBFS_TOKEN="$(mobfs token)"
mobfs remote start plyght@example.com --root /srv/projects --token "$MOBFS_TOKEN"
mobfs mount plyght@example.com:/srv/projects/app --name app --token "$MOBFS_TOKEN" --ssh-tunnel
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
ignore = [".mobfs", "target", "node_modules", ".mobfs.toml", ".DS_Store", "._*", ".mobfs-mountfs-journal.jsonl"]
connect_retries = 8
operation_retries = 5
cache_ttl_secs = 1
```

Remote targets use `host:/absolute/path` or `user@host:/absolute/path`. Folder-style backends are available for explicit mirror workflows with `icloud://` and `gdrive://`. R2 and S3 are represented in configuration but are not implemented yet.

## Usage Model

MobFS should be treated as a remote development mount, not as a backup system or a full local filesystem replacement. Keep authoritative code in Git on the remote machine, commit before risky bulk edits, and keep generated directories ignored.

Use raw FUSE paths for:

- editing source files
- reading focused files
- local search over source directories
- AI agent file inspection and patches
- editor temp-write and rename workflows

Prefer `mobfs run` or `mobfs git` for:

- Git status, diff, add, commit, checkout, and branch operations
- builds and tests
- package-manager commands
- language-server or tool commands that scan large dependency trees
- commands that should see remote secrets, architecture, or installed toolchains

Mount mode does not create `.mobfs.toml`, `.mobfs/token`, snapshots, or a project mirror under the mountpoint. It records active mounts in the user cache directory so `mobfs run` and `mobfs git` can resolve the remote workspace without writing config into source.

## Performance

Local proof testing on a real Rust workspace shows source reads, `rg`, editor atomic saves, symlinks, deletes, daemon restart recovery, 32 MiB writes, ignored-directory traversal, temporary branch checkout, and remote `cargo check` working through the mount.

Remote Raspberry Pi testing over an SSH tunnel shows the important distinction:

- raw FUSE reads and writes are usable for normal editing
- small-file prefetch makes source search and Git metadata reads much faster than the initial baseline
- large streaming writes are much improved but still slower than local TCP
- raw metadata-heavy Git over FUSE is slower than remote-native `mobfs git`

Use these defaults unless you are measuring a specific behavior:

- use `--cache-ttl-secs 1` for normal editing, agent work, search, and Git workflows
- use `--cache-ttl-secs 0` only when remote-side edits made outside MobFS must appear immediately
- use `mobfs git ...` and `mobfs run ...` from inside the mount for command latency
- avoid broad raw-FUSE scans of unignored build/cache directories

See `benchmarks.md` and `testing.md` for current measured results.

## Architecture

- `cli.rs`: Command definitions and argument parsing
- `config.rs`: Workspace configuration, remote target parsing, and defaults
- `crypto.rs`: Authenticated encrypted client/server stream setup
- `daemon.rs`: Remote filesystem server and root access policy
- `protocol.rs`: Wire protocol requests, responses, binary write streams, and command output
- `remote.rs`: Daemon client, retry handling, SSH tunneling, resumable transfers, and remote operations
- `storage.rs`: Backend abstraction for daemon and folder-backed remotes
- `snapshot.rs`: File metadata snapshots, planning, drift detection, and conflict logic
- `local.rs`: Local tree scanning, ignore handling, and snapshot persistence for mirror mode
- `journal.rs`: Local operation journal for mirror transfers
- `sync.rs`: User-facing workflows for mount, mirror, pull, push, sync, watch, run, build, and doctor
- `mountfs.rs`: No-local-code FUSE filesystem implementation
- `ui.rs`: Minimal terminal status output

## Development

```bash
cargo build
cargo test
cargo test --no-default-features
cargo clippy --all-targets --all-features -- -D warnings
```

Recommended dogfooding checks:

- `scripts/chaos.sh /Volumes/app`
- `mobfs bench --scale-files 50000 --iterations 3`
- editor atomic-save workloads
- Git status, diff, checkout, add, and commit workloads over FUSE
- IDE indexing, `rg`, language-server startup, and remote `mobfs run` workflows

## Security

Run `mobfs security` for the operational checklist.

- Bind `mobfsd` to `127.0.0.1` and connect with `--ssh-tunnel` unless the daemon is on a trusted private network
- Rotate tokens with `mobfs token`, restart the daemon with the new token, and update clients through `MOBFS_TOKEN` or the workspace token file
- Prefer one token and one `--allow-root` per workspace or team boundary
- Do not use `--allow-any-root` outside local tests
- Keep authoritative source in Git and treat MobFS recovery as operational resilience, not backup

## License

MIT License
