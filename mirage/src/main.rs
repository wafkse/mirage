#![forbid(rustdoc::all)]
// NOTE: The clippy lint groups are denied rather than forbidden so that the `#[allow]` attributes emitted by derive and
// `select!`-style macros do not trip the `forbidden_lint_groups` future hard error; the enforcement is unchanged.
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

    // NOTE: `--oneshot` renders synchronously off any runtime; the daemon path owns a multi-thread runtime instead. The
    // one-shot render builds its own current-thread runtime internally to drive async module code, so `main` stays sync.
    let target_result = if target_settings.oneshot {
        Mirage::render_oneshot(target_settings)
    } else {
        run_daemon(target_settings)
    };

    if let Err(target_value) = target_result {
        report_error(&target_value);
    }
}

/// Initialise the tracing subscriber, honouring `RUST_LOG` and otherwise defaulting to verbose informational logging.
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
}

/// Stand up the daemon on a fresh multi-thread runtime and drive it until termination.
fn run_daemon(target_settings: MirageCli) -> eyre::Result<()> {
    tokio::runtime::Runtime::new()?
        .block_on(async move { Mirage::new(target_settings)?.run().await })
}

/// Report a terminal error chain through the tracing subscriber.
fn report_error(target_report: &eyre::Report) {
    let target_chain = target_report
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    tracing::error!(causes = ?target_chain, "mirage exited with an error");
}
