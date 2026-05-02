use crate::cli::{
    BenchArgs, BuildArgs, GitArgs, InitArgs, MountArgs, MountDoctorArgs, MountFsArgs, PullArgs,
    PushArgs, RemoteArgs, RemoteCommand, RemoteHostArgs, RemoteStatusArgs, RunArgs, ServeArgs,
    SetupArgs, SetupRemoteArgs, StartArgs, SyncArgs, UnmountArgs, WatchArgs,
};
use crate::config::{
    AppConfig, DEFAULT_CONNECT_RETRIES, DEFAULT_OP_RETRIES, LocalConfig, RemoteConfig, STATE_DIR,
    SyncConfig, parse_remote,
};
use crate::error::{MobfsError, Result};
use crate::local;
use crate::snapshot::{
    EntryKind, PlanItem, Snapshot, StatusItem, pull_items, push_items, status_plan, sync_items,
};
use crate::storage::StorageClient;
use crate::ui;
use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

pub fn init(args: InitArgs) -> Result<()> {
    let target = parse_remote(&args.remote)?;
    let root = args.local.unwrap_or(std::env::current_dir()?);
    let config = new_config(target, root, args.port, args.token, args.ssh_tunnel);
    write_config(&config)?;
    ui::ok("initialized .mobfs.toml");
    Ok(())
}

pub fn start(args: StartArgs) -> Result<()> {
    if let Some(remote) = args.remote {
        mirror(MountArgs {
            remote,
            name: args.name,
            local: args.local,
            port: args.port,
            token: args.token,
            ssh_tunnel: args.ssh_tunnel,
            cache_ttl_secs: 1,
            no_open: args.no_open,
        })?;
    }
    serve(ServeArgs {
        debounce_ms: args.debounce_ms,
        remote_interval: args.remote_interval,
        delete: args.delete,
    })
}

pub fn mountfs(args: MountFsArgs) -> Result<()> {
    #[cfg(feature = "fuse")]
    {
        let (remote, mountpoint) = match args.paths.as_slice() {
            [mountpoint] => (None, std::path::PathBuf::from(mountpoint)),
            [remote, mountpoint] => (Some(remote.clone()), std::path::PathBuf::from(mountpoint)),
            _ => {
                return Err(MobfsError::Config(
                    "usage: mobfs mountfs [remote] <mountpoint>".to_string(),
                ));
            }
        };
        let config = match remote {
            Some(remote) => crate::mountfs::config_from_remote(
                remote,
                &mountpoint,
                args.port,
                args.token,
                args.ssh_tunnel,
            )?,
            None => AppConfig::load()?,
        };
        crate::mountfs::prepare_mountpoint(&mountpoint)?;
        crate::mountfs::mount(config, mountpoint)
    }
    #[cfg(not(feature = "fuse"))]
    {
        let _ = args;
        Err(MobfsError::Config(
            "mobfs mountfs requires building with --features fuse".to_string(),
        ))
    }
}

pub fn mount(args: MountArgs) -> Result<()> {
    #[cfg(feature = "fuse")]
    {
        let target = parse_remote(&args.remote)?;
        let root = match args.local {
            Some(path) => path,
            None => {
                default_no_local_code_mountpoint(args.name.as_deref(), &target.host, &target.path)?
            }
        };
        let mut config = new_config(target, root.clone(), args.port, args.token, args.ssh_tunnel);
        config.sync.cache_ttl_secs = args.cache_ttl_secs;
        crate::mountfs::prepare_mountpoint(&root)?;
        ui::added(
            "mounting no-local-code filesystem",
            root.display().to_string(),
        );
        ui::info("cache ttl", format!("{}s", config.sync.cache_ttl_secs));
        save_mount_registry_entry(&config)?;
        crate::mountfs::mount(config, root)
    }
    #[cfg(not(feature = "fuse"))]
    {
        let _ = args;
        Err(MobfsError::Config(
            "mobfs mount is no-local-code FUSE-first and requires building with --features fuse; use `mobfs mirror` for a durable local mirror".to_string(),
        ))
    }
}

pub fn mirror(args: MountArgs) -> Result<()> {
    let target = parse_remote(&args.remote)?;
    let root = match args.local {
        Some(path) => path,
        None => default_mirror_root(args.name.as_deref(), &target.host, &target.path)?,
    };
    let config = new_config(target, root.clone(), args.port, args.token, args.ssh_tunnel);
    write_config(&config)?;
    ui::added("mirrored", root.display().to_string());
    let spinner = ui::spinner("initial pull");
    let mut client = StorageClient::connect(config.clone())?;
    let remote = client.snapshot()?;
    let local_snapshot = local::snapshot(&config)?;
    let plan = pull_items(&local_snapshot, &remote, false);
    ensure_local_dirs(&config, &remote)?;
    apply_plan(&mut client, &config, &remote, &plan)?;
    local::save_snapshot(&config, &remote)?;
    spinner.finish_and_clear();
    if !args.no_open {
        open_path(&root)?;
    }
    ui::ok("ready");
    Ok(())
}

