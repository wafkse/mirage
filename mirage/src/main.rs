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

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

use mirage::server::{Mirage, MirageCli};

use mirage::background::Background;

/// The command-line settings for this Mirage invocation.
// NOTE(invariant): None.
#[derive(Debug)]
struct Application {
    /// Daemon and rendering options supplied by the user.
    settings: MirageCli,
}

impl Application {
    /// Run the requested mode and report terminal failures with a failure exit status.
    fn run(self) -> ExitCode {
        let Self { settings } = self;

        let MirageCli { oneshot, .. } = &settings;

        let target_result = if *oneshot {
            Mirage::oneshot(settings)
        } else {
            Self::daemon(settings)
        };

        match target_result {
            Ok(()) => ExitCode::SUCCESS,
            Err(target_value) => {
                Self::report(&target_value);

                ExitCode::FAILURE
            }
        }
    }

    /// Initialise tracing with the `RUST_LOG` filter or the default `info` filter.
    fn tracing() {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .init();
    }

    /// Run the daemon on a multithreaded Tokio runtime until termination.
    fn daemon(target_settings: MirageCli) -> eyre::Result<()> {
        let target_runtime = tokio::runtime::Runtime::new()?;

        // NOTE: Daemon construction registers I/O resources with this runtime.
        let _target_guard = target_runtime.enter();

        target_runtime.block_on(Mirage::new(target_settings)?.run())
    }

    /// Report a terminal error chain through the tracing subscriber.
    fn report(target_report: &eyre::Report) {
        let target_chain = target_report
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        tracing::error!(causes = ?target_chain, "mirage exited with an error");
    }
}

fn main() -> ExitCode {
    Application::tracing();

    let settings = MirageCli::parse();

    Application { settings }.run()
}
