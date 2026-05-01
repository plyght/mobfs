mod cli;
mod config;
mod crypto;
mod daemon;
mod error;
mod local;
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
        Command::Pull(args) => sync::pull(args),
        Command::Push(args) => sync::push(args),
        Command::Sync(args) => sync::sync(args),
        Command::Status => sync::status(),
        Command::Run(args) => sync::run(args),
        Command::Git(args) => sync::git(args),
        Command::Watch(args) => sync::watch(args),
        Command::Serve(args) => sync::serve(args),
        Command::Open => sync::open(),
        Command::Daemon(args) => daemon::serve(
            &args.bind,
            &args.token,
            args.allow_roots,
            args.allow_any_root,
        ),
        Command::Doctor => sync::doctor(),
    }
}
