# mobfs

A resilient filesystem workspace backed by `mobfsd`, not SSHFS/SFTP.

The model is “mosh for filesystem workspaces”: a visible local workspace for Finder, editors, git, build tools, and coding agents; a small remote daemon owns the canonical tree; the client reconnects and reconciles changes after network drops.

## Install

Build the same binary on both machines:

```sh
cargo install --path .
```

## Remote

Run the daemon on the machine that stores the code:

```sh
MOBFS_TOKEN='change-me' mobfs daemon --bind 0.0.0.0:7727
```

## Mac/client

Create a visible local workspace:

```sh
mobfs mount host:/srv/project --name project --token 'change-me'
cd ~/MobFS/project
mobfs serve
```

`start` is the frictionless path: it mounts when given a remote, pulls the tree, opens Finder on macOS, then stays online with the resilient sync loop. `mount` only creates `~/MobFS/project`, pulls the remote tree, writes `.mobfs.toml`, and opens Finder. `serve` keeps an existing workspace reconciled using the mobfs protocol.

## Commands

```sh
mobfs daemon --bind 0.0.0.0:7727 --token secret
MOBFS_TOKEN=secret mobfs start host:/absolute/path --name app
mobfs mount host:/absolute/path --name app --token secret
mobfs serve
mobfs sync
mobfs pull
mobfs push
mobfs status
mobfs run cargo test
mobfs doctor
mobfs open
```

## Remote compute

Use `mobfs run ...` to execute build, test, and git commands on the machine that owns the code:

```sh
mobfs run cargo test
mobfs run git status
mobfs run bun test
```

This keeps the local Mac as the fast editor/agent workspace while the remote does canonical compute.

## iCloud and Google Drive

mobfs supports provider-synced folder backends for iCloud and Google Drive. These do not run a hosting server; the cloud folder is the canonical storage root and mobfs reconciles it with the visible workspace.

```sh
mobfs start icloud:///Users/me/Library/Mobile Documents/com~apple~CloudDocs/MobFS/app --name app
mobfs start gdrive:///Users/me/Library/CloudStorage/GoogleDrive-me@example.com/My Drive/MobFS/app --name app
```

This supports pull, push, sync, status, watch, serve, conflict detection, and network-drop recovery through the provider's local sync client. `mobfs run` still requires the daemon backend because iCloud and Google Drive have no compute host.

The folder backend filters common provider noise such as `.icloud`, `.tmp.drivedownload`, `.DS_Store`, `.TemporaryItems`, and `.Trashes`.

## Storage direction

The config has an explicit storage backend field. `daemon` is the fast path for live filesystem work and remote compute. `icloud` and `gdrive` work today through provider-synced folders. The accepted backend names are `daemon`, `r2`, `s3`, `icloud`, and `gdrive`, so the config format can grow into object-store APIs without breaking workspaces.

## Protocol

Runtime file traffic uses mobfs' own framed protocol over a long-lived daemon connection. It does not use SSH or SFTP for filesystem operations.

## Conflict behavior

`mobfs sync` and `mobfs serve` use the last saved snapshot as the base. If local and remote both changed the same path, mobfs refuses to clobber either side and writes a `.mobfs-conflict-local` copy for the local file.

## Default ignores

- `.mobfs`
- `.mobfs.toml`
- `.git`
- `target`
- `node_modules`
