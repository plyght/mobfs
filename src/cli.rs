use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "mobfs")]
#[command(version)]
#[command(about = "mobfs - a resilient mosh-like filesystem workspace", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    #[command(about = "Create a mobfs workspace")]
    Init(InitArgs),
    #[command(about = "Mount if needed, then run the resilient sync loop  [alias: up]")]
    #[command(visible_alias = "up")]
    Start(StartArgs),
    #[command(about = "Start the remote daemon over SSH, then mount the workspace  [alias: c]")]
    #[command(visible_alias = "c")]
    Connect(ConnectArgs),
    #[command(about = "Mount a no-local-code on-demand read-write filesystem  [alias: add]")]
    #[command(visible_alias = "add")]
    Mount(MountArgs),
    #[command(about = "Create/open a durable local mirror workspace backed by mobfsd")]
    Mirror(MountArgs),
    #[command(about = "Mount a no-local-code on-demand read-write FUSE filesystem")]
    Mountfs(MountFsArgs),
    #[command(about = "Pull remote files into the local workspace  [alias: get]")]
    #[command(visible_alias = "get")]
    Pull(PullArgs),
    #[command(about = "Push local workspace changes to the remote  [alias: put]")]
    #[command(visible_alias = "put")]
    Push(PushArgs),
    #[command(about = "Safely reconcile local and remote changes  [alias: s]")]
    #[command(visible_alias = "s")]
    Sync(SyncArgs),
    #[command(about = "Show local/remote drift  [alias: st]")]
    #[command(visible_alias = "st")]
    Status,
    #[command(about = "Run a command on the remote in the workspace root  [alias: r]")]
    #[command(visible_alias = "r")]
    Run(RunArgs),
    #[command(about = "Build somewhere fast, while the remote keeps source and artifacts")]
    Build(BuildArgs),
    #[command(about = "Run git on the remote after syncing local edits  [alias: g]")]
    #[command(visible_alias = "g")]
    Git(GitArgs),
    #[command(about = "Watch the local workspace and push changes")]
    Watch(WatchArgs),
    #[command(about = "Run resilient bidirectional sync loop")]
    Serve(ServeArgs),
    #[command(about = "Open the workspace in Finder  [alias: o]")]
    #[command(visible_alias = "o")]
    Open,
    #[command(about = "Unmount a mobfs FUSE mount and clean up the mountpoint  [alias: rm]")]
    #[command(visible_alias = "rm")]
    #[command(alias = "remove")]
    Unmount(UnmountArgs),
    #[command(about = "Print macOS/FUSE dogfooding checks for a mount")]
    MountDoctor(MountDoctorArgs),
    #[command(about = "Print security hardening guidance")]
    Security,
    #[command(about = "Run mobfs remote daemon")]
    Daemon(DaemonArgs),
    #[command(about = "Generate a strong mobfs daemon token")]
    Token,
    #[command(about = "Print remote daemon setup commands")]
    Setup(SetupArgs),
    #[command(about = "Manage a remote mobfsd over SSH  [alias: rmt]")]
    #[command(visible_alias = "rmt")]
    Remote(RemoteArgs),
    #[command(about = "Install/start mobfsd on a remote host over SSH  [alias: sr]")]
    #[command(hide = true)]
    #[command(alias = "sr")]
    SetupRemote(SetupRemoteArgs),
    #[command(about = "Check workspace and daemon connectivity  [alias: dx]")]
    #[command(visible_alias = "dx")]
    Doctor,
    #[command(about = "Benchmark snapshot and daemon transfer performance")]
    Bench(BenchArgs),
}

#[derive(Args)]
pub struct InitArgs {
    #[arg(help = "Remote root like host:/absolute/path")]
    pub remote: String,
    #[arg(long, help = "Local workspace root")]
    pub local: Option<PathBuf>,
    #[arg(long, default_value_t = 7727, help = "mobfsd port")]
    pub port: u16,
    #[arg(long, env = "MOBFS_TOKEN", help = "Shared mobfsd token")]
    pub token: Option<String>,
    #[arg(long, help = "Connect to mobfsd through ssh -L using the remote host")]
    pub ssh_tunnel: bool,
}

