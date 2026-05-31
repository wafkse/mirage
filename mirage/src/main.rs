#![forbid(
    clippy::all,
    clippy::perf,
    clippy::nursery,
    clippy::unwrap_used,
    clippy::panic,
    clippy::pedantic,
    rustdoc::all
)]
//! The entry point to the Mirage daemon.

use clap::Parser;

use mirage::server::{Mirage, MirageCli};

use mirage::background::Background;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    match Mirage::new(MirageCli::parse())?.run().await {
        Ok(..) => Ok(()),
        Err(target_value) => {
            eprintln!("{target_value:?}");

            let mut target_error = target_value.source();

            while let Some(source_error) = target_error {
                eprintln!("Caused by:\n\t{source_error}");

                target_error = source_error.source();
            }

            Ok(())
        }
    }
}
