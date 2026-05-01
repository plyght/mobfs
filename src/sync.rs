use crate::cli::{
    BenchArgs, GitArgs, InitArgs, MountArgs, MountFsArgs, PullArgs, PushArgs, RunArgs, ServeArgs,
    StartArgs, SyncArgs, WatchArgs,
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
use std::fs;
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

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
        mount(MountArgs {
            remote,
            name: args.name,
            local: args.local,
            port: args.port,
            token: args.token,
            ssh_tunnel: args.ssh_tunnel,
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
        let config = match args.remote {
            Some(remote) => crate::mountfs::config_from_remote(
                remote,
                &args.mountpoint,
                args.port,
                args.token,
                args.ssh_tunnel,
            )?,
            None => AppConfig::load()?,
        };
        crate::mountfs::mount(config, args.mountpoint)
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
    let target = parse_remote(&args.remote)?;
    let root = match args.local {
        Some(path) => path,
        None => default_mount_root(args.name.as_deref(), &target.host, &target.path)?,
    };
    let config = new_config(target, root.clone(), args.port, args.token, args.ssh_tunnel);
    write_config(&config)?;
    ui::added("mounted", root.display().to_string());
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
            user: String::new(),
            path: target.path,
            port,
            identity: None,
            ssh_tunnel,
            token: token.or_else(|| std::env::var("MOBFS_TOKEN").ok()),
        },
        local: LocalConfig { root },
        sync: SyncConfig {
            ignore: vec![
                STATE_DIR.to_string(),
                "target".to_string(),
                "node_modules".to_string(),
                ".mobfs.toml".to_string(),
            ],
            connect_retries: DEFAULT_CONNECT_RETRIES,
            operation_retries: DEFAULT_OP_RETRIES,
        },
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
            "sync stopped because both sides changed the same path; local and remote conflict copies were written next to conflicted files".to_string(),
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
    let config = AppConfig::load()?;
    let spinner = ui::spinner("scanning local and remote");
    let local_snapshot = local::snapshot(&config)?;
    let mut client = StorageClient::connect(config)?;
    let remote = client.snapshot()?;
    spinner.finish_and_clear();
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

pub fn git(args: GitArgs) -> Result<()> {
    let mut command = Vec::with_capacity(args.args.len() + 1);
    command.push("git".to_string());
    command.extend(args.args);
    run_remote(command, !args.no_sync)
}

fn run_remote(command: Vec<String>, sync_first: bool) -> Result<()> {
    if sync_first {
        sync(SyncArgs {
            delete: false,
            dry_run: false,
        })?;
    }
    let config = AppConfig::load()?;
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

pub fn bench(args: BenchArgs) -> Result<()> {
    let config = AppConfig::load()?;
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

pub fn doctor() -> Result<()> {
    let config = AppConfig::load()?;
    ui::info("local", config.local.root.display().to_string());
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
    } else {
        ui::warn("remote compute unavailable for provider-backed workspaces");
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
        return Ok(());
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

fn default_mount_root(
    name: Option<&str>,
    host: &str,
    remote_path: &str,
) -> Result<std::path::PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| MobfsError::Config("home directory not found".to_string()))?;
    let fallback = remote_path
        .trim_matches('/')
        .replace('/', "-")
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_' || *ch == '.')
        .collect::<String>();
    let workspace = name
        .map(str::to_string)
        .unwrap_or_else(|| format!("{host}-{}", fallback));
    Ok(home.join("MobFS").join(workspace))
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
