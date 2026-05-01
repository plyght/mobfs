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

`mount` creates `~/MobFS/project`, pulls the remote tree, writes `.mobfs.toml`, and opens Finder on macOS. `serve` keeps both sides reconciled using the mobfs protocol.

## Commands

```sh
mobfs daemon --bind 0.0.0.0:7727 --token secret
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
