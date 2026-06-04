#[macro_use]
mod macros;

extern crate hiro_system_kit;

mod agent_io;
mod cli;
// mod manifest;
mod http;
mod no_dna;
mod runbook;
mod scaffold;
mod tui;
mod types;

fn main() {
    cli::main();
}