#[derive(Args)]
pub struct StartArgs {
    #[arg(help = "Remote root like host:/absolute/path; omitted inside an existing workspace")]
    pub remote: Option<String>,
    #[arg(long, help = "Workspace name under ~/MobFS")]
    pub name: Option<String>,
    #[arg(long, help = "Local visible workspace root")]
    pub local: Option<PathBuf>,
    #[arg(long, default_value_t = 7727, help = "mobfsd port")]
    pub port: u16,
    #[arg(long, env = "MOBFS_TOKEN", help = "Shared mobfsd token")]
    pub token: Option<String>,
    #[arg(long, help = "Connect to mobfsd through ssh -L using the remote host")]
    pub ssh_tunnel: bool,
    #[arg(
        long,
        default_value_t = 500,
        help = "Local change debounce in milliseconds"
    )]
    pub debounce_ms: u64,
    #[arg(long, default_value_t = 2, help = "Remote scan interval in seconds")]
    pub remote_interval: u64,
    #[arg(long, help = "Propagate deletions on the side that did not change")]
    pub delete: bool,
    #[arg(long, help = "Do not open Finder after mounting")]
    pub no_open: bool,
}

#[derive(Args)]
pub struct ConnectArgs {
    #[arg(help = "Remote root like user@host:/absolute/path")]
    pub remote: String,
    #[arg(
        long,
        help = "Workspace name under /Volumes on macOS or ~/MobFSMounts elsewhere"
    )]
    pub name: Option<String>,
    #[arg(long, help = "Local mountpoint")]
    pub local: Option<PathBuf>,
    #[arg(long, default_value_t = 7727, help = "mobfsd port")]
    pub port: u16,
    #[arg(long, env = "MOBFS_TOKEN", help = "Shared mobfsd token")]
    pub token: Option<String>,
    #[arg(
        long,
        default_value_t = 1,
        help = "Kernel attribute/entry cache TTL in seconds"
    )]
    pub cache_ttl_secs: u64,
    #[arg(long, help = "Do not open Finder after mounting")]
    pub no_open: bool,
    #[arg(long, help = "Stop any running ~/.mobfsd daemon before starting")]
    pub restart: bool,
}

#[derive(Args)]
pub struct MountArgs {
    #[arg(help = "Remote root like host:/absolute/path")]
    pub remote: String,
    #[arg(
        long,
        help = "Workspace name under /Volumes on macOS or ~/MobFSMounts elsewhere"
    )]
    pub name: Option<String>,
    #[arg(long, help = "Local mountpoint or mirror root")]
    pub local: Option<PathBuf>,
    #[arg(long, default_value_t = 7727, help = "mobfsd port")]
    pub port: u16,
    #[arg(long, env = "MOBFS_TOKEN", help = "Shared mobfsd token")]
    pub token: Option<String>,
    #[arg(long, help = "Connect to mobfsd through ssh -L using the remote host")]
    pub ssh_tunnel: bool,
    #[arg(
        long,
        default_value_t = 1,
        help = "Kernel attribute/entry cache TTL in seconds"
    )]
    pub cache_ttl_secs: u64,
    #[arg(long, help = "Do not open Finder after mounting")]
    pub no_open: bool,
}

#[derive(Args)]
pub struct MountFsArgs {
    #[arg(help = "Either <mountpoint> inside a workspace or <remote> <mountpoint>", num_args = 1..=2)]
    pub paths: Vec<String>,
    #[arg(long, default_value_t = 7727, help = "mobfsd port")]
    pub port: u16,
    #[arg(long, env = "MOBFS_TOKEN", help = "Shared mobfsd token")]
    pub token: Option<String>,
    #[arg(long, help = "Connect to mobfsd through ssh -L using the remote host")]
    pub ssh_tunnel: bool,
}

