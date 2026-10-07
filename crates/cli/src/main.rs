//! `ferro` CLI entry point. Subcommands (`run`, `chat`, `sessions`, `doctor`)
//! land in stage 0.2; for now this only reports the version.

fn main() {
    println!("ferro {}", ferro_core::VERSION);
}
