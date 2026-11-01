#![deny(unsafe_code)]

mod cli;
mod demo;

fn main() -> std::process::ExitCode {
    cli::run(std::env::args_os().skip(1))
}
