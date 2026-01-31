// SPDX-License-Identifier: MPL-2.0

use crate::{
    fragment_canvas, gpu, img_source,
    toplevel_info::{AsToplevelTracker, ToplevelTracker},
    upower::{start_power_monitor, PowerMonitorHandle, PowerStateChanged},
    user_context::{EnvGuard, UserContext},
    wallpaper::Wallpaper,
    workspace_info::{AsWorkspaceTracker, WorkspaceTracker},
};
use cosmic_config::{calloop::ConfigWatchSource, CosmicConfigEntry};
use cosmic_protocols::toplevel_info::v1::client::{
    zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
    zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1,
};
use eyre::{eyre, Context};
use glowberry_config::{
    power_saving::{OnBatteryAction, PowerSavingConfig},
    state::State,
    Config,
};
use sctk::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputInfo, OutputState},
    reexports::{
        calloop,
        calloop_wayland_source::WaylandSource,
        client::{
            delegate_noop,
            globals::registry_queue_init,
            protocol::{
                wl_output::{self, WlOutput},
                wl_surface,
            },
            Connection, Dispatch, Proxy, QueueHandle, Weak,
        },
        protocols::{
            ext::foreign_toplevel_list::v1::client::{
                ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
                ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
            },
            ext::workspace::v1::client::{
                ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
                ext_workspace_handle_v1::ExtWorkspaceHandleV1,
                ext_workspace_manager_v1::ExtWorkspaceManagerV1,
            },
            wp::{
                fractional_scale::v1::client::{
                    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
                },
                viewporter::client::{wp_viewport, wp_viewporter},
            },
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::thread;
use tracing::error;

/// Access glibc malloc tunables.
#[cfg(target_env = "gnu")]
mod malloc {
    use std::os::raw::c_int;
    const M_MMAP_THRESHOLD: c_int = -3;

    unsafe extern "C" {
        fn malloc_trim(pad: usize);
        fn mallopt(param: c_int, value: c_int) -> c_int;
    }

    /// Prevents glibc from hoarding memory via memory fragmentation.
    pub fn limit_mmap_threshold() {
        unsafe {
            mallopt(M_MMAP_THRESHOLD, 65536);
        }
    }

    /// Asks glibc to trim malloc arenas.
    pub fn trim() {
        unsafe {
            malloc_trim(0);
        }
    }
}

/// GPU state for shader-based live wallpapers.
pub struct GpuLayerState {
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    canvas: fragment_canvas::FragmentCanvas,
}

// Manual Debug impl since wgpu types don't implement Debug
impl std::fmt::Debug for GpuLayerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuLayerState")
            .field("surface_config", &self.surface_config)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct EngineConfig {
    pub enable_wayland: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            enable_wayland: true,
        }
    }
}

#[derive(Debug)]
pub struct BackgroundEngine;

impl BackgroundEngine {
    #[allow(clippy::too_many_lines)]
    pub fn run(config: EngineConfig) -> eyre::Result<()> {
        Self::run_with_stop(config, None)
    }

