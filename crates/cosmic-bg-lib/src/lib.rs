pub mod colored;
pub mod draw;
pub mod engine;
pub mod fragment_canvas;
pub mod gpu;
pub mod img_source;
pub mod scaler;
pub mod user_context;
pub mod wallpaper;

pub use engine::{BackgroundEngine, BackgroundHandle, CosmicBg, CosmicBgLayer, EngineConfig};
pub use user_context::{EnvGuard, UserContext};
pub use wallpaper::Wallpaper;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_config_defaults() {
        let config = EngineConfig::default();
        assert!(config.enable_wayland);
    }
}
