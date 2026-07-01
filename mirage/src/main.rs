#![forbid(rustdoc::all)]
// NOTE: `deny` permits lint allowances emitted by derive and `select!` macros.
#![deny(
    clippy::all,
    clippy::perf,
    clippy::nursery,
    clippy::unwrap_used,
    clippy::panic,
    clippy::pedantic
)]
//! The entry point to the Mirage daemon.

use clap::Parser;

use tracing_subscriber::EnvFilter;

use mirage::server::{Mirage, MirageCli};

use mirage::background::Background;

fn main() {
    init_tracing();

    let target_settings = MirageCli::parse();

    // NOTE: Each execution path creates the Tokio runtime that drives its work.
    let target_result = if target_settings.oneshot {
        Mirage::render_oneshot(target_settings)
    } else {
        run_daemon(target_settings)
    };

    if let Err(target_value) = target_result {
        report_error(&target_value);
    }
}

/// Initialise tracing with the `RUST_LOG` filter or the default `info` filter.
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

/// Run the daemon on a multithreaded Tokio runtime until termination.
fn run_daemon(target_settings: MirageCli) -> eyre::Result<()> {
    let target_runtime = tokio::runtime::Runtime::new()?;

    // NOTE: Daemon construction registers I/O resources with this runtime.
    let _target_guard = target_runtime.enter();

    target_runtime.block_on(Mirage::new(target_settings)?.run())
}

/// Report a terminal error chain through the tracing subscriber.
fn report_error(target_report: &eyre::Report) {
    let target_chain = target_report
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    tracing::error!(causes = ?target_chain, "mirage exited with an error");
}