fn new_config(
    target: crate::config::RemoteTarget,
    root: std::path::PathBuf,
    port: u16,
    token: Option<String>,
    ssh_tunnel: bool,
) -> AppConfig {
    AppConfig {
        remote: RemoteConfig {
            backend: target.backend,
            host: target.host,
            user: target.user,
            path: target.path,
            port,
            identity: None,
            ssh_tunnel,
            token: Some(
                token
                    .or_else(|| std::env::var("MOBFS_TOKEN").ok())
                    .unwrap_or_else(crate::config::generate_token),
            ),
        },
        local: LocalConfig { root },
        sync: SyncConfig {
            ignore: vec![
                STATE_DIR.to_string(),
                "target".to_string(),
                "node_modules".to_string(),
                ".mobfs.toml".to_string(),
                ".DS_Store".to_string(),
                "._*".to_string(),
                ".mobfs-mountfs-journal.jsonl".to_string(),
            ],
            connect_retries: DEFAULT_CONNECT_RETRIES,
            operation_retries: DEFAULT_OP_RETRIES,
            cache_ttl_secs: 1,
        },
    }
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn pending_write_count(config: &AppConfig, mirror_workspace: bool) -> usize {
    let mirror_pending = if mirror_workspace {
        crate::journal::pending_count(config).unwrap_or(0)
    } else {
        0
    };
    #[cfg(feature = "fuse")]
    {
        mirror_pending + crate::mountfs::pending_journal_ops(config).unwrap_or(0)
    }
    #[cfg(not(feature = "fuse"))]
    {
        mirror_pending
    }
}

fn write_config(config: &AppConfig) -> Result<()> {
    fs::create_dir_all(config.local.root.join(STATE_DIR))?;
    let previous = std::env::current_dir()?;
    fs::create_dir_all(&config.local.root)?;
    std::env::set_current_dir(&config.local.root)?;
    let result = config.save();
    std::env::set_current_dir(previous)?;
    result
}

pub fn pull(args: PullArgs) -> Result<()> {
    let config = AppConfig::load()?;
    let spinner = ui::spinner("connecting");
    let mut client = StorageClient::connect(config.clone())?;
    spinner.set_message("scanning remote");
    let remote = client.snapshot()?;
    let local_snapshot = local::snapshot(&config)?;
    spinner.set_message("pulling files");
    let plan = pull_items(&local_snapshot, &remote, args.delete);
    let count = plan.len();
    if args.dry_run {
        print_plan(&plan);
        spinner.finish_and_clear();
        return Ok(());
    }
    ensure_local_dirs(&config, &remote)?;
    apply_plan(&mut client, &config, &remote, &plan)?;
    local::save_snapshot(&config, &remote)?;
    spinner.finish_and_clear();
    if count == 0 {
        ui::ok("already up to date");
    } else {
        ui::summary("changes pulled", count);
    }
    Ok(())
}

pub fn push(args: PushArgs) -> Result<()> {
    let config = AppConfig::load()?;
    let spinner = ui::spinner("scanning local");
    let local_snapshot = local::snapshot(&config)?;
    spinner.set_message("connecting");
    let mut client = StorageClient::connect(config.clone())?;
    spinner.set_message("pushing changes");
    let count = push_plan(&mut client, &local_snapshot, args.delete, args.dry_run)?;
    let remote = client.snapshot()?;
    local::save_snapshot(&config, &remote)?;
    spinner.finish_and_clear();
    if count == 0 {
        ui::ok("already up to date");
    } else {
        ui::summary("changes pushed", count);
    }
    Ok(())
}

pub fn sync(args: SyncArgs) -> Result<()> {
    let config = AppConfig::load()?;
    let spinner = ui::spinner("scanning local and remote");
    let base = local::load_snapshot(&config)?;
    let local_snapshot = local::snapshot(&config)?;
    let mut client = StorageClient::connect(config.clone())?;
    let remote = client.snapshot()?;
    spinner.finish_and_clear();
    let plan = sync_items(&base, &local_snapshot, &remote, args.delete);
    if plan
        .iter()
        .any(|item| matches!(item, PlanItem::Conflict(_)))
    {
        for item in &plan {
            if let PlanItem::Conflict(path) = item {
                write_conflict_artifacts(&config, &mut client, &remote, path)?;
                ui::change("conflict", path);
            }
        }
        return Err(MobfsError::Remote(
            "sync stopped because both sides changed the same path; local and remote conflict copies were written next to conflicted files. Resolve by choosing or merging the .mobfs-conflict-local and .mobfs-conflict-remote files, then run `mobfs sync` again".to_string(),
        ));
    }
    let count = plan.len();
    if args.dry_run {
        print_plan(&plan);
        return Ok(());
    }
    apply_plan(&mut client, &config, &remote, &plan)?;
    let remote = client.snapshot()?;
    local::save_snapshot(&config, &remote)?;
    if count == 0 {
        ui::ok("clean");
    } else {
        ui::summary("changes synced", count);
    }
    Ok(())
}

pub fn status() -> Result<()> {
    let (config, mirror_workspace) = load_command_config()?;
    let pending = pending_write_count(&config, mirror_workspace);
    let spinner = ui::spinner("scanning local and remote");
    let local_snapshot = if mirror_workspace {
        local::snapshot(&config)?
    } else {
        Snapshot::default()
    };
    let mut client = match StorageClient::connect(config.clone()) {
        Ok(client) => client,
        Err(error) => {
            spinner.finish_and_clear();
            ui::change("connection", "reconnecting");
            ui::change("pending-writes", pending.to_string());
            ui::change("last-error", error.to_string());
            return Ok(());
        }
    };
    let remote = match client.snapshot() {
        Ok(remote) => remote,
        Err(error) => {
            spinner.finish_and_clear();
            ui::change("connection", "reconnecting");
            ui::change("pending-writes", pending.to_string());
            ui::change("last-error", error.to_string());
            return Ok(());
        }
    };
    spinner.finish_and_clear();
    ui::change("connection", "connected");
    ui::change("mode", if mirror_workspace { "mirror" } else { "mount" });
    ui::change("pending-writes", pending.to_string());
    if pending > 0 {
        ui::warn(
            "pending mount journal operations will replay on the next successful reconnect or remount",
        );
    }
    ui::change("last-synced", unix_timestamp().to_string());
    if !mirror_workspace {
        ui::info("remote-entries", remote.entries.len().to_string());
        return Ok(());
    }
    let items = status_plan(&local_snapshot, &remote);
    if items.is_empty() {
        ui::ok("clean");
        return Ok(());
    }
    let count = items.len();
    for item in items {
        match item {
            StatusItem::LocalOnly(path) => ui::change("local-only", path),
            StatusItem::RemoteOnly(path) => ui::change("remote-only", path),
            StatusItem::Modified(path) => ui::change("modified", path),
        }
    }
    ui::summary("differences", count);
    Ok(())
}

pub fn run(args: RunArgs) -> Result<()> {
    run_remote(args.command, !args.no_sync)
}

pub fn build(args: BuildArgs) -> Result<()> {
    let (config, mirror_workspace) = load_command_config()?;
    if args.here {
        return run_here_build(args, config, mirror_workspace);
    }
    if !args.no_sync && mirror_workspace {
        sync(SyncArgs {
            delete: false,
            dry_run: false,
        })?;
    }
    run_builder_build(args, config)
}

pub fn git(args: GitArgs) -> Result<()> {
    let mut command = Vec::with_capacity(args.args.len() + 1);
    command.push("git".to_string());
    command.extend(args.args);
    run_remote(command, !args.no_sync)
}

fn run_remote(command: Vec<String>, sync_first: bool) -> Result<()> {
    let (config, mirror_workspace) = load_command_config()?;
    if sync_first && mirror_workspace {
        sync(SyncArgs {
            delete: false,
            dry_run: false,
        })?;
    }
    let mut client = StorageClient::connect(config)?;
    let label = command.join(" ");
    ui::info("run", label);
    let (code, _, _) = client.run(command)?;
    if code.unwrap_or(1) == 0 {
        Ok(())
    } else {
        Err(MobfsError::Remote(format!(
            "remote command exited with {}",
            code.map(|value| value.to_string())
                .unwrap_or_else(|| "signal".to_string())
        )))
    }
}

fn run_builder_build(args: BuildArgs, config: AppConfig) -> Result<()> {
    let builder = args.builder.as_ref().ok_or_else(|| {
        MobfsError::Config("choose where to build: use `mobfs build --here -- <cmd>` or `mobfs build --on user@host -- <cmd>`".to_string())
    })?;
    if args.artifact.is_some() && args.out.is_none() {
        return Err(MobfsError::Config(
            "--artifact requires --out so mobfs knows where to copy the build output".to_string(),
        ));
    }
    let token = config.remote.token.clone().ok_or_else(|| {
        MobfsError::Config(
            "builder builds require a mobfs token in config or MOBFS_TOKEN".to_string(),
        )
    })?;
    let source = remote_source_spec(&config);
    let tunnel_flag = if config.remote.ssh_tunnel {
        " --ssh-tunnel"
    } else {
        ""
    };
    let command = shell_words(&args.command);
    let build_id = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    );
    let artifact_copy = args.artifact.as_ref().map(|artifact| {
        let file_name = artifact
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("artifact");
        format!(
            "mkdir -p ~/.mobfs-build-artifacts/{id}; cp {artifact} ~/.mobfs-build-artifacts/{id}/{file_name};",
            id = build_id,
            artifact = shell_path_quote(&artifact.display().to_string()),
            file_name = shell_quote(file_name),
        )
    });
    let workspace_setup = if args.mirror {
        format!(
            "mobfs mirror {source} --local \"$work/src\"{tunnel_flag} --token \"$MOBFS_TOKEN\" --no-open >/dev/null; cd \"$work/src\"; mobfs pull >/dev/null;",
            source = shell_quote(&source),
            tunnel_flag = tunnel_flag,
        )
    } else {
        format!(
            "mkdir -p \"$work/src\"; mobfs mount {source} --local \"$work/src\"{tunnel_flag} --token \"$MOBFS_TOKEN\" --no-open >/dev/null; cd \"$work/src\";",
            source = shell_quote(&source),
            tunnel_flag = tunnel_flag,
        )
    };
    let cleanup = if args.mirror {
        "rm -rf \"$work\"".to_string()
    } else {
        "mobfs unmount \"$work/src\" >/dev/null 2>&1 || true; rm -rf \"$work\"".to_string()
    };
    let remote_command = format!(
        "set -e; command -v mobfs >/dev/null || {{ echo 'mobfs not found in PATH on builder' >&2; exit 127; }}; export MOBFS_TOKEN={token}; work=$(mktemp -d /tmp/mobfs-build.XXXXXX); trap '{cleanup}' EXIT; {workspace_setup} {command}; {artifact_copy}",
        token = shell_quote(&token),
        cleanup = cleanup.replace('\'', "'\\''"),
        workspace_setup = workspace_setup,
        command = command,
        artifact_copy = artifact_copy.unwrap_or_default(),
    );
    ui::info("builder", builder);
    ui::info("source", &source);
    ui::info("build", args.command.join(" "));
    let status = Command::new("ssh")
        .arg(builder)
        .arg(remote_command)
        .status()?;
    if !status.success() {
        return Err(MobfsError::Remote(format!(
            "builder command failed with {status}"
        )));
    }
    if let (Some(artifact), Some(out)) = (args.artifact.as_ref(), args.out.as_ref()) {
        copy_builder_artifact(builder, &build_id, artifact, out)?;
    }
    Ok(())
}