#[derive(Args)]
pub struct PullArgs {
    #[arg(long, help = "Delete local paths missing from the remote")]
    pub delete: bool,
    #[arg(long, help = "Show planned changes without applying them")]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct PushArgs {
    #[arg(long, help = "Delete remote paths missing locally")]
    pub delete: bool,
    #[arg(long, help = "Show planned changes without applying them")]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct SyncArgs {
    #[arg(long, help = "Propagate deletions on the side that did not change")]
    pub delete: bool,
    #[arg(long, help = "Show planned changes without applying them")]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct RunArgs {
    #[arg(long, help = "Run without syncing local edits first")]
    pub no_sync: bool,
    #[arg(
        required = true,
        trailing_var_arg = true,
        help = "Command and arguments to run remotely"
    )]
    pub command: Vec<String>,
}

#[derive(Args)]
pub struct BuildArgs {
    #[arg(
        long = "on",
        conflicts_with = "here",
        help = "SSH builder target like user@host"
    )]
    pub builder: Option<String>,
    #[arg(
        long,
        alias = "local",
        conflicts_with = "builder",
        help = "Build on this machine in a staged local workspace"
    )]
    pub here: bool,
    #[arg(
        long,
        help = "Use an ephemeral mirror on the SSH builder instead of a FUSE mount"
    )]
    pub mirror: bool,
    #[arg(long, help = "Run without syncing local mirror edits first")]
    pub no_sync: bool,
    #[arg(long, help = "Path to an artifact inside the build workspace")]
    pub artifact: Option<PathBuf>,
    #[arg(long, help = "Local destination for --artifact when using --on")]
    pub out: Option<PathBuf>,
    #[arg(long, help = "Remote destination for --artifact when using --here")]
    pub remote_artifact: Option<PathBuf>,
    #[arg(
        long,
        help = "Persistent local staging directory for --here; defaults to a MobFS cache directory"
    )]
    pub workdir: Option<PathBuf>,
    #[arg(
        long,
        help = "Keep the generated --here staging directory after the build"
    )]
    pub keep: bool,
    #[arg(
        required = true,
        trailing_var_arg = true,
        help = "Build command and arguments"
    )]
    pub command: Vec<String>,
}

#[derive(Args)]
pub struct GitArgs {
    #[arg(long, help = "Run without syncing local edits first")]
    pub no_sync: bool,
    #[arg(
        required = true,
        trailing_var_arg = true,
        help = "Git arguments to run remotely"
    )]
    pub args: Vec<String>,
}

#[derive(Args)]
pub struct WatchArgs {
    #[arg(long, default_value_t = 500, help = "Push debounce in milliseconds")]
    pub debounce_ms: u64,
    #[arg(long, help = "Delete remote paths missing locally")]
    pub delete: bool,
}

#[derive(Args)]
pub struct BenchArgs {
    #[arg(long, default_value_t = 3, help = "Number of benchmark iterations")]
    pub iterations: u32,
    #[arg(long, default_value_t = 8, help = "Transfer test size in MiB")]
    pub mib: u64,
    #[arg(
        long,
        default_value_t = 0,
        help = "Create and scan a synthetic many-file fixture before transfer benchmarking"
    )]
    pub scale_files: u32,
    #[arg(
        long,
        default_value_t = 1000,
        help = "Files per directory for synthetic scale fixtures"
    )]
    pub files_per_dir: u32,
}

#[derive(Args)]
pub struct UnmountArgs {
    #[arg(help = "Mountpoint to unmount; omitted inside a configured workspace")]
    pub mountpoint: Option<PathBuf>,
}

#[derive(Args)]
pub struct MountDoctorArgs {
    #[arg(help = "Mountpoint to inspect")]
    pub mountpoint: PathBuf,
}

#[derive(Args)]
pub struct DaemonArgs {
    #[arg(long, default_value = "127.0.0.1:7727", help = "Address to listen on")]
    pub bind: String,
    #[arg(long, env = "MOBFS_TOKEN", help = "Shared client token")]
    pub token: Option<String>,
    #[arg(
        long = "allow-root",
        help = "Allowed canonical workspace root or parent; repeatable"
    )]
    pub allow_roots: Vec<PathBuf>,
    #[arg(
        long,
        help = "Allow clients to request any root the daemon process can access"
    )]
    pub allow_any_root: bool,
}

