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
    #[command(about = "Mount a no-local-code on-demand read-write filesystem")]
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
    #[command(about = "Run mobfs remote daemon")]
    Daemon(DaemonArgs),
    #[command(about = "Generate a strong mobfs daemon token")]
    Token,
    #[command(about = "Print remote daemon setup commands")]
    Setup(SetupArgs),
    #[command(about = "Check workspace and daemon connectivity")]
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