    #[allow(clippy::too_many_lines)]
    fn run_with_stop(
        config: EngineConfig,
        stop_rx: Option<calloop::channel::Channel<()>>,
    ) -> eyre::Result<()> {
        if !config.enable_wayland {
            return Ok(());
        }

        // Prevents glibc from hoarding memory via memory fragmentation.
        #[cfg(target_env = "gnu")]
        malloc::limit_mmap_threshold();

        let conn = Connection::connect_to_env().wrap_err("wayland client connection failed")?;
        // Clone the connection for use in CosmicBg state (needed for GPU surface creation)
        let conn_for_state = conn.clone();

        let mut event_loop: calloop::EventLoop<'static, CosmicBg> =
            calloop::EventLoop::try_new().wrap_err("failed to create event loop")?;

        let (globals, event_queue) =
            registry_queue_init(&conn).wrap_err("failed to initialize registry queue")?;

        let qh = event_queue.handle();

        WaylandSource::new(conn, event_queue)
            .insert(event_loop.handle())
            .map_err(|err| err.error)
            .wrap_err("failed to insert main EventLoop into WaylandSource")?;

        if let Some(stop_rx) = stop_rx {
            event_loop
                .handle()
                .insert_source(stop_rx, |event, _, state| match event {
                    calloop::channel::Event::Msg(()) | calloop::channel::Event::Closed => {
                        state.exit = true;
                    }
                })
                .map_err(|err| eyre!("failed to insert stop channel into event loop: {err}"))?;
        }

        let config_context = glowberry_config::context();

        let config = match config_context {
            Ok(config_context) => {
                let source = ConfigWatchSource::new(&config_context.0)
                    .expect("failed to create ConfigWatchSource");

                let conf_context = config_context.clone();
                event_loop
                    .handle()
                    .insert_source(source, move |(_config, keys), (), state| {
                        let mut changes_applied = false;

                        for key in &keys {
                            match key.as_str() {
                                glowberry_config::BACKGROUNDS => {
                                    tracing::debug!("updating backgrounds");
                                    state.config.load_backgrounds(&conf_context);
                                    changes_applied = true;
                                }

                                glowberry_config::DEFAULT_BACKGROUND => {
                                    tracing::debug!("updating default background");
                                    let entry = conf_context.default_background();

                                    if state.config.default_background != entry {
                                        state.config.default_background = entry;
                                        changes_applied = true;
                                    }
                                }

                                glowberry_config::SAME_ON_ALL => {
                                    tracing::debug!("updating same_on_all");
                                    state.config.same_on_all = conf_context.same_on_all();

                                    if state.config.same_on_all {
                                        state.config.outputs.clear();
                                    } else {
                                        state.config.load_backgrounds(&conf_context);
                                    }
                                    state.config.outputs.clear();
                                    changes_applied = true;
                                }

                                // Power saving config keys
                                glowberry_config::power_saving::PAUSE_ON_FULLSCREEN
                                | glowberry_config::power_saving::PAUSE_ON_COVERED
                                | glowberry_config::power_saving::COVERAGE_THRESHOLD
                                | glowberry_config::power_saving::ADJUST_ON_BATTERY
                                | glowberry_config::power_saving::ON_BATTERY_ACTION
                                | glowberry_config::power_saving::PAUSE_ON_LOW_BATTERY
                                | glowberry_config::power_saving::LOW_BATTERY_THRESHOLD
                                | glowberry_config::power_saving::PAUSE_ON_LID_CLOSED => {
                                    tracing::debug!(key, "power saving config changed");
                                    state.power_saving_config = conf_context.power_saving_config();
                                    tracing::info!(config = ?state.power_saving_config, "Updated power saving config");
                                    // Force reapply frame rates with new config
                                    state.reapply_frame_rates();
                                }

                                _ => {
                                    tracing::debug!(key, "key modified");
                                    if let Some(output) = key.strip_prefix("output.") {
                                        if let Ok(new_entry) = conf_context.entry(key) {
                                            if let Some(existing) = state.config.entry_mut(output) {
                                                *existing = new_entry;
                                                changes_applied = true;
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if changes_applied {
                            state.apply_backgrounds();

                            #[cfg(target_env = "gnu")]
                            malloc::trim();

                            tracing::debug!(
                                same_on_all = state.config.same_on_all,
                                outputs = ?state.config.outputs,
                                backgrounds = ?state.config.backgrounds,
                                default_background = ?state.config.default_background.source,
                                "new state"
                            );
                        }
                    })
                    .expect("failed to insert config watching source into event loop");

                Config::load(&config_context).unwrap_or_else(|why| {
                    tracing::error!(?why, "Config file error, falling back to defaults");
                    Config::default()
                })
            }
            Err(why) => {
                tracing::error!(?why, "Config file error, falling back to defaults");
                Config::default()
            }
        };

        // Load power saving configuration
        let power_saving_config = glowberry_config::context()
            .map(|ctx| ctx.power_saving_config())
            .unwrap_or_default();
        tracing::info!(?power_saving_config, "Loaded power saving config");

        // Create channel for power state change notifications
        let (power_notify_tx, power_notify_rx) = calloop::channel::channel();

        // Start power monitor for battery/lid state tracking
        let power_monitor = start_power_monitor(Some(power_notify_tx));
        if power_monitor.is_some() {
            tracing::info!("Power monitor started successfully");
        } else {
            tracing::warn!("Failed to start power monitor, power saving features will be disabled");
        }

        // Insert power state change notification source into event loop
        event_loop
            .handle()
            .insert_source(power_notify_rx, |event, _, state| {
                if let calloop::channel::Event::Msg(PowerStateChanged) = event {
                    tracing::debug!("Received power state change notification");
                    state.on_power_state_changed();
                }
            })
            .expect("failed to insert power notification channel into event loop");

        // Initialize toplevel tracker for fullscreen detection
        let toplevel_tracker = ToplevelTracker::try_new(&globals, &qh);
        if toplevel_tracker.is_some() {
            tracing::info!("Fullscreen detection enabled via zcosmic_toplevel_info_v1");
        } else {
            tracing::warn!(
                "Fullscreen detection unavailable - zcosmic_toplevel_info_v1 protocol not supported"
            );
        }

        // Initialize workspace tracker for active workspace detection
        let workspace_tracker = WorkspaceTracker::try_new(&globals, &qh);
        if workspace_tracker.is_some() {
            tracing::info!("Workspace tracking enabled via ext_workspace_manager_v1");
        } else {
            tracing::warn!(
                "Workspace tracking unavailable - ext_workspace_manager_v1 protocol not supported"
            );
        }

        let source_tx = img_source::img_source(&event_loop.handle(), |state, source, event| {
            use notify::event::{ModifyKind, RenameMode};

            match event.kind {
                notify::EventKind::Create(_)
                | notify::EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                    for w in state
                        .wallpapers
                        .iter_mut()
                        .filter(|w| w.entry.output == source)
                    {
                        for p in &event.paths {
                            if !w.image_queue.contains(p) {
                                w.image_queue.push_front(p.into());
                            }
                        }
                        w.image_queue.retain(|p| !event.paths.contains(p));
                        // TODO maybe resort or shuffle at some point?
                    }
                }
                notify::EventKind::Remove(_)
                | notify::EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                    for w in state
                        .wallpapers
                        .iter_mut()
                        .filter(|w| w.entry.output == source)
                    {
                        w.image_queue.retain(|p| !event.paths.contains(p));
                    }
                }
                _ => {}
            }
        });

        // initial setup with all images
        let wallpapers = {
            let mut wallpapers = Vec::with_capacity(config.backgrounds.len() + 1);

            wallpapers.extend({
                config.backgrounds.iter().map(|bg| {
                    Wallpaper::new(
                        bg.clone(),
                        qh.clone(),
                        event_loop.handle(),
                        source_tx.clone(),
                    )
                })
            });

            wallpapers.sort_by(|a, b| a.entry.output.cmp(&b.entry.output));

            wallpapers.push(Wallpaper::new(
                config.default_background.clone(),
                qh.clone(),
                event_loop.handle(),
                source_tx.clone(),
            ));

            wallpapers
        };

        // Check if any wallpaper uses a shader source
        let has_shader_source = config
            .backgrounds
            .iter()
            .any(|bg| matches!(bg.source, glowberry_config::Source::Shader(_)))
            || matches!(
                config.default_background.source,
                glowberry_config::Source::Shader(_)
            );

        // Lazily initialize GPU renderer only if needed
        let gpu_renderer = if has_shader_source {
            tracing::info!("Initializing GPU renderer for shader wallpapers");
            Some(gpu::GpuRenderer::new())
        } else {
            None
        };

        let mut bg_state = CosmicBg {
            registry_state: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            compositor_state: CompositorState::bind(&globals, &qh).unwrap(),
            shm_state: Shm::bind(&globals, &qh).unwrap(),
            layer_state: LayerShell::bind(&globals, &qh).unwrap(),
            viewporter: globals.bind(&qh, 1..=1, ()).unwrap(),
            fractional_scale_manager: globals.bind(&qh, 1..=1, ()).ok(),
            qh,
            source_tx,
            loop_handle: event_loop.handle(),
            exit: false,
            wallpapers,
            config,
            active_outputs: Vec::new(),
            gpu_renderer,
            connection: conn_for_state,
            power_monitor,
            power_saving_config,
            current_frame_rate_override: None,
            was_on_battery: false,
            toplevel_tracker,
            workspace_tracker,
        };

        loop {
            event_loop.dispatch(None, &mut bg_state)?;

            if bg_state.exit {
                break;
            }
        }

        Ok(())
    }
}

pub struct BackgroundHandle {
    stop_tx: calloop::channel::Sender<()>,
    join: Option<thread::JoinHandle<()>>,
    env_guard: Option<EnvGuard>,
}

impl BackgroundHandle {
    pub fn spawn(user: UserContext, config: EngineConfig) -> Self {
        // Environment variables are process-wide, so keep the guard for the handle lifetime.
        let env_guard = user.apply();
        let (stop_tx, stop_rx) = calloop::channel::channel();
        let join = thread::spawn(move || {
            if let Err(err) = BackgroundEngine::run_with_stop(config, Some(stop_rx)) {
                tracing::error!(?err, "background engine exited with error");
            }
        });

        Self {
            stop_tx,
            join: Some(join),
            env_guard: Some(env_guard),
        }
    }

    pub fn stop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        self.env_guard.take();
    }
}

impl Drop for BackgroundHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug)]
pub struct CosmicBgLayer {
    pub(crate) layer: LayerSurface,
    pub(crate) viewport: wp_viewport::WpViewport,
    pub(crate) wl_output: WlOutput,
    pub(crate) output_info: OutputInfo,
    pub(crate) pool: Option<SlotPool>,
    pub(crate) needs_redraw: bool,
    pub(crate) size: Option<(u32, u32)>,
    pub(crate) fractional_scale: Option<u32>,
    /// GPU state for shader wallpapers (None for static wallpapers).
    pub(crate) gpu_state: Option<GpuLayerState>,
}

pub struct CosmicBg {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    shm_state: Shm,
    layer_state: LayerShell,
    viewporter: wp_viewporter::WpViewporter,
    fractional_scale_manager: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    qh: QueueHandle<CosmicBg>,
    source_tx: calloop::channel::SyncSender<(String, notify::Event)>,
    loop_handle: calloop::LoopHandle<'static, CosmicBg>,
    exit: bool,
    pub(crate) wallpapers: Vec<Wallpaper>,
    config: Config,
    active_outputs: Vec<WlOutput>,
    /// GPU renderer for shader wallpapers (lazily initialized).
    gpu_renderer: Option<gpu::GpuRenderer>,
    /// Wayland connection for creating GPU surfaces.
    connection: Connection,
    /// Power monitor handle for battery/lid state.
    power_monitor: Option<PowerMonitorHandle>,
    /// Power saving configuration.
    power_saving_config: PowerSavingConfig,
    /// Currently applied frame rate override (None = using configured rates).
    current_frame_rate_override: Option<u8>,
    /// Whether we were on battery in the last check (for detecting changes).
    was_on_battery: bool,
    /// Toplevel tracker for fullscreen detection (None if protocol unavailable).
    toplevel_tracker: Option<ToplevelTracker>,
    /// Workspace tracker for active workspace detection (None if protocol unavailable).
    workspace_tracker: Option<WorkspaceTracker>,
}

// Manual Debug impl since wgpu types don't implement Debug
impl std::fmt::Debug for CosmicBg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CosmicBg")
            .field("exit", &self.exit)
            .field("wallpapers", &self.wallpapers)
            .field("config", &self.config)
            .field("active_outputs", &self.active_outputs)
            .field("gpu_renderer", &self.gpu_renderer.is_some())
            .field("power_monitor", &self.power_monitor.is_some())
            .field("toplevel_tracker", &self.toplevel_tracker)
            .finish_non_exhaustive()
    }
}

