//! `crewctl`: ask a running `crewd` what it is doing, over the ops API it publishes (#45).
//!
//! A separate binary so that "the client cannot touch the daemon's store" is a fact about the
//! dependency graph rather than an early `return` someone has to remember to keep: this crate
//! links `libcrew` and nothing that can open SQLite, a worktree or a tracker credential. An
//! operator asking what is running must not be able to disturb it, and a second process on the
//! daemon's database would be exactly that. CI checks the graph with `cargo tree`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use libcrew::client::{Client, endpoint};
use libcrew::render;

#[derive(Parser, Debug)]
#[command(name = "crewctl", version = libcrew::build(), about = "Query a running crewd over its ops API")]
struct Args {
    /// The daemon's config, read only for `[api] bind` and leniently: a config the daemon would
    /// refuse to start with still says where to look.
    #[arg(short, long, default_value = "crew.toml", global = true)]
    config: PathBuf,

    /// Query this address instead of the one the config or the default names.
    #[arg(long, value_name = "ADDR", global = true)]
    api: Option<String>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Print what a running daemon is doing, read from its ops API.
    Status(StatusArgs),
}

#[derive(clap::Args, Debug)]
struct StatusArgs {
    /// One issue in full, by dispatch id or tracker identifier. Omit for every issue.
    #[arg(value_name = "ISSUE")]
    issue: Option<String>,

    /// Print the API's JSON verbatim. For a script; the rendered form is for a person.
    #[arg(long)]
    json: bool,
}

fn main() {
    let args = Args::parse();
    let code = match &args.command {
        Cmd::Status(status) => run_status(&args, status),
    };
    std::process::exit(code);
}

/// Returns a process exit code rather than a `Result`, because the two failures an operator
/// cares about are not the same event. "No daemon is listening" is the answer to a question a
/// script may legitimately be asking; rendering it through an error chain would bury a message
/// written to be read under one written to be debugged.
///
/// `1` for anything that stopped this from printing a snapshot. The message on stderr is what
/// distinguishes the cases; see `StatusError`.
fn run_status(args: &Args, status: &StatusArgs) -> i32 {
    let client = Client::new(endpoint(args.api.as_deref(), &args.config));
    let addr = client.endpoint().addr.clone();

    let rendered = match (&status.issue, status.json) {
        (None, false) => client.snapshot().map(|s| render::snapshot(&s, &addr)),
        (Some(key), false) => client.issue(key).map(|r| render::issue(&r)),
        (None, true) => client.raw_snapshot(),
        (Some(key), true) => client.raw_issue(key),
    };

    match rendered {
        Ok(text) => {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}