fn run_here_build(args: BuildArgs, source_config: AppConfig, mirror_workspace: bool) -> Result<()> {
    if !args.no_sync && mirror_workspace {
        sync(SyncArgs {
            delete: false,
            dry_run: false,
        })?;
    }
    let generated_workdir = args.workdir.is_none();
    let stage_root = args
        .workdir
        .clone()
        .unwrap_or(default_local_build_root(&source_config)?);
    let mut build_config = source_config.clone();
    build_config.local.root = stage_root.clone();
    let spinner = ui::spinner("staging remote workspace");
    fs::create_dir_all(&stage_root)?;
    let mut client = StorageClient::connect(build_config.clone())?;
    let remote = client.snapshot()?;
    ensure_local_dirs(&build_config, &remote)?;
    let local_snapshot = local::snapshot(&build_config)?;
    let plan = pull_items(&local_snapshot, &remote, false);
    apply_plan(&mut client, &build_config, &remote, &plan)?;
    local::save_snapshot(&build_config, &remote)?;
    spinner.finish_and_clear();
    ui::info("staged", stage_root.display().to_string());
    ui::info("build", args.command.join(" "));
    let status = Command::new(&args.command[0])
        .args(&args.command[1..])
        .current_dir(&stage_root)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()?;
    if !status.success() {
        return Err(MobfsError::Remote(format!(
            "local build failed with {status}; staged workspace left at {}",
            stage_root.display()
        )));
    }
    if let Some(artifact) = args.artifact {
        let remote_artifact = args.remote_artifact.as_deref().unwrap_or(&artifact);
        let spinner = ui::spinner("publishing artifact");
        publish_local_artifact(&mut client, &build_config, &artifact, remote_artifact)?;
        spinner.finish_and_clear();
        ui::ok(format!("published {}", remote_artifact.display()));
    }
    if args.keep || !generated_workdir {
        ui::info("kept", stage_root.display().to_string());
    } else {
        fs::remove_dir_all(&stage_root)?;
    }
    ui::ok("local build complete");
    Ok(())
}

