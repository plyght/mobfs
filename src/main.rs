mod cli;
mod config;
mod crypto;
mod daemon;
mod error;
mod journal;
mod local;
#[cfg(feature = "fuse")]
mod mountfs;
mod protocol;
mod remote;
mod snapshot;
mod storage;
mod sync;
mod ui;

use clap::Parser;
use cli::{Cli, Command};
use error::Result;

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init(args) => sync::init(args),
        Command::Start(args) => sync::start(args),
        Command::Mount(args) => sync::mount(args),
        Command::Mirror(args) => sync::mirror(args),
        Command::Mountfs(args) => sync::mountfs(args),
        Command::Pull(args) => sync::pull(args),
        Command::Push(args) => sync::push(args),
        Command::Sync(args) => sync::sync(args),
        Command::Status => sync::status(),
        Command::Run(args) => sync::run(args),
        Command::Git(args) => sync::git(args),
        Command::Watch(args) => sync::watch(args),
        Command::Serve(args) => sync::serve(args),
        Command::Open => sync::open(),
        Command::Unmount(args) => sync::unmount(args),
        Command::MountDoctor(args) => sync::mount_doctor(args),
        Command::Security => sync::security(),
        Command::Daemon(args) => {
            let token = args.token.ok_or_else(|| {
                error::MobfsError::Config(
                    "daemon token missing; pass --token or set MOBFS_TOKEN".to_string(),
                )
            })?;
            daemon::serve(&args.bind, &token, args.allow_roots, args.allow_any_root)
        }
        Command::Token => sync::token(),
        Command::Setup(args) => sync::setup(args),
        Command::SetupRemote(args) => sync::setup_remote(args),
        Command::Doctor => sync::doctor(),
        Command::Bench(args) => sync::bench(args),
    }
}