/// Reason why shader animation is paused.
#[derive(Debug, Clone, Copy)]
enum PauseReason {
    /// A fullscreen application is covering this output.
    FullscreenApp,
    /// The laptop lid is closed.
    LidClosed,
    /// Battery level is below the configured threshold.
    LowBattery { percentage: u8, threshold: u8 },
    /// Running on battery power with pause action configured.
    OnBattery,
}

impl std::fmt::Display for PauseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PauseReason::FullscreenApp => write!(f, "fullscreen app detected"),
            PauseReason::LidClosed => write!(f, "lid closed"),
            PauseReason::LowBattery {
                percentage,
                threshold,
            } => write!(
                f,
                "low battery ({}% <= {}% threshold)",
                percentage, threshold
            ),
            PauseReason::OnBattery => write!(f, "on battery power"),
        }
    }
}

impl CosmicBg {
    /// Check if shader animation should be paused based on current power state and fullscreen windows.
    /// Returns the reason for pausing, or None if animation should continue.
    fn get_pause_reason(&self, output: &WlOutput) -> Option<PauseReason> {
        let config = &self.power_saving_config;

        // Check fullscreen app on this output
        if config.pause_on_fullscreen {
            if let Some(ref tracker) = self.toplevel_tracker {
                // Extract the raw object ID number from the WlOutput
                // The object ID format is "wl_output@N" where N is the number we need
                let output_id = output.id().protocol_id();

                // Get active workspace IDs for this output (empty if workspace tracking unavailable)
                let active_workspace_ids = self
                    .workspace_tracker
                    .as_ref()
                    .map(|wt| wt.get_active_workspace_ids_for_output(output_id))
                    .unwrap_or_default();

                // Check if there's a fullscreen window on an active workspace on this output
                if tracker.has_active_fullscreen_on_output_id(output_id, &active_workspace_ids) {
                    return Some(PauseReason::FullscreenApp);
                }
            }
        }

        // Power state checks (apply to all outputs)
        let Some(ref power_monitor) = self.power_monitor else {
            return None; // No power monitor, don't pause
        };

        let power_state = power_monitor.current();

        // Check lid closed (pause on internal displays)
        if config.pause_on_lid_closed && power_state.lid_is_closed {
            return Some(PauseReason::LidClosed);
        }

        // Check low battery
        if config.pause_on_low_battery {
            if let Some(percentage) = power_state.battery_percentage {
                if percentage <= config.low_battery_threshold as f64 {
                    return Some(PauseReason::LowBattery {
                        percentage: percentage as u8,
                        threshold: config.low_battery_threshold,
                    });
                }
            }
        }

        // Check on battery action
        if power_state.on_battery && config.on_battery_action == OnBatteryAction::Pause {
            return Some(PauseReason::OnBattery);
        }

        None
    }

    /// Check if shader animation should be paused for the given output.
    fn should_pause_animation(&self, output: &WlOutput) -> bool {
        self.get_pause_reason(output).is_some()
    }

    /// Check if animation should be paused globally (for any reason not output-specific).
    /// Used when we don't have a specific output to check.
    fn should_pause_animation_global(&self) -> bool {
        let Some(ref power_monitor) = self.power_monitor else {
            return false;
        };

        let power_state = power_monitor.current();
        let config = &self.power_saving_config;

        // Check lid closed
        if config.pause_on_lid_closed && power_state.lid_is_closed {
            return true;
        }

        // Check low battery
        if config.pause_on_low_battery {
            if let Some(percentage) = power_state.battery_percentage {
                if percentage <= config.low_battery_threshold as f64 {
                    return true;
                }
            }
        }

        // Check on battery action
        if power_state.on_battery && config.on_battery_action == OnBatteryAction::Pause {
            return true;
        }

        false
    }

    /// Check if power state has changed and update frame rates if needed.
    /// Returns true if frame rate was changed.
    fn check_and_update_frame_rates(&mut self) -> bool {
        let Some(ref power_monitor) = self.power_monitor else {
            return false;
        };

        let power_state = power_monitor.current();
        let on_battery = power_state.on_battery;

        // Check if battery state changed
        if on_battery == self.was_on_battery {
            return false;
        }

        self.was_on_battery = on_battery;
        self.reapply_frame_rates();
        true
    }

    /// Reapply frame rate settings based on current power state and config.
    /// Called when config changes or battery state changes.
    fn reapply_frame_rates(&mut self) {
        let on_battery = self
            .power_monitor
            .as_ref()
            .map(|pm| pm.current().on_battery)
            .unwrap_or(false);

        // Determine new frame rate override
        let new_override = if on_battery {
            self.power_saving_config.on_battery_action.frame_rate()
        } else {
            None // Restore to configured rate
        };

        // Check if override actually changed
        if new_override == self.current_frame_rate_override {
            return;
        }

        self.current_frame_rate_override = new_override;

        // Apply to all shader canvases
        for wallpaper in &mut self.wallpapers {
            for layer in &mut wallpaper.layers {
                if let Some(gpu_state) = &mut layer.gpu_state {
                    gpu_state.canvas.set_frame_rate_override(new_override);
                    tracing::info!(
                        output = ?layer.output_info.name,
                        override_fps = ?new_override,
                        configured_fps = gpu_state.canvas.configured_frame_rate(),
                        "Updated shader frame rate"
                    );
                }
            }
        }
    }

    /// Called when power state changes (from D-Bus notification).
    /// This handles resuming from paused state and updating frame rates.
    fn on_power_state_changed(&mut self) {
        let was_paused = self.should_pause_animation_global();

        // Update battery state tracking
        if let Some(ref power_monitor) = self.power_monitor {
            self.was_on_battery = power_monitor.current().on_battery;
        }

        // Reapply frame rates based on new power state
        self.reapply_frame_rates();

        let is_paused = self.should_pause_animation_global();

        // If we were paused and now we're not, request frame callbacks to resume
        if was_paused && !is_paused {
            tracing::info!("Resuming shader animation after power state change");
            self.request_frame_callbacks_if_needed();
        }
    }

    /// Request frame callbacks for shader layers that should not be paused.
    /// Used to resume animation after being paused (either by power state or fullscreen changes).
    fn request_frame_callbacks_if_needed(&mut self) {
        // Collect outputs that need frame callbacks (to avoid borrow issues)
        let outputs_needing_frames: Vec<WlOutput> = self
            .wallpapers
            .iter()
            .flat_map(|w| w.layers.iter())
            .filter(|l| l.gpu_state.is_some())
            .map(|l| l.wl_output.clone())
            .collect();

        // Check which outputs should not be paused
        let outputs_to_resume: Vec<WlOutput> = outputs_needing_frames
            .into_iter()
            .filter(|output| !self.should_pause_animation(output))
            .collect();

        // Now request frame callbacks for those outputs
        let qh = self.qh.clone();
        for wallpaper in &mut self.wallpapers {
            for layer in &mut wallpaper.layers {
                if layer.gpu_state.is_some() && outputs_to_resume.contains(&layer.wl_output) {
                    let wl_surface = layer.layer.wl_surface();
                    wl_surface.frame(&qh, wl_surface.clone());
                    layer.layer.commit();
                    tracing::info!(
                        output = ?layer.output_info.name.as_deref().unwrap_or("unknown"),
                        "Resuming shader animation"
                    );
                }
            }
        }
    }

