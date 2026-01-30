// SPDX-License-Identifier: MPL-2.0

use clap::Parser;
use glowberry_lib::engine::{BackgroundEngine, EngineConfig};
use tracing_subscriber::prelude::*;

/// GlowBerry - Enhanced background service for COSMIC desktop
#[derive(Parser, Debug)]
#[command(name = "glowberry")]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Import configuration from official cosmic-bg
    #[arg(long)]
    import_cosmic_bg: bool,

    /// Export configuration to cosmic-bg format
    #[arg(long)]
    export_cosmic_bg: bool,
}

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    if std::env::var("RUST_SPANTRACE").is_err() {
        unsafe {
            std::env::set_var("RUST_SPANTRACE", "0");
        }
    }

    init_logger();

    let args = Args::parse();

    if args.import_cosmic_bg {
        return import_from_cosmic_bg();
    }

    if args.export_cosmic_bg {
        return export_to_cosmic_bg();
    }

    BackgroundEngine::run(EngineConfig::default())?;

    Ok(())
}

fn import_from_cosmic_bg() -> color_eyre::Result<()> {
    match glowberry_config::import_from_cosmic_bg() {
        Ok(count) => {
            tracing::info!("Successfully imported {} entries from cosmic-bg", count);
            println!("Successfully imported {count} entries from cosmic-bg");
            Ok(())
        }
        Err(e) => {
            tracing::error!("Failed to import from cosmic-bg: {}", e);
            eprintln!("Failed to import from cosmic-bg: {e}");
            Err(e.into())
        }
    }
}

fn export_to_cosmic_bg() -> color_eyre::Result<()> {
    match glowberry_config::export_to_cosmic_bg() {
        Ok(count) => {
            tracing::info!("Successfully exported {} entries to cosmic-bg", count);
            println!("Successfully exported {count} entries to cosmic-bg");
            Ok(())
        }
        Err(e) => {
            tracing::error!("Failed to export to cosmic-bg: {}", e);
            eprintln!("Failed to export to cosmic-bg: {e}");
            Err(e.into())
        }
    }
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
                || (metadata.target().starts_with("glowberry") && metadata.level() <= &log_level)
        }));

    tracing_subscriber::registry().with(log_filter).init();
}

#[cfg(test)]
mod tests {
    use super::{BackgroundEngine, EngineConfig};

    #[test]
    fn main_calls_library() {
        // Compile-time linkage check for glowberry-lib symbols.
        let _run: fn(EngineConfig) -> eyre::Result<()> = BackgroundEngine::run;
        let _ = EngineConfig::default();
    }
}
