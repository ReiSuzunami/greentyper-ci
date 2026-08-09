//! GreenTyper Target Machine acceptance entry point.

mod harness;

// Cargo's all-features verification selects one allocator deterministically. Allocator
// evidence is exposed only when exactly one candidate feature is enabled.
#[cfg(feature = "bench-allocator-snmalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: snmalloc_rs::SnMalloc = snmalloc_rs::SnMalloc;

#[cfg(all(
    not(feature = "bench-allocator-snmalloc"),
    feature = "bench-allocator-mimalloc"
))]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> std::process::ExitCode {
    harness::run(std::env::args().skip(1))
}