    fn shader_physical_size(
        layer_size: Option<(u32, u32)>,
        fractional_scale: Option<u32>,
        output_mode_dims: Option<(u32, u32)>,
    ) -> (u32, u32) {
        if let Some((w, h)) = layer_size {
            let scale = fractional_scale.unwrap_or(120);
            return (w * scale / 120, h * scale / 120);
        }

        if let Some((w, h)) = output_mode_dims {
            return (w, h);
        }

        let (w, h) = (1920, 1080);
        let scale = fractional_scale.unwrap_or(120);
        (w * scale / 120, h * scale / 120)
    }

    fn shader_layer_physical_size(layer: &CosmicBgLayer) -> (u32, u32) {
        let output_mode_dims = layer
            .output_info
            .modes
            .iter()
            .find(|m| m.current)
            .map(|m| (m.dimensions.0 as u32, m.dimensions.1 as u32));

        Self::shader_physical_size(layer.size, layer.fractional_scale, output_mode_dims)
    }

    fn update_shader_layer_surface(
        gpu: &gpu::GpuRenderer,
        qh: &QueueHandle<Self>,
        layer: &mut CosmicBgLayer,
    ) {
        let (physical_w, physical_h) = Self::shader_layer_physical_size(layer);
        let Some(gpu_state) = layer.gpu_state.as_mut() else {
            return;
        };

        gpu_state.surface_config =
            gpu.configure_surface(&gpu_state.surface, physical_w, physical_h);
        gpu_state
            .canvas
            .update_resolution(gpu.queue(), physical_w, physical_h);

        // Set viewport destination to logical size so compositor scales correctly
        if let Some((logical_w, logical_h)) = layer.size {
            layer
                .viewport
                .set_destination(logical_w as i32, logical_h as i32);
        }

        let wl_surface = layer.layer.wl_surface();
        wl_surface.frame(qh, wl_surface.clone());
        layer.layer.commit();
    }

    fn apply_backgrounds(&mut self) {
        self.wallpapers.clear();

        let mut all_wallpaper = Wallpaper::new(
            self.config.default_background.clone(),
            self.qh.clone(),
            self.loop_handle.clone(),
            self.source_tx.clone(),
        );

        let mut backgrounds = self.config.backgrounds.clone();
        backgrounds.sort_by(|a, b| a.output.cmp(&b.output));

        'outer: for output in &self.active_outputs {
            let Some(output_info) = self.output_state.info(output) else {
                continue;
            };

            let o_name = output_info.name.clone().unwrap_or_default();
            for background in &backgrounds {
                if background.output == o_name {
                    let mut new_wallpaper = Wallpaper::new(
                        background.clone(),
                        self.qh.clone(),
                        self.loop_handle.clone(),
                        self.source_tx.clone(),
                    );

                    new_wallpaper
                        .layers
                        .push(self.new_layer(output.clone(), output_info));
                    _ = new_wallpaper.save_state();
                    self.wallpapers.push(new_wallpaper);

                    continue 'outer;
                }
            }

            all_wallpaper
                .layers
                .push(self.new_layer(output.clone(), output_info));
        }

        _ = all_wallpaper.save_state();
        self.wallpapers.push(all_wallpaper);
    }

    #[must_use]
    pub fn new_layer(&self, output: WlOutput, output_info: OutputInfo) -> CosmicBgLayer {
        let surface = self.compositor_state.create_surface(&self.qh);

        let layer = self.layer_state.create_layer_surface(
            &self.qh,
            surface.clone(),
            Layer::Background,
            "wallpaper".into(),
            Some(&output),
        );

        layer.set_anchor(Anchor::all());
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        surface.commit();

        let viewport = self.viewporter.get_viewport(&surface, &self.qh, ());

        let fractional_scale = if let Some(mngr) = self.fractional_scale_manager.as_ref() {
            mngr.get_fractional_scale(&surface, &self.qh, surface.downgrade());
            None
        } else {
            (self.compositor_state.wl_compositor().version() < 6)
                .then_some(output_info.scale_factor as u32 * 120)
        };

        CosmicBgLayer {
            layer,
            viewport,
            wl_output: output,
            output_info,
            size: None,
            fractional_scale,
            needs_redraw: false,
            pool: None,
            gpu_state: None,
        }
    }

    /// Initialize GPU state for a shader wallpaper layer (internal version using indices).
    fn init_gpu_layer_internal(
        &mut self,
        wallpaper_idx: usize,
        layer_idx: usize,
        shader_source: &glowberry_config::ShaderSource,
    ) {
        // Ensure GPU renderer is initialized
        if self.gpu_renderer.is_none() {
            tracing::info!("Lazily initializing GPU renderer for shader wallpaper");
            self.gpu_renderer = Some(gpu::GpuRenderer::new());
        }

        let gpu = self.gpu_renderer.as_ref().unwrap();

        // Get layer info needed for surface creation
        let layer = &self.wallpapers[wallpaper_idx].layers[layer_idx];
        let wl_surface = layer.layer.wl_surface().clone();
        let output_name = layer.output_info.name.clone();

        // Get native resolution from the current output mode
        let (physical_width, physical_height) = layer
            .output_info
            .modes
            .iter()
            .find(|m| m.current)
            .map(|m| (m.dimensions.0 as u32, m.dimensions.1 as u32))
            .unwrap_or_else(|| {
                // Fallback to layer size with scale if no mode info
                let (w, h) = layer.size.unwrap_or((1920, 1080));
                let scale = layer.fractional_scale.unwrap_or(120);
                (w * scale / 120, h * scale / 120)
            });

        tracing::debug!(
            output = ?output_name,
            physical_width,
            physical_height,
            "GPU layer dimensions (native resolution)"
        );

        // Create GPU surface
        let surface = unsafe { gpu.create_surface(&self.connection, &wl_surface) };

        // Configure surface at native resolution
        let surface_config = gpu.configure_surface(&surface, physical_width, physical_height);

        // Create fragment canvas
        match fragment_canvas::FragmentCanvas::new(gpu, shader_source, surface_config.format) {
            Ok(mut canvas) => {
                canvas.update_resolution(gpu.queue(), physical_width, physical_height);

                // Render the first frame immediately to avoid showing default wallpaper
                if let Ok(surface_texture) = surface.get_current_texture() {
                    let view = surface_texture
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default());
                    canvas.render(gpu, &view);
                    surface_texture.present();
                    canvas.mark_frame_rendered();
                    tracing::debug!(output = ?output_name, "Rendered initial shader frame");
                }

                let layer = &mut self.wallpapers[wallpaper_idx].layers[layer_idx];
                layer.gpu_state = Some(GpuLayerState {
                    surface,
                    surface_config,
                    canvas,
                });

                // Set viewport destination to logical size so compositor scales correctly
                if let Some((logical_w, logical_h)) = layer.size {
                    layer
                        .viewport
                        .set_destination(logical_w as i32, logical_h as i32);
                }

                // Request first frame callback to continue animation
                wl_surface.frame(&self.qh, wl_surface.clone());
                layer.layer.commit();

                tracing::info!(
                    output = ?output_name,
                    "Initialized GPU layer for shader wallpaper"
                );
            }
            Err(err) => {
                tracing::error!(
                    ?err,
                    "Failed to create fragment canvas for shader wallpaper"
                );
            }
        }
    }
}

impl CompositorHandler for CosmicBg {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if self.fractional_scale_manager.is_none() {
            let mut target: Option<(usize, usize, bool)> = None;
            for (wallpaper_idx, wallpaper) in self.wallpapers.iter().enumerate() {
                if let Some(layer_idx) = wallpaper
                    .layers
                    .iter()
                    .position(|layer| layer.layer.wl_surface() == surface)
                {
                    target = Some((wallpaper_idx, layer_idx, wallpaper.is_shader()));
                    break;
                }
            }

