// SPDX-License-Identifier: MPL-2.0

use cosmic_bg_lib::engine::{BackgroundEngine, EngineConfig};
use tracing_subscriber::prelude::*;

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    if std::env::var("RUST_SPANTRACE").is_err() {
        unsafe {
            std::env::set_var("RUST_SPANTRACE", "0");
        }
    }

    init_logger();
    BackgroundEngine::run(EngineConfig::default())?;

    Ok(())
}

fn init_logger() {
    let log_level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|level| level.parse::<tracing::Level>().ok())
        .unwrap_or(tracing::Level::INFO);

    let log_format = tracing_subscriber::fmt::format()
        .pretty()
        .without_time()
        .with_line_number(true)
        .with_file(true)
        .with_target(false)
        .with_thread_names(true);

    let log_filter = tracing_subscriber::fmt::Layer::default()
        .with_writer(std::io::stderr)
        .event_format(log_format)
        .with_filter(tracing_subscriber::filter::filter_fn(move |metadata| {
            metadata.level() == &tracing::Level::ERROR
                || (metadata.target().starts_with("cosmic_bg") && metadata.level() <= &log_level)
        }));

    tracing_subscriber::registry().with(log_filter).init();
}

#[cfg(test)]
mod tests {
    use super::{BackgroundEngine, EngineConfig};

    #[test]
    fn main_calls_library() {
        // Compile-time linkage check for cosmic-bg-lib symbols.
        let _run: fn(EngineConfig) -> eyre::Result<()> = BackgroundEngine::run;
        let _ = EngineConfig::default();
    }
}