#[derive(Args)]
pub struct SetupArgs {
    #[arg(help = "Remote workspace root to allow")]
    pub remote_root: PathBuf,
    #[arg(
        long,
        default_value = "host",
        help = "SSH host used in the local start command"
    )]
    pub host: String,
    #[arg(long, help = "Workspace name under ~/MobFS")]
    pub name: Option<String>,
    #[arg(long, default_value_t = 7727, help = "mobfsd port")]
    pub port: u16,
    #[arg(long, env = "MOBFS_TOKEN", help = "Shared mobfsd token")]
    pub token: Option<String>,
}

#[derive(Args)]
pub struct SetupRemoteArgs {
    #[arg(help = "SSH target like user@host")]
    pub ssh_target: String,
    #[arg(long, help = "Remote workspace root to create and allow")]
    pub root: PathBuf,
    #[arg(long, default_value_t = 7727, help = "mobfsd port")]
    pub port: u16,
    #[arg(long, env = "MOBFS_TOKEN", help = "Shared mobfsd token")]
    pub token: Option<String>,
    #[arg(long, help = "Print commands without running SSH")]
    pub dry_run: bool,
    #[arg(long, help = "Stop any running ~/.mobfsd daemon before starting")]
    pub restart: bool,
    #[arg(long, help = "Only check the remote ~/.mobfsd daemon status")]
    pub status: bool,
    #[arg(long, help = "Workspace name to show in the suggested mount command")]
    pub name: Option<String>,
}

#[derive(Args)]
pub struct RemoteArgs {
    #[command(subcommand)]
    pub command: RemoteCommand,
}

#[derive(Subcommand)]
pub enum RemoteCommand {
    #[command(about = "Start mobfsd on a host over SSH  [alias: up]")]
    #[command(visible_alias = "up")]
    Start(RemoteHostArgs),
    #[command(about = "Restart mobfsd on a host over SSH")]
    Restart(RemoteHostArgs),
    #[command(about = "Show remote mobfsd status  [alias: st]")]
    #[command(visible_alias = "st")]
    Status(RemoteStatusArgs),
}

#[derive(Args)]
pub struct RemoteHostArgs {
    #[arg(help = "SSH target like user@host")]
    pub ssh_target: String,
    #[arg(long, help = "Remote workspace root to create and allow")]
    pub root: PathBuf,
    #[arg(long, default_value_t = 7727, help = "mobfsd port")]
    pub port: u16,
    #[arg(long, env = "MOBFS_TOKEN", help = "Shared mobfsd token")]
    pub token: Option<String>,
    #[arg(long, help = "Print commands without running SSH")]
    pub dry_run: bool,
    #[arg(long, help = "Workspace name to show in the suggested mount command")]
    pub name: Option<String>,
}

#[derive(Args)]
pub struct RemoteStatusArgs {
    #[arg(help = "SSH target like user@host")]
    pub ssh_target: String,
    #[arg(
        long,
        default_value = "~",
        help = "Remote workspace root used for command context"
    )]
    pub root: PathBuf,
    #[arg(long, default_value_t = 7727, help = "mobfsd port")]
    pub port: u16,
    #[arg(long, env = "MOBFS_TOKEN", help = "Shared mobfsd token")]
    pub token: Option<String>,
    #[arg(long, help = "Print commands without running SSH")]
    pub dry_run: bool,
    #[arg(long, help = "Workspace name to show in the suggested mount command")]
    pub name: Option<String>,
}

#[derive(Args)]
pub struct ServeArgs {
    #[arg(
        long,
        default_value_t = 500,
        help = "Local change debounce in milliseconds"
    )]
    pub debounce_ms: u64,
    #[arg(long, default_value_t = 2, help = "Remote scan interval in seconds")]
    pub remote_interval: u64,
    #[arg(long, help = "Propagate deletions on the side that did not change")]
    pub delete: bool,
}