            if let Some((wallpaper_idx, layer_idx, is_shader)) = target {
                let qh = self.qh.clone();
                let gpu = self.gpu_renderer.as_ref();
                let wallpaper = &mut self.wallpapers[wallpaper_idx];
                let layer = &mut wallpaper.layers[layer_idx];
                layer.fractional_scale = Some(new_factor as u32 * 120);
                if is_shader {
                    if let Some(gpu) = gpu {
                        Self::update_shader_layer_surface(gpu, &qh, layer);
                    }
                } else {
                    wallpaper.draw();
                }
            }
        }
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // Check for power state changes and update frame rates if needed
        self.check_and_update_frame_rates();

        // Find the output and output name for this surface first (immutable borrow)
        let output_info = self
            .wallpapers
            .iter()
            .flat_map(|w| w.layers.iter())
            .find(|l| l.layer.wl_surface() == surface)
            .map(|l| (l.wl_output.clone(), l.output_info.name.clone()));

        let Some((output, output_name)) = output_info else {
            return;
        };

        // Check if animation should be paused for this specific output (and get reason)
        let pause_reason = self.get_pause_reason(&output);

        // Find the wallpaper and layer for this surface (mutable borrow)
        for wallpaper in &mut self.wallpapers {
            if let Some(layer) = wallpaper
                .layers
                .iter_mut()
                .find(|l| l.layer.wl_surface() == surface)
            {
                // Check if this is a shader wallpaper with GPU state
                if let Some(gpu_state) = &mut layer.gpu_state {
                    // Skip rendering if paused
                    if pause_reason.is_none() {
                        // Check if we should render this frame (frame rate limiting)
                        if gpu_state.canvas.should_render() {
                            if let Some(gpu) = &self.gpu_renderer {
                                // Get current texture
                                match gpu_state.surface.get_current_texture() {
                                    Ok(surface_texture) => {
                                        let view = surface_texture
                                            .texture
                                            .create_view(&wgpu::TextureViewDescriptor::default());

                                        // Update resolution for this specific layer's surface
                                        let width = gpu_state.surface_config.width;
                                        let height = gpu_state.surface_config.height;

                                        tracing::trace!(
                                            output = ?layer.output_info.name,
                                            width,
                                            height,
                                            "Rendering shader frame"
                                        );

                                        gpu_state.canvas.update_resolution(
                                            gpu.queue(),
                                            width,
                                            height,
                                        );

                                        // Render the shader
                                        gpu_state.canvas.render(gpu, &view);

                                        // Present
                                        surface_texture.present();

                                        gpu_state.canvas.mark_frame_rendered();
                                    }
                                    Err(wgpu::SurfaceError::Timeout) => {
                                        tracing::warn!("GPU surface timeout");
                                    }
                                    Err(
                                        wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated,
                                    ) => {
                                        let width = gpu_state.surface_config.width;
                                        let height = gpu_state.surface_config.height;
                                        gpu_state.surface_config = gpu.configure_surface(
                                            &gpu_state.surface,
                                            width,
                                            height,
                                        );
                                        gpu_state.canvas.update_resolution(
                                            gpu.queue(),
                                            width,
                                            height,
                                        );
                                        tracing::warn!(
                                            "GPU surface lost or outdated; reconfigured surface"
                                        );
                                    }
                                    Err(wgpu::SurfaceError::OutOfMemory) => {
                                        tracing::error!("GPU out of memory");
                                    }
                                    Err(err) => {
                                        tracing::warn!(?err, "GPU surface error");
                                    }
                                }
                            }
                        }
                    }

                    // Request next frame callback to continue animation
                    // Only request if not paused - when paused, GPU goes truly idle
                    // The on_power_state_changed handler will request frames when resuming
                    if let Some(reason) = pause_reason {
                        tracing::info!(
                            output = ?output_name.as_deref().unwrap_or("unknown"),
                            reason = %reason,
                            "Pausing shader animation"
                        );
                    } else {
                        surface.frame(qh, surface.clone());
                        layer.layer.commit();
                    }
                }
                break;
            }
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
        // TODO
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &WlOutput,
    ) {
    }
}

