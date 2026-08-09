//! GreenTyper Target Machine acceptance entry point.

mod harness;

fn main() -> std::process::ExitCode {
    harness::run(std::env::args().skip(1))
}