fn remote_source_spec(config: &AppConfig) -> String {
    let host = if config.remote.user.is_empty() {
        config.remote.host.clone()
    } else {
        format!("{}@{}", config.remote.user, config.remote.host)
    };
    format!("{}:{}", host, config.remote.path)
}

fn shell_words(words: &[String]) -> String {
    words
        .iter()
        .map(|word| shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn copy_builder_artifact(builder: &str, build_id: &str, artifact: &Path, out: &Path) -> Result<()> {
    let file_name = artifact
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("artifact");
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    let remote_path = format!(
        "{}:~/.mobfs-build-artifacts/{}/{}",
        builder, build_id, file_name
    );
    let status = Command::new("scp").arg(remote_path).arg(out).status()?;
    if status.success() {
        ui::ok(format!("artifact copied to {}", out.display()));
        Ok(())
    } else {
        Err(MobfsError::Remote(format!(
            "artifact copy failed with {status}"
        )))
    }
}

fn default_local_build_root(config: &AppConfig) -> Result<std::path::PathBuf> {
    let base = dirs::cache_dir().ok_or_else(|| {
        MobfsError::Config("could not determine user cache directory for local build".to_string())
    })?;
    let name = default_workspace_name(None, &config.remote.host, &config.remote.path);
    let id = format!("{}-{}", unix_timestamp(), std::process::id());
    Ok(base.join("mobfs").join("local-builds").join(name).join(id))
}

fn publish_local_artifact(
    client: &mut StorageClient,
    config: &AppConfig,
    artifact: &Path,
    remote_artifact: &Path,
) -> Result<()> {
    let src = config.local.root.join(artifact);
    if !src.exists() {
        return Err(MobfsError::InvalidPath(format!(
            "artifact {} does not exist",
            src.display()
        )));
    }
    if src.is_dir() {
        for entry in WalkDir::new(&src).into_iter() {
            let entry = entry?;
            let path = entry.path();
            let rel_inside = path.strip_prefix(&src).map_err(|_| {
                MobfsError::InvalidPath(format!("invalid artifact path {}", path.display()))
            })?;
            let remote_rel = remote_artifact.join(rel_inside);
            let remote_rel = path_to_rel(&remote_rel)?;
            if entry.file_type().is_dir() {
                client.mkdir_p(&remote_rel)?;
            } else {
                let stage_rel = path_to_rel(&artifact.join(rel_inside))?;
                if stage_rel == remote_rel {
                    client.upload_file(&stage_rel)?;
                } else {
                    upload_artifact_alias(client, config, path, &remote_rel)?;
                }
            }
        }
    } else {
        let stage_rel = path_to_rel(artifact)?;
        let remote_rel = path_to_rel(remote_artifact)?;
        if stage_rel == remote_rel {
            client.upload_file(&stage_rel)?;
        } else {
            upload_artifact_alias(client, config, &src, &remote_rel)?;
        }
    }
    Ok(())
}

fn upload_artifact_alias(
    client: &mut StorageClient,
    config: &AppConfig,
    src: &Path,
    remote_rel: &str,
) -> Result<()> {
    let dst = config.local.root.join(remote_rel);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(src, &dst)?;
    client.upload_file(remote_rel)
}

fn path_to_rel(path: &Path) -> Result<String> {
    if path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(MobfsError::InvalidPath(format!(
            "expected relative path, got {}",
            path.display()
        )));
    }
    Ok(path.to_string_lossy().trim_start_matches('/').to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct MountRegistry {
    entries: Vec<AppConfig>,
}

fn load_command_config() -> Result<(AppConfig, bool)> {
    match AppConfig::load() {
        Ok(config) => Ok((config, true)),
        Err(config_error) => match load_mount_registry_entry()? {
            Some(config) => Ok((config, false)),
            None => Err(config_error),
        },
    }
}

fn save_mount_registry_entry(config: &AppConfig) -> Result<()> {
    let path = mount_registry_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut registry = read_mount_registry(&path)?;
    registry
        .entries
        .retain(|entry| entry.local.root != config.local.root);
    registry.entries.push(config.clone());
    fs::write(&path, toml::to_string_pretty(&registry)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn load_mount_registry_entry() -> Result<Option<AppConfig>> {
    let path = mount_registry_path()?;
    let registry = read_mount_registry(&path)?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    Ok(registry
        .entries
        .into_iter()
        .filter_map(|mut entry| {
            let root = entry.local.root.canonicalize().ok()?;
            if cwd.starts_with(&root) {
                entry.local.root = root;
                Some(entry)
            } else {
                None
            }
        })
        .max_by_key(|entry| entry.local.root.as_os_str().len()))
}

fn read_mount_registry(path: &std::path::Path) -> Result<MountRegistry> {
    if !path.exists() {
        return Ok(MountRegistry::default());
    }
    Ok(toml::from_str(&fs::read_to_string(path)?)?)
}

fn mount_registry_path() -> Result<std::path::PathBuf> {
    let base = dirs::cache_dir().ok_or_else(|| {
        MobfsError::Config(
            "could not determine user cache directory for mount registry".to_string(),
        )
    })?;
    Ok(base.join("mobfs").join("mounts.toml"))
}

pub fn token() -> Result<()> {
    println!("{}", crate::config::generate_token());
    Ok(())
}

pub fn unmount(args: UnmountArgs) -> Result<()> {
    let mountpoint = match args.mountpoint {
        Some(path) => path,
        None => load_command_config()?.0.local.root,
    };
    let status = if cfg!(target_os = "macos") {
        Command::new("diskutil")
            .arg("unmount")
            .arg(&mountpoint)
            .status()
    } else {
        Command::new("umount").arg(&mountpoint).status()
    }?;
    if !status.success() {
        return Err(MobfsError::Remote(format!(
            "failed to unmount {}",
            mountpoint.display()
        )));
    }
    match fs::remove_dir(&mountpoint) {
        Ok(()) => ui::ok(format!("unmounted and removed {}", mountpoint.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ui::ok(format!("unmounted {}", mountpoint.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => ui::warn(format!(
            "unmounted but left non-empty mountpoint {}",
            mountpoint.display()
        )),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn mount_doctor(args: MountDoctorArgs) -> Result<()> {
    ui::info("mountpoint", args.mountpoint.display().to_string());
    if args.mountpoint.exists() {
        if args.mountpoint.is_dir() {
            ui::ok("mountpoint directory exists");
        } else {
            ui::warn("mountpoint exists but is not a directory");
        }
    } else {
        ui::warn("mountpoint does not exist yet");
    }
    #[cfg(target_os = "macos")]
    {
        if std::path::Path::new("/Library/Filesystems/macfuse.fs").exists() {
            ui::ok("macFUSE installed");
        } else {
            ui::warn(
                "macFUSE missing; install it and approve the system extension before mounting",
            );
        }
        if Command::new("mount")
            .output()
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .contains(args.mountpoint.to_string_lossy().as_ref())
            })
            .unwrap_or(false)
        {
            ui::ok("mountpoint appears in mount output");
        } else {
            ui::warn("mountpoint is not currently mounted");
        }
        check_tool("diskutil");
        check_tool("open");
    }
    #[cfg(not(target_os = "macos"))]
    ui::info("platform", "macOS-specific Finder checks skipped");
    Ok(())
}

pub fn security() -> Result<()> {
    ui::section("Security hardening");
    ui::bullet(
        "Bind mobfsd to 127.0.0.1 and use --ssh-tunnel unless you are on a trusted private network.",
    );
    ui::bullet(
        "Rotate tokens with `mobfs token`, restart mobfsd with the new value, and update clients via MOBFS_TOKEN or the workspace token file.",
    );
    ui::bullet("Prefer one token and one --allow-root per workspace or team boundary.");
    ui::bullet("Never run mobfsd with --allow-any-root outside local tests.");
    ui::bullet(
        "Audit daemon logs for rejected roots and invalid paths after onboarding new workspaces.",
    );
    Ok(())
}

pub fn setup(args: SetupArgs) -> Result<()> {
    let token = args.token.unwrap_or_else(crate::config::generate_token);
    let name = args
        .name
        .map(|value| format!(" --name {value}"))
        .unwrap_or_default();
    ui::section("Remote");
    ui::command(format!("export MOBFS_TOKEN='{token}'"));
    ui::command(format!(
        "mobfs daemon --bind 127.0.0.1:{} --allow-root '{}' --token \"$MOBFS_TOKEN\"",
        args.port,
        args.remote_root.display()
    ));
    ui::blank();
    ui::section("Local mount");
    ui::command(format!(
        "mobfs mount {}:{} --ssh-tunnel{name} --token \"$MOBFS_TOKEN\"",
        args.host,
        args.remote_root.display()
    ));
    ui::blank();
    ui::section("Mirror fallback");
    ui::command(format!(
        "mobfs mirror {}:{} --ssh-tunnel{name} --token \"$MOBFS_TOKEN\"",
        args.host,
        args.remote_root.display()
    ));
    ui::blank();
    ui::section("Validate");
    ui::command("mobfs doctor");
    ui::command("mobfs security");
    Ok(())
}

pub fn remote(args: RemoteArgs) -> Result<()> {
    match args.command {
        RemoteCommand::Start(args) => setup_remote(remote_host_to_setup(args, false, false)),
        RemoteCommand::Restart(args) => setup_remote(remote_host_to_setup(args, true, false)),
        RemoteCommand::Status(args) => setup_remote(remote_status_to_setup(args)),
    }
}

fn remote_host_to_setup(args: RemoteHostArgs, restart: bool, status: bool) -> SetupRemoteArgs {
    SetupRemoteArgs {
        ssh_target: args.ssh_target,
        root: args.root,
        port: args.port,
        token: args.token,
        dry_run: args.dry_run,
        restart,
        status,
        name: args.name,
    }
}

fn remote_status_to_setup(args: RemoteStatusArgs) -> SetupRemoteArgs {
    SetupRemoteArgs {
        ssh_target: args.ssh_target,
        root: args.root,
        port: args.port,
        token: args.token,
        dry_run: args.dry_run,
        restart: false,
        status: true,
        name: args.name,
    }
}

pub fn setup_remote(args: SetupRemoteArgs) -> Result<()> {
    let token = args.token.unwrap_or_else(crate::config::generate_token);
    let root = args.root.display().to_string();
    let remote_root = shell_path_quote(&root);
    let name = args
        .name
        .as_ref()
        .map(|value| format!(" --name {}", shell_quote(value)))
        .unwrap_or_default();
    let remote_command = if args.status {
        "if [ -s ~/.mobfsd/daemon.pid ] && kill -0 \"$(cat ~/.mobfsd/daemon.pid)\" 2>/dev/null; then echo running pid=$(cat ~/.mobfsd/daemon.pid); else echo stopped; [ -f ~/.mobfsd/daemon.log ] && tail -n 20 ~/.mobfsd/daemon.log; fi".to_string()
    } else {
        format!(
            "mkdir -p {root} ~/.mobfsd && command -v mobfs >/dev/null || {{ echo 'mobfs not found in PATH; install it on the remote first' >&2; exit 127; }}; if [ -s ~/.mobfsd/daemon.pid ] && kill -0 \"$(cat ~/.mobfsd/daemon.pid)\" 2>/dev/null; then if [ {restart} = yes ]; then kill \"$(cat ~/.mobfsd/daemon.pid)\"; sleep 1; else echo 'mobfsd already running pid='$(cat ~/.mobfsd/daemon.pid); exit 0; fi; fi; MOBFS_TOKEN={token} nohup mobfs daemon --bind 127.0.0.1:{port} --allow-root {root} --token \"$MOBFS_TOKEN\" > ~/.mobfsd/daemon.log 2>&1 < /dev/null & echo $! > ~/.mobfsd/daemon.pid; echo started pid=$(cat ~/.mobfsd/daemon.pid)",
            root = remote_root,
            token = shell_quote(&token),
            port = args.port,
            restart = if args.restart { "yes" } else { "no" },
        )
    };
    if args.dry_run {
        ui::section(if args.status {
            "Remote daemon status"
        } else {
            "Remote setup"
        });
        ui::command(format!(
            "ssh {} {}",
            args.ssh_target,
            shell_quote(&remote_command)
        ));
        if !args.status {
            ui::blank();
            ui::section("Local mount");
            ui::command(format!(
                "MOBFS_TOKEN={} mobfs mount {}:{} --ssh-tunnel{name}",
                shell_quote(&token),
                args.ssh_target,
                args.root.display()
            ));
        }
        return Ok(());
    }
    let status = Command::new("ssh")
        .arg(&args.ssh_target)
        .arg(remote_command)
        .status()?;
    if !status.success() {
        return Err(MobfsError::Remote(format!(
            "remote setup failed with {status}; ensure mobfs is installed on {} and SSH works",
            args.ssh_target
        )));
    }
    if args.status {
        return Ok(());
    }
    ui::ok("remote daemon ready");
    ui::info("token", token);
    ui::command(format!(
        "MOBFS_TOKEN=... mobfs mount {}:{} --ssh-tunnel{name}",
        args.ssh_target,
        args.root.display()
    ));
    ui::command(format!(
        "mobfs remote status {} --root {}",
        args.ssh_target,
        args.root.display()
    ));
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn shell_path_quote(value: &str) -> String {
    if value == "~" {
        return "~".to_string();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return format!("~/{}", shell_quote(rest));
    }
    shell_quote(value)
}

pub fn bench(args: BenchArgs) -> Result<()> {
    let config = AppConfig::load()?;
    if args.scale_files > 0 {
        create_scale_fixture(&config, args.scale_files, args.files_per_dir.max(1))?;
    }
    let iterations = args.iterations.max(1);
    let started = Instant::now();
    let mut entries = 0_usize;
    for _ in 0..iterations {
        entries = local::snapshot(&config)?.entries.len();
    }
    let snapshot_ms = started.elapsed().as_millis() / u128::from(iterations);
    ui::info("snapshot entries", entries.to_string());
    ui::info("snapshot avg ms", snapshot_ms.to_string());
    if config.remote.backend == crate::config::StorageBackend::Daemon {
        let bench_path = config.local.root.join(".mobfs-bench.bin");
        let size = args.mib.saturating_mul(1024 * 1024);
        let data = vec![b'm'; size as usize];
        fs::write(&bench_path, data)?;
        let mut client = StorageClient::connect(config.clone())?;
        let started = Instant::now();
        client.upload_file(".mobfs-bench.bin")?;
        let upload_secs = started.elapsed().as_secs_f64().max(0.001);
        let remote = client.snapshot()?;
        let started = Instant::now();
        if let Some(meta) = remote.entries.get(".mobfs-bench.bin") {
            client.download_file(".mobfs-bench.bin", meta)?;
        }
        let download_secs = started.elapsed().as_secs_f64().max(0.001);
        let mib = args.mib as f64;
        ui::info("upload MiB/s", format!("{:.2}", mib / upload_secs));
        ui::info("download MiB/s", format!("{:.2}", mib / download_secs));
        let _ = fs::remove_file(bench_path);
        if let Some(meta) = remote.entries.get(".mobfs-bench.bin") {
            let _ = client.remove(".mobfs-bench.bin", meta);
        }
    }
    Ok(())
}

fn create_scale_fixture(config: &AppConfig, files: u32, files_per_dir: u32) -> Result<()> {
    let root = config.local.root.join(".mobfs-scale-fixture");
    if root.exists() {
        fs::remove_dir_all(&root)?;
    }
    fs::create_dir_all(&root)?;
    let started = Instant::now();
    for index in 0..files {
        let dir = root.join(format!("d{:05}", index / files_per_dir));
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join(format!("f{:05}.txt", index)),
            format!("mobfs scale fixture {index}\n"),
        )?;
    }
    ui::info("scale fixture files", files.to_string());
    ui::info(
        "scale fixture create ms",
        started.elapsed().as_millis().to_string(),
    );
    Ok(())
}

pub fn doctor() -> Result<()> {
    let (config, mirror_workspace) = load_command_config()?;
    ui::info("local", config.local.root.display().to_string());
    ui::info("mode", if mirror_workspace { "mirror" } else { "mount" });
    ui::info(
        "remote",
        format!("{}:{}", config.remote.host, config.remote.path),
    );
    ui::info(
        "backend",
        crate::storage::backend_label(&config.remote.backend),
    );
    let has_token = config.remote.token.is_some() || std::env::var("MOBFS_TOKEN").is_ok();
    if config.remote.backend == crate::config::StorageBackend::Daemon {
        if has_token {
            ui::ok("daemon token available");
        } else {
            ui::warn("daemon token missing; set MOBFS_TOKEN or configure a token");
        }
        ui::info("remote compute", "available");
        if config.remote.ssh_tunnel {
            check_tool("ssh");
        } else {
            ui::warn(
                "daemon is configured without --ssh-tunnel; expose mobfsd only on trusted networks",
            );
        }
    } else {
        ui::warn("remote compute unavailable for provider-backed workspaces");
    }
    check_tool("git");
    #[cfg(target_os = "macos")]
    if !std::path::Path::new("/Library/Filesystems/macfuse.fs").exists() {
        ui::warn("macFUSE not found; mountfs will be unavailable until macFUSE is installed");
    }
    let spinner = ui::spinner("checking storage");
    let mut client = StorageClient::connect(config.clone())?;
    spinner.set_message("scanning remote");
    let _ = client.snapshot()?;
    spinner.finish_and_clear();
    let backends = crate::storage::supported_backends()
        .iter()
        .map(crate::storage::backend_label)
        .collect::<Vec<_>>()
        .join(", ");
    ui::info("storage", backends);
    ui::ok("workspace ready");
    Ok(())
}

pub fn watch(args: WatchArgs) -> Result<()> {
    watch_push_loop(args.debounce_ms, args.delete)
}

pub fn serve(args: ServeArgs) -> Result<()> {
    let config = AppConfig::load()?;
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = RecommendedWatcher::new(tx, NotifyConfig::default())?;
    watcher.watch(&config.local.root, RecursiveMode::Recursive)?;
    ui::info("serving", config.local.root.display().to_string());
    let debounce = Duration::from_millis(args.debounce_ms);
    let remote_interval = Duration::from_secs(args.remote_interval.max(1));
    let mut last_change: Option<Instant> = None;
    let mut last_remote_scan = Instant::now() - remote_interval;
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Ok(event)) => {
                if event
                    .paths
                    .iter()
                    .any(|path| local::should_ignore_path(&config, path))
                {
                    continue;
                }
                last_change = Some(Instant::now());
            }
            Ok(Err(error)) => ui::warn(format!("watch error: {error}")),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(MobfsError::Remote("watcher disconnected".to_string()));
            }
        }
        if last_change
            .map(|instant| instant.elapsed() >= debounce)
            .unwrap_or(false)
            || last_remote_scan.elapsed() >= remote_interval
        {
            resilient_sync_once(&config, args.delete)?;
            last_change = None;
            last_remote_scan = Instant::now();
        }
    }
}

pub fn open() -> Result<()> {
    let config = AppConfig::load()?;
    open_path(&config.local.root)
}

fn check_tool(name: &str) {
    let available = Command::new(name).arg("--version").output().is_ok();
    if available {
        ui::ok(format!("{name} available"));
    } else {
        ui::warn(format!("{name} not found in PATH"));
    }
}

fn watch_push_loop(debounce_ms: u64, delete: bool) -> Result<()> {
    let config = AppConfig::load()?;
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = RecommendedWatcher::new(tx, NotifyConfig::default())?;
    watcher.watch(&config.local.root, RecursiveMode::Recursive)?;
    ui::info("watching", config.local.root.display().to_string());
    let debounce = Duration::from_millis(debounce_ms);
    let mut last_change: Option<Instant> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Ok(event)) => {
                if event
                    .paths
                    .iter()
                    .any(|path| local::should_ignore_path(&config, path))
                {
                    continue;
                }
                last_change = Some(Instant::now());
            }
            Ok(Err(error)) => ui::warn(format!("watch error: {error}")),
            Err(RecvTimeoutError::Timeout) => {
                if last_change
                    .map(|instant| instant.elapsed() >= debounce)
                    .unwrap_or(false)
                {
                    let local_snapshot = local::snapshot(&config)?;
                    let mut client = StorageClient::connect(config.clone())?;
                    push_plan(&mut client, &local_snapshot, delete, false)?;
                    let remote = client.snapshot()?;
                    local::save_snapshot(&config, &remote)?;
                    last_change = None;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(MobfsError::Remote("watcher disconnected".to_string()));
            }
        }
    }
}

fn resilient_sync_once(config: &AppConfig, delete: bool) -> Result<()> {
    let base = local::load_snapshot(config)?;
    let local_snapshot = local::snapshot(config)?;
    let mut client = StorageClient::connect(config.clone())?;
    let remote = client.snapshot()?;
    let plan = sync_items(&base, &local_snapshot, &remote, delete);
    if plan.is_empty() {
        return Ok(());
    }
    if plan
        .iter()
        .any(|item| matches!(item, PlanItem::Conflict(_)))
    {
        for item in &plan {
            if let PlanItem::Conflict(path) = item {
                write_conflict_artifacts(config, &mut client, &remote, path)?;
                ui::change("conflict", path);
            }
        }
        return Err(MobfsError::Remote(
            "resilient sync stopped because both sides changed the same path; local and remote conflict copies were written next to conflicted files. Resolve by choosing or merging the .mobfs-conflict-local and .mobfs-conflict-remote files, then run `mobfs sync` again".to_string(),
        ));
    }
    apply_plan(&mut client, config, &remote, &plan)?;
    let remote = client.snapshot()?;
    local::save_snapshot(config, &remote)?;
    Ok(())
}

fn write_conflict_artifacts(
    config: &AppConfig,
    client: &mut StorageClient,
    remote: &Snapshot,
    rel: &str,
) -> Result<()> {
    let path = config.local.root.join(rel);
    if !path.is_file() {
        return Ok(());
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| MobfsError::InvalidPath(path.display().to_string()))?;
    let local_conflict = path.with_file_name(format!("{name}.mobfs-conflict-local"));
    let remote_conflict = path.with_file_name(format!("{name}.mobfs-conflict-remote"));
    let swap = path.with_file_name(format!(".{name}.mobfs-conflict-swap"));
    fs::copy(&path, &local_conflict)?;
    if let Some(meta) = remote.entries.get(rel)
        && meta.kind == EntryKind::File
    {
        fs::rename(&path, &swap)?;
        let result = client.download_file(rel, meta);
        if result.is_ok() {
            fs::rename(&path, &remote_conflict)?;
        }
        fs::rename(&swap, &path)?;
        result?;
    }
    Ok(())
}

fn print_plan(plan: &[PlanItem]) {
    if plan.is_empty() {
        ui::ok("no changes planned");
        return;
    }
    for item in plan {
        match item {
            PlanItem::Put(rel) => ui::change("would-put", rel),
            PlanItem::Get(rel) => ui::change("would-get", rel),
            PlanItem::DeleteLocal(rel) => ui::change("would-delete-local", rel),
            PlanItem::DeleteRemote(rel) => ui::change("would-delete-remote", rel),
            PlanItem::Conflict(rel) => ui::change("would-conflict", rel),
        }
    }
    ui::summary("planned changes", plan.len());
}

fn apply_plan(
    client: &mut StorageClient,
    config: &AppConfig,
    remote: &Snapshot,
    plan: &[PlanItem],
) -> Result<()> {
    for item in plan {
        match item {
            PlanItem::Put(rel) => {
                ui::change("put", rel);
                client.upload_file(rel)?;
            }
            PlanItem::Get(rel) => {
                ui::change("get", rel);
                if let Some(meta) = remote.entries.get(rel) {
                    client.download_file(rel, meta)?;
                }
            }
            PlanItem::DeleteLocal(rel) => {
                ui::change("delete-local", rel);
                remove_local(config, rel)?;
            }
            PlanItem::DeleteRemote(rel) => {
                if let Some(meta) = remote.entries.get(rel) {
                    ui::change("delete-remote", rel);
                    client.remove(rel, meta)?;
                }
            }
            PlanItem::Conflict(rel) => ui::change("conflict", rel),
        }
    }
    Ok(())
}

#[cfg_attr(not(feature = "fuse"), allow(dead_code))]
fn default_no_local_code_mountpoint(
    name: Option<&str>,
    host: &str,
    remote_path: &str,
) -> Result<std::path::PathBuf> {
    let workspace = default_workspace_name(name, host, remote_path);
    if cfg!(target_os = "macos") {
        Ok(std::path::PathBuf::from("/Volumes").join(workspace))
    } else {
        let home = dirs::home_dir()
            .ok_or_else(|| MobfsError::Config("home directory not found".to_string()))?;
        Ok(home.join("MobFSMounts").join(workspace))
    }
}

fn default_mirror_root(
    name: Option<&str>,
    host: &str,
    remote_path: &str,
) -> Result<std::path::PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| MobfsError::Config("home directory not found".to_string()))?;
    Ok(home
        .join("MobFS")
        .join(default_workspace_name(name, host, remote_path)))
}

fn default_workspace_name(name: Option<&str>, host: &str, remote_path: &str) -> String {
    let fallback = remote_path
        .trim_matches('/')
        .replace('/', "-")
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_' || *ch == '.')
        .collect::<String>();
    name.map(str::to_string)
        .unwrap_or_else(|| format!("{host}-{}", fallback))
}

fn open_path(path: &std::path::Path) -> Result<()> {
    let status = if cfg!(target_os = "macos") {
        Command::new("open").arg(path).status()?
    } else if cfg!(target_os = "windows") {
        Command::new("explorer").arg(path).status()?
    } else {
        Command::new("xdg-open").arg(path).status()?
    };
    if status.success() {
        Ok(())
    } else {
        Err(MobfsError::Remote(format!(
            "failed to open {}",
            path.display()
        )))
    }
}

fn remove_local(config: &AppConfig, rel: &str) -> Result<()> {
    let path = config.local.root.join(rel);
    if path.is_dir() {
        fs::remove_dir_all(path)?;
    } else if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn push_plan(
    client: &mut StorageClient,
    local_snapshot: &Snapshot,
    delete: bool,
    dry_run: bool,
) -> Result<usize> {
    let remote = client.snapshot()?;
    ensure_remote_dirs(client, local_snapshot)?;
    let plan = push_items(local_snapshot, &remote, delete);
    let count = plan.len();
    if dry_run {
        print_plan(&plan);
        return Ok(count);
    }
    for item in plan {
        match item {
            PlanItem::Put(rel) => {
                ui::change("put", &rel);
                client.upload_file(&rel)?;
            }
            PlanItem::DeleteRemote(rel) => {
                if let Some(meta) = remote.entries.get(&rel) {
                    ui::change("delete", &rel);
                    client.remove(&rel, meta)?;
                }
            }
            PlanItem::Get(_) | PlanItem::DeleteLocal(_) | PlanItem::Conflict(_) => {}
        }
    }
    Ok(count)
}

fn ensure_local_dirs(config: &AppConfig, remote: &Snapshot) -> Result<()> {
    for (rel, meta) in &remote.entries {
        if meta.kind == EntryKind::Dir {
            fs::create_dir_all(config.local.root.join(rel))?;
        }
    }
    Ok(())
}

fn ensure_remote_dirs(client: &mut StorageClient, local_snapshot: &Snapshot) -> Result<()> {
    for (rel, meta) in &local_snapshot.entries {
        if meta.kind == EntryKind::Dir {
            client.mkdir_p(rel)?;
        }
    }
    Ok(())
}