impl OutputHandler for CosmicBg {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        wl_output: wl_output::WlOutput,
    ) {
        self.active_outputs.push(wl_output.clone());
        let Some(output_info) = self.output_state.info(&wl_output) else {
            return;
        };

        // Log the output ID to name mapping for debugging fullscreen detection
        tracing::info!(
            output_name = ?output_info.name,
            output_id = wl_output.id().protocol_id(),
            "New output discovered"
        );

        if let Some(pos) = self
            .wallpapers
            .iter()
            .position(|w| match w.entry.output.as_str() {
                "all" => !w.layers.iter().any(|l| l.wl_output == wl_output),
                name => {
                    Some(name) == output_info.name.as_deref()
                        && !w.layers.iter().any(|l| l.wl_output == wl_output)
                }
            })
        {
            let layer = self.new_layer(wl_output, output_info);
            self.wallpapers[pos].layers.push(layer);
            if let Err(err) = self.wallpapers[pos].save_state() {
                tracing::error!("{err}");
            }
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.fractional_scale_manager.is_none()
            && self.compositor_state.wl_compositor().version() < 6
        {
            let Some(output_info) = self.output_state.info(&output) else {
                return;
            };
            let output_info = output_info.clone();
            let mut target: Option<(usize, usize, bool)> = None;
            for (wallpaper_idx, wallpaper) in self.wallpapers.iter().enumerate() {
                if let Some(layer_idx) = wallpaper
                    .layers
                    .iter()
                    .position(|layer| layer.wl_output == output)
                {
                    target = Some((wallpaper_idx, layer_idx, wallpaper.is_shader()));
                    break;
                }
            }

            if let Some((wallpaper_idx, layer_idx, is_shader)) = target {
                let qh = self.qh.clone();
                let gpu = self.gpu_renderer.as_ref();
                let wallpaper = &mut self.wallpapers[wallpaper_idx];
                let layer = &mut wallpaper.layers[layer_idx];
                layer.output_info = output_info;
                layer.fractional_scale = Some(layer.output_info.scale_factor as u32 * 120);
                if is_shader {
                    if let Some(gpu) = gpu {
                        Self::update_shader_layer_surface(gpu, &qh, layer);
                    }
                } else {
                    wallpaper.draw();
                }
            }
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.active_outputs.retain(|o| o != &output);
        let Some(output_info) = self.output_state.info(&output) else {
            return;
        };

        // state cleanup
        if let Ok(state_helper) = State::state() {
            let mut state = State::get_entry(&state_helper).unwrap_or_default();
            state
                .wallpapers
                .retain(|(o_name, _source)| Some(o_name) != output_info.name.as_ref());
            if let Err(err) = state.write_entry(&state_helper) {
                error!("{err}");
            }
        }

        let Some(output_wallpaper) =
            self.wallpapers
                .iter_mut()
                .find(|w| match w.entry.output.as_str() {
                    "all" => true,
                    name => Some(name) == output_info.name.as_deref(),
                })
        else {
            return;
        };

        let Some(layer_position) = output_wallpaper
            .layers
            .iter()
            .position(|bg_layer| bg_layer.wl_output == output)
        else {
            return;
        };

        output_wallpaper.layers.remove(layer_position);
    }
}

impl LayerShellHandler for CosmicBg {
    fn closed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        dropped_layer: &LayerSurface,
    ) {
        for wallpaper in &mut self.wallpapers {
            wallpaper
                .layers
                .retain(|layer| &layer.layer != dropped_layer);
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let span = tracing::debug_span!("<CosmicBg as LayerShellHandler>::configure");
        let _handle = span.enter();

        let (w, h) = configure.new_size;

        // Find the wallpaper and layer index for this surface
        let mut found_info: Option<(usize, usize, bool, Option<glowberry_config::ShaderSource>)> =
            None;

        for (wp_idx, wallpaper) in self.wallpapers.iter_mut().enumerate() {
            if let Some(layer_idx) = wallpaper.layers.iter().position(|l| &l.layer == layer) {
                let is_shader = wallpaper.is_shader();
                let shader_source = wallpaper.shader_source().cloned();
                found_info = Some((wp_idx, layer_idx, is_shader, shader_source));

                // Update layer state
                let w_layer = &mut wallpaper.layers[layer_idx];
                w_layer.size = Some((w, h));
                w_layer.needs_redraw = true;
                break;
            }
        }

        let Some((wp_idx, layer_idx, is_shader, shader_source)) = found_info else {
            return;
        };

        if is_shader {
            // Initialize or update GPU state for shader wallpapers
            if let Some(shader_source) = shader_source {
                let w_layer = &mut self.wallpapers[wp_idx].layers[layer_idx];

                if w_layer.gpu_state.is_none() {
                    // Initialize GPU state
                    self.init_gpu_layer_internal(wp_idx, layer_idx, &shader_source);
                } else {
                    let qh = self.qh.clone();
                    if let Some(gpu) = self.gpu_renderer.as_ref() {
                        let layer = &mut self.wallpapers[wp_idx].layers[layer_idx];
                        Self::update_shader_layer_surface(gpu, &qh, layer);
                    }
                }
            }
        } else {
            // Static wallpaper - use SHM buffer pool
            let w_layer = &mut self.wallpapers[wp_idx].layers[layer_idx];

            if let Some(pool) = w_layer.pool.as_mut() {
                if let Err(why) = pool.resize(w as usize * h as usize * 4) {
                    tracing::error!(?why, "failed to resize pool");
                    return;
                }
            } else {
                match SlotPool::new(w as usize * h as usize * 4, &self.shm_state) {
                    Ok(pool) => {
                        w_layer.pool.replace(pool);
                    }
                    Err(why) => {
                        tracing::error!(?why, "failed to create pool");
                        return;
                    }
                }
            }

            self.wallpapers[wp_idx].draw();
        }
    }
}

impl ShmHandler for CosmicBg {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

delegate_compositor!(CosmicBg);
delegate_output!(CosmicBg);
delegate_shm!(CosmicBg);
delegate_layer!(CosmicBg);
delegate_registry!(CosmicBg);
delegate_noop!(CosmicBg: wp_viewporter::WpViewporter);
delegate_noop!(CosmicBg: wp_viewport::WpViewport);
delegate_noop!(CosmicBg: wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, Weak<wl_surface::WlSurface>>
    for CosmicBg
{
    fn event(
        state: &mut CosmicBg,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        surface: &Weak<wl_surface::WlSurface>,
        _: &Connection,
        _: &QueueHandle<CosmicBg>,
    ) {
        match event {
            wp_fractional_scale_v1::Event::PreferredScale { scale } => {
                if let Ok(surface) = surface.upgrade() {
                    let mut target: Option<(usize, usize, bool)> = None;
                    for (wallpaper_idx, wallpaper) in state.wallpapers.iter().enumerate() {
                        if let Some(layer_idx) = wallpaper
                            .layers
                            .iter()
                            .position(|layer| layer.layer.wl_surface() == &surface)
                        {
                            target = Some((wallpaper_idx, layer_idx, wallpaper.is_shader()));
                            break;
                        }
                    }

                    if let Some((wallpaper_idx, layer_idx, is_shader)) = target {
                        let qh = state.qh.clone();
                        let gpu = state.gpu_renderer.as_ref();
                        let wallpaper = &mut state.wallpapers[wallpaper_idx];
                        let layer = &mut wallpaper.layers[layer_idx];
                        layer.fractional_scale = Some(scale);
                        if is_shader {
                            if let Some(gpu) = gpu {
                                CosmicBg::update_shader_layer_surface(gpu, &qh, layer);
                            }
                        } else {
                            wallpaper.draw();
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
    }
}

impl ProvidesRegistryState for CosmicBg {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

// Implement AsToplevelTracker for CosmicBg to enable fullscreen detection
impl AsToplevelTracker for CosmicBg {
    fn as_toplevel_tracker(&self) -> Option<&ToplevelTracker> {
        self.toplevel_tracker.as_ref()
    }

    fn as_toplevel_tracker_mut(&mut self) -> &mut ToplevelTracker {
        self.toplevel_tracker
            .as_mut()
            .expect("toplevel tracker required for AsToplevelTracker::as_toplevel_tracker_mut")
    }

    fn on_toplevel_fullscreen_changed(&mut self) {
        // A toplevel's fullscreen state changed - we need to:
        // 1. Resume animation on outputs that no longer have fullscreen windows
        // 2. Log outputs that are now paused due to fullscreen windows
        //
        // Note: Outputs with fullscreen windows are effectively paused by the compositor
        // (it doesn't send frame callbacks for occluded surfaces), but we want to track
        // and log this state explicitly.

        let fullscreen_output_ids = self
            .toplevel_tracker
            .as_ref()
            .map(|t| t.get_fullscreen_output_ids())
            .unwrap_or_default();

        // Log outputs that are paused due to fullscreen (for visibility)
        if !fullscreen_output_ids.is_empty() {
            for wallpaper in &self.wallpapers {
                for layer in &wallpaper.layers {
                    if layer.gpu_state.is_some() {
                        let output_id = layer.wl_output.id().protocol_id();
                        if fullscreen_output_ids.contains(&output_id) {
                            let output_name =
                                layer.output_info.name.as_deref().unwrap_or("unknown");
                            tracing::info!(
                                output = output_name,
                                output_id,
                                reason = "FullscreenApp",
                                "Shader animation paused (fullscreen window detected)"
                            );
                        }
                    }
                }
            }
        }

        // Resume outputs that no longer have fullscreen windows
        self.request_frame_callbacks_if_needed();
    }
}

// Implement AsWorkspaceTracker for CosmicBg to enable workspace-aware fullscreen detection
impl AsWorkspaceTracker for CosmicBg {
    fn as_workspace_tracker(&self) -> Option<&WorkspaceTracker> {
        self.workspace_tracker.as_ref()
    }

    fn as_workspace_tracker_mut(&mut self) -> Option<&mut WorkspaceTracker> {
        self.workspace_tracker.as_mut()
    }

    fn on_workspace_active_changed(&mut self) {
        // A workspace's active state changed - this may affect which outputs have
        // "active" fullscreen windows. Re-evaluate fullscreen state.
        tracing::trace!("Workspace active state changed, re-evaluating fullscreen state");
        self.request_frame_callbacks_if_needed();
    }
}

// Dispatch implementation for ZcosmicToplevelInfoV1
// Delegates to ToplevelTracker which handles new toplevel creation
impl Dispatch<ZcosmicToplevelInfoV1, ()> for CosmicBg {
    fn event(
        state: &mut CosmicBg,
        _proxy: &ZcosmicToplevelInfoV1,
        event: <ZcosmicToplevelInfoV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<CosmicBg>,
    ) {
        use cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_info_v1::Event;
        match event {
            Event::Toplevel { toplevel } => {
                tracing::trace!(?toplevel, "New toplevel announced");
                // The toplevel handle will receive its own events via ZcosmicToplevelHandleV1 dispatch
            }
            Event::Done => {
                let count = state
                    .toplevel_tracker
                    .as_ref()
                    .map(|t| t.toplevels.len())
                    .unwrap_or(0);
                tracing::trace!(toplevel_count = count, "Received initial toplevel list");
            }
            Event::Finished => {
                tracing::info!("Toplevel info protocol finished");
            }
            _ => {}
        }
    }

    sctk::reexports::client::event_created_child!(CosmicBg, ZcosmicToplevelInfoV1, [
        cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_info_v1::EVT_TOPLEVEL_OPCODE => (ZcosmicToplevelHandleV1, ())
    ]);
}

// Dispatch implementation for ZcosmicToplevelHandleV1
// Handles cosmic-specific toplevel state changes (fullscreen, outputs, etc.)
impl Dispatch<ZcosmicToplevelHandleV1, ()> for CosmicBg {
    fn event(
        state: &mut CosmicBg,
        handle: &ZcosmicToplevelHandleV1,
        event: <ZcosmicToplevelHandleV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<CosmicBg>,
    ) {
        // Only process events if we have a toplevel tracker
        let Some(ref mut tracker) = state.toplevel_tracker else {
            tracing::warn!("Received toplevel handle event but no tracker available");
            return;
        };

        use cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1::Event;

        // Ensure toplevel exists in our list
        if !tracker.toplevels.iter().any(|(h, _)| h == handle) {
            tracing::trace!(?handle, "Adding new toplevel to tracker");
            tracker.add_toplevel(handle.clone());
        }

        match event {
            Event::AppId { ref app_id } => {
                tracing::trace!(?handle, app_id, "Toplevel app_id");
            }
            Event::Title { ref title } => {
                tracing::trace!(?handle, title, "Toplevel title");
            }
            Event::OutputEnter { ref output } => {
                let output_id = output.id().protocol_id();
                tracing::trace!(?handle, output_id, "Toplevel entered output");
                tracker.add_pending_output(handle, output_id);
            }
            Event::OutputLeave { ref output } => {
                let output_id = output.id().protocol_id();
                tracing::trace!(?handle, output_id, "Toplevel left output");
                tracker.remove_pending_output(handle, output_id);
            }
            Event::State {
                state: ref state_bytes,
            } => {
                use cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1::State;
                use std::collections::HashSet;

                let mut states = HashSet::new();
                for chunk in state_bytes.chunks_exact(4) {
                    if let Ok(bytes) = chunk.try_into() {
                        let value = u32::from_ne_bytes(bytes);
                        if let Ok(s) = State::try_from(value) {
                            states.insert(s);
                        }
                    }
                }
                let is_fullscreen = states.contains(&State::Fullscreen);
                tracing::trace!(?handle, ?states, is_fullscreen, "Toplevel state update");
                tracker.set_pending_state(handle, states);
            }
            Event::Done => {
                let changed = tracker.commit_toplevel(handle);
                tracing::trace!(?handle, changed, "Toplevel done event");
                if changed {
                    // Notify that fullscreen state may have changed
                    state.on_toplevel_fullscreen_changed();
                }
            }
            Event::Closed => {
                use cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1::State;

                let had_fullscreen = tracker
                    .toplevels
                    .iter()
                    .find(|(h, _)| h == handle)
                    .map(|(_, data)| data.state.contains(&State::Fullscreen))
                    .unwrap_or(false);

                tracing::trace!(?handle, had_fullscreen, "Toplevel closed");
                tracker.remove_toplevel(handle);

                if had_fullscreen {
                    state.on_toplevel_fullscreen_changed();
                }
            }
            Event::WorkspaceEnter { ref workspace } => {
                // Track workspace for this toplevel (deprecated API, use protocol ID)
                let workspace_id = workspace.id().protocol_id();
                tracing::debug!(
                    ?handle,
                    workspace_id,
                    "Toplevel entered workspace (deprecated)"
                );
                tracker.add_pending_workspace(handle, workspace_id);
            }
            Event::WorkspaceLeave { ref workspace } => {
                // Track workspace for this toplevel (deprecated API, use protocol ID)
                let workspace_id = workspace.id().protocol_id();
                tracing::debug!(
                    ?handle,
                    workspace_id,
                    "Toplevel left workspace (deprecated)"
                );
                tracker.remove_pending_workspace(handle, workspace_id);
            }
            Event::ExtWorkspaceEnter { ref workspace } => {
                // Track workspace for this toplevel (v3+ API)
                let workspace_id = workspace.id().protocol_id();
                tracing::trace!(?handle, workspace_id, "Toplevel entered workspace");
                tracker.add_pending_workspace(handle, workspace_id);
            }
            Event::ExtWorkspaceLeave { ref workspace } => {
                // Track workspace for this toplevel (v3+ API)
                let workspace_id = workspace.id().protocol_id();
                tracing::trace!(?handle, workspace_id, "Toplevel left workspace");
                tracker.remove_pending_workspace(handle, workspace_id);
            }
            _ => {}
        }
    }
}

// Dispatch implementation for ExtForeignToplevelListV1
// Handles the list of foreign toplevels (needed for cosmic protocol v2+)
impl Dispatch<ExtForeignToplevelListV1, ()> for CosmicBg {
    fn event(
        state: &mut CosmicBg,
        _proxy: &ExtForeignToplevelListV1,
        event: <ExtForeignToplevelListV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<CosmicBg>,
    ) {
        use sctk::reexports::protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::Event;

        match event {
            Event::Toplevel { toplevel } => {
                tracing::trace!(?toplevel, "Foreign toplevel announced");
                // For each foreign toplevel, request a cosmic toplevel handle
                if let Some(ref tracker) = state.toplevel_tracker {
                    let cosmic_handle = tracker.toplevel_info().get_cosmic_toplevel(
                        &toplevel,
                        qh,
                        toplevel.clone(),
                    );
                    tracing::debug!(
                        ?toplevel,
                        ?cosmic_handle,
                        "Requested cosmic toplevel handle"
                    );
                }
            }
            Event::Finished => {
                tracing::info!("Foreign toplevel list protocol finished");
            }
            _ => {}
        }
    }

    sctk::reexports::client::event_created_child!(CosmicBg, ExtForeignToplevelListV1, [
        sctk::reexports::protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ())
    ]);
}

// Dispatch implementation for ExtForeignToplevelHandleV1
// Handles events for individual foreign toplevel windows
impl Dispatch<ExtForeignToplevelHandleV1, ()> for CosmicBg {
    fn event(
        state: &mut CosmicBg,
        handle: &ExtForeignToplevelHandleV1,
        event: <ExtForeignToplevelHandleV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<CosmicBg>,
    ) {
        use sctk::reexports::protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::Event;

        match event {
            Event::Closed => {
                tracing::trace!(?handle, "Foreign toplevel closed");
                if let Some(ref mut tracker) = state.toplevel_tracker {
                    tracker.remove_foreign_toplevel(handle);
                }
            }
            Event::Done => {
                // For v2+ protocol, the Done event on ext_foreign_toplevel_handle_v1
                // signals that all state changes are complete. We need to commit
                // the corresponding cosmic toplevel's pending state.
                if let Some(ref mut tracker) = state.toplevel_tracker {
                    let changed = tracker.commit_foreign_toplevel(handle);
                    if changed {
                        tracing::debug!(
                            ?handle,
                            "Foreign toplevel done - fullscreen state changed"
                        );
                        state.on_toplevel_fullscreen_changed();
                    }
                }
            }
            Event::Title { ref title } => {
                tracing::trace!(?handle, title, "Foreign toplevel title");
            }
            Event::AppId { ref app_id } => {
                tracing::trace!(?handle, app_id, "Foreign toplevel app_id");
            }
            Event::Identifier { ref identifier } => {
                tracing::trace!(?handle, identifier, "Foreign toplevel identifier");
            }
            _ => {}
        }
    }
}

// Special dispatch for cosmic toplevel handles created via get_cosmic_toplevel
// The user data contains the foreign handle that was used to create it
impl Dispatch<ZcosmicToplevelHandleV1, ExtForeignToplevelHandleV1> for CosmicBg {
    fn event(
        state: &mut CosmicBg,
        handle: &ZcosmicToplevelHandleV1,
        event: <ZcosmicToplevelHandleV1 as Proxy>::Event,
        foreign_handle: &ExtForeignToplevelHandleV1,
        _conn: &Connection,
        _qh: &QueueHandle<CosmicBg>,
    ) {
        // Only process events if we have a toplevel tracker
        let Some(ref mut tracker) = state.toplevel_tracker else {
            return;
        };

        use cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1::Event;

        // On first event, register the mapping and add toplevel
        if !tracker.toplevels.iter().any(|(h, _)| h == handle) {
            tracing::debug!(
                ?handle,
                ?foreign_handle,
                "Registering cosmic toplevel from foreign handle"
            );
            tracker.register_cosmic_handle(foreign_handle.clone(), handle.clone());
            tracker.add_toplevel(handle.clone());
        }

        // Handle the same events as the () variant
        match event {
            Event::OutputEnter { ref output } => {
                let output_id = output.id().protocol_id();
                tracing::trace!(?handle, output_id, "Toplevel entered output");
                tracker.add_pending_output(handle, output_id);
            }
            Event::OutputLeave { ref output } => {
                let output_id = output.id().protocol_id();
                tracing::trace!(?handle, output_id, "Toplevel left output");
                tracker.remove_pending_output(handle, output_id);
            }
            Event::State {
                state: ref state_bytes,
            } => {
                use cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1::State;
                use std::collections::HashSet;

                let mut states = HashSet::new();
                for chunk in state_bytes.chunks_exact(4) {
                    if let Ok(bytes) = chunk.try_into() {
                        let value = u32::from_ne_bytes(bytes);
                        if let Ok(s) = State::try_from(value) {
                            states.insert(s);
                        }
                    }
                }
                let is_fullscreen = states.contains(&State::Fullscreen);
                tracing::trace!(?handle, ?states, is_fullscreen, "Toplevel state update");
                tracker.set_pending_state(handle, states);
            }
            Event::Done => {
                let changed = tracker.commit_toplevel(handle);
                tracing::trace!(?handle, changed, "Toplevel done event");
                if changed {
                    state.on_toplevel_fullscreen_changed();
                }
            }
            _ => {}
        }
    }
}

// Dispatch implementation for ExtWorkspaceManagerV1
impl Dispatch<ExtWorkspaceManagerV1, ()> for CosmicBg {
    fn event(
        state: &mut CosmicBg,
        _proxy: &ExtWorkspaceManagerV1,
        event: <ExtWorkspaceManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<CosmicBg>,
    ) {
        use sctk::reexports::protocols::ext::workspace::v1::client::ext_workspace_manager_v1::Event;

        match event {
            Event::WorkspaceGroup { workspace_group } => {
                tracing::trace!(?workspace_group, "New workspace group announced");
                if let Some(ref mut tracker) = state.workspace_tracker {
                    tracker.add_group(workspace_group);
                }
            }
            Event::Done => {
                // Commit all pending workspace state changes
                if let Some(ref mut tracker) = state.workspace_tracker {
                    // Commit all group states
                    let groups: Vec<_> = tracker.groups.iter().map(|(h, _)| h.clone()).collect();
                    for group in groups {
                        tracker.commit_group(&group);
                    }

                    // Commit all workspace states and check if any changed
                    let workspaces: Vec<_> = tracker.workspaces.keys().cloned().collect();
                    let any_changed = workspaces
                        .iter()
                        .map(|ws| tracker.commit_workspace(ws))
                        .any(|changed| changed);

                    if any_changed {
                        tracing::trace!("Workspace active state changed after manager done");
                        state.on_workspace_active_changed();
                    }
                }
                tracing::trace!("Workspace manager done event processed");
            }
            Event::Finished => {
                tracing::info!("Workspace manager protocol finished");
            }
            Event::Workspace { workspace } => {
                tracing::trace!(?workspace, "New workspace announced");
                // Workspace will be added to a group when we receive WorkspaceEnter on that group
            }
            _ => {}
        }
    }

    // Tell wayland-client how to dispatch events for child objects created by this protocol
    sctk::reexports::client::event_created_child!(CosmicBg, ExtWorkspaceManagerV1, [
        // workspace_group event (opcode 0) creates ExtWorkspaceGroupHandleV1
        0 => (ExtWorkspaceGroupHandleV1, ()),
        // workspace event (opcode 1) creates ExtWorkspaceHandleV1
        1 => (ExtWorkspaceHandleV1, ())
    ]);
}

// Dispatch implementation for ExtWorkspaceGroupHandleV1
impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for CosmicBg {
    fn event(
        state: &mut CosmicBg,
        group: &ExtWorkspaceGroupHandleV1,
        event: <ExtWorkspaceGroupHandleV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<CosmicBg>,
    ) {
        use sctk::reexports::protocols::ext::workspace::v1::client::ext_workspace_group_handle_v1::Event;

        let Some(ref mut tracker) = state.workspace_tracker else {
            return;
        };

        match event {
            Event::OutputEnter { output } => {
                let output_id = output.id().protocol_id();
                tracing::trace!(?group, output_id, "Workspace group entered output");
                tracker.add_pending_group_output(group, output_id);
            }
            Event::OutputLeave { output } => {
                let output_id = output.id().protocol_id();
                tracing::trace!(?group, output_id, "Workspace group left output");
                tracker.remove_pending_group_output(group, output_id);
            }
            Event::WorkspaceEnter { workspace } => {
                tracing::trace!(?group, ?workspace, "Workspace entered group");
                tracker.add_workspace_to_group(group, workspace);
            }
            Event::WorkspaceLeave { workspace } => {
                tracing::trace!(?group, ?workspace, "Workspace left group");
                tracker.remove_workspace_from_group(group, &workspace);
            }
            Event::Removed => {
                tracing::trace!(?group, "Workspace group removed");
                tracker.remove_group(group);
            }
            _ => {}
        }
    }
}

// Dispatch implementation for ExtWorkspaceHandleV1
impl Dispatch<ExtWorkspaceHandleV1, ()> for CosmicBg {
    fn event(
        state: &mut CosmicBg,
        workspace: &ExtWorkspaceHandleV1,
        event: <ExtWorkspaceHandleV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<CosmicBg>,
    ) {
        use sctk::reexports::protocols::ext::workspace::v1::client::ext_workspace_handle_v1::Event;

        let Some(ref mut tracker) = state.workspace_tracker else {
            return;
        };

        match event {
            Event::State { state: state_bits } => {
                use sctk::reexports::protocols::ext::workspace::v1::client::ext_workspace_handle_v1::State;

                // The state is a bitfield - check if Active bit is set
                let is_active = match state_bits {
                    sctk::reexports::client::WEnum::Value(bits) => bits.contains(State::Active),
                    sctk::reexports::client::WEnum::Unknown(_) => false,
                };
                tracing::trace!(?workspace, is_active, "Workspace state update");
                tracker.set_workspace_pending_active(workspace, is_active);
            }
            Event::Name { ref name } => {
                tracing::trace!(?workspace, name, "Workspace name");
            }
            Event::Removed => {
                tracing::trace!(?workspace, "Workspace removed");
                // Workspace removal is handled via group's WorkspaceLeave event
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CosmicBg;

    #[test]
    fn shader_physical_size_prefers_layer_size_over_mode() {
        let size = Some((100, 50));
        let scale = Some(150);
        let mode = Some((1920, 1080));

        let result = CosmicBg::shader_physical_size(size, scale, mode);

        assert_eq!(result, (125, 62));
    }

    #[test]
    fn shader_physical_size_uses_mode_when_size_missing() {
        let result = CosmicBg::shader_physical_size(None, Some(150), Some((1280, 720)));

        assert_eq!(result, (1280, 720));
    }

    #[test]
    fn shader_physical_size_defaults_scale_to_120() {
        let result = CosmicBg::shader_physical_size(Some((1200, 800)), None, Some((640, 480)));

        assert_eq!(result, (1200, 800));
    }
}
