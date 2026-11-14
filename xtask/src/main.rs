//! `cargo xtask layers` fails when a crate depends on a crate in the same or
//! a higher layer. Every workspace crate must be listed in `LAYERS`, so a new
//! crate cannot skip the rule by accident.
//!
//! `cargo xtask parity [THEIRS OURS]` compares a request Claude Code sent with
//! one this engine sent.

mod layers;
mod parity;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("layers") => layers::check(),
        Some("parity") => parity::run(&args[1..]),
        _ => {
            eprintln!("usage: cargo xtask layers | cargo xtask parity [THEIRS OURS]");
            std::process::exit(2);
        }
    }
}
