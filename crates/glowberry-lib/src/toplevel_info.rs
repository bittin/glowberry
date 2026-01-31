// SPDX-License-Identifier: MPL-2.0

//! Toplevel tracking for fullscreen detection using zcosmic_toplevel_info_v1 protocol.
//!
//! This module monitors toplevel windows and tracks which outputs have fullscreen
//! windows covering them. When a fullscreen window is detected on an output,
//! the shader animation on that output can be paused to save resources.
//!
//! For protocol version 2+, we use ext_foreign_toplevel_list_v1 to get the list of
//! toplevels, then call get_cosmic_toplevel to get the cosmic-specific handles.

use std::collections::{HashMap, HashSet};

use cosmic_protocols::toplevel_info::v1::client::{
    zcosmic_toplevel_handle_v1::State, zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
    zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1,
};
use sctk::reexports::client::{globals::GlobalList, Dispatch, Proxy, QueueHandle};
use sctk::reexports::protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
};

/// Data tracked for each toplevel window.
#[derive(Debug, Default)]
pub struct ToplevelData {
    /// Pending state (accumulated until Done event)
    pending_state: HashSet<State>,
    /// Committed state
    pub state: HashSet<State>,
    /// Output Wayland object IDs this toplevel is visible on (pending).
    /// We store the raw object ID (u32) because WlOutput proxy objects from different
    /// sources (OutputHandler vs toplevel OutputEnter) have different object IDs
    /// even when representing the same physical output.
    pending_output_ids: HashSet<u32>,
    /// Output Wayland object IDs this toplevel is visible on (committed).
    output_ids: HashSet<u32>,
    /// Workspace Wayland object IDs this toplevel is on (pending).
    /// We store the raw object ID (u32) to handle both deprecated ZcosmicWorkspaceHandleV1
    /// and newer ExtWorkspaceHandleV1.
    pending_workspace_ids: HashSet<u32>,
    /// Workspace Wayland object IDs this toplevel is on (committed).
    workspace_ids: HashSet<u32>,
}

/// Tracks toplevel windows for fullscreen detection.
///
/// This tracker binds to the `zcosmic_toplevel_info_v1` protocol and monitors
/// all toplevel windows. It can be queried to check if any fullscreen window
/// is present on a specific output.
///
/// For protocol version 2+, we also need `ext_foreign_toplevel_list_v1` to get
/// the list of toplevels, then we call `get_cosmic_toplevel` for each.
pub struct ToplevelTracker {
    /// The cosmic toplevel info protocol (for getting cosmic-specific handles)
    toplevel_info: ZcosmicToplevelInfoV1,
    /// The foreign toplevel list protocol (for getting the list of toplevels in v2+)
    #[allow(dead_code)]
    foreign_toplevel_list: Option<ExtForeignToplevelListV1>,
    /// Mapping from foreign toplevel handle to cosmic toplevel handle
    foreign_to_cosmic: HashMap<ExtForeignToplevelHandleV1, ZcosmicToplevelHandleV1>,
    /// The tracked toplevels with their state
    pub toplevels: Vec<(ZcosmicToplevelHandleV1, ToplevelData)>,
}

impl std::fmt::Debug for ToplevelTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToplevelTracker")
            .field("toplevels_count", &self.toplevels.len())
            .field("protocol_version", &self.toplevel_info.version())
            .finish()
    }
}

impl ToplevelTracker {
    /// Try to bind to the toplevel info protocol.
    /// Returns None if the protocol is not available (not running under COSMIC).
    pub fn try_new<D>(globals: &GlobalList, qh: &QueueHandle<D>) -> Option<Self>
    where
        D: Dispatch<ZcosmicToplevelInfoV1, ()>
            + Dispatch<ZcosmicToplevelHandleV1, ()>
            + Dispatch<ExtForeignToplevelListV1, ()>
            + Dispatch<ExtForeignToplevelHandleV1, ()>
            + 'static,
    {
        // First try to bind the cosmic toplevel info protocol
        let toplevel_info = globals
            .bind::<ZcosmicToplevelInfoV1, _, _>(qh, 1..=3, ())
            .ok()?;

        let version = toplevel_info.version();
        tracing::info!(version, "Bound zcosmic_toplevel_info_v1 protocol");

        // For version 2+, we need ext_foreign_toplevel_list_v1 to get toplevels
        let foreign_toplevel_list = if version >= 2 {
            match globals.bind::<ExtForeignToplevelListV1, _, _>(qh, 1..=1, ()) {
                Ok(list) => {
                    tracing::info!("Bound ext_foreign_toplevel_list_v1 protocol for v2+ support");
                    Some(list)
                }
                Err(e) => {
                    tracing::warn!(
                        ?e,
                        "Failed to bind ext_foreign_toplevel_list_v1, fullscreen detection may not work"
                    );
                    None
                }
            }
        } else {
            None
        };

        Some(Self {
            toplevel_info,
            foreign_toplevel_list,
            foreign_to_cosmic: HashMap::new(),
            toplevels: Vec::new(),
        })
    }

    /// Get the cosmic toplevel info protocol for making requests.
    pub fn toplevel_info(&self) -> &ZcosmicToplevelInfoV1 {
        &self.toplevel_info
    }

    /// Register a cosmic toplevel handle for a foreign toplevel.
    pub fn register_cosmic_handle(
        &mut self,
        foreign: ExtForeignToplevelHandleV1,
        cosmic: ZcosmicToplevelHandleV1,
    ) {
        tracing::debug!(
            ?foreign,
            ?cosmic,
            "Registered cosmic handle for foreign toplevel"
        );
        self.foreign_to_cosmic.insert(foreign, cosmic);
    }

    /// Remove a foreign toplevel mapping.
    pub fn remove_foreign_toplevel(&mut self, foreign: &ExtForeignToplevelHandleV1) {
        if let Some(cosmic) = self.foreign_to_cosmic.remove(foreign) {
            self.remove_toplevel(&cosmic);
        }
    }

    /// Get the cosmic handle for a foreign handle (if registered).
    pub fn get_cosmic_for_foreign(
        &self,
        foreign: &ExtForeignToplevelHandleV1,
    ) -> Option<&ZcosmicToplevelHandleV1> {
        self.foreign_to_cosmic.get(foreign)
    }

    /// Commit a toplevel's pending state and return whether fullscreen state changed.
    /// This is called when the foreign toplevel's Done event is received (for v2+ protocol).
    pub fn commit_foreign_toplevel(&mut self, foreign: &ExtForeignToplevelHandleV1) -> bool {
        if let Some(cosmic) = self.foreign_to_cosmic.get(foreign).cloned() {
            self.commit_toplevel(&cosmic)
        } else {
            false
        }
    }

    /// Check if any fullscreen window is covering the given output.
    ///
    /// Takes the raw Wayland object ID from the output protocol (the numeric part of wl_output@N).
    pub fn has_fullscreen_on_output_id(&self, output_id: u32) -> bool {
        let result = self.toplevels.iter().any(|(_, data)| {
            let is_fullscreen = data.state.contains(&State::Fullscreen);
            let on_output = data.output_ids.contains(&output_id);
            if is_fullscreen {
                tracing::trace!(
                    is_fullscreen,
                    on_output,
                    output_count = data.output_ids.len(),
                    checking_output_id = output_id,
                    toplevel_output_ids = ?data.output_ids,
                    "Checking fullscreen toplevel"
                );
            }
            is_fullscreen && on_output
        });
        result
    }

    /// Get all output IDs that currently have fullscreen windows on them.
    ///
    /// Returns a HashSet of raw Wayland object IDs for outputs with fullscreen coverage.
    pub fn get_fullscreen_output_ids(&self) -> HashSet<u32> {
        self.toplevels
            .iter()
            .filter(|(_, data)| data.state.contains(&State::Fullscreen))
            .flat_map(|(_, data)| data.output_ids.iter().copied())
            .collect()
    }

    /// Check if any fullscreen window on the given output is on one of the active workspaces.
    ///
    /// This combines fullscreen detection with workspace awareness - a fullscreen window
    /// only "counts" if it's on a workspace that is currently active on that output.
    ///
    /// `active_workspace_ids` should contain the IDs of workspaces that are currently active
    /// on the output being checked.
    pub fn has_active_fullscreen_on_output_id(
        &self,
        output_id: u32,
        active_workspace_ids: &HashSet<u32>,
    ) -> bool {
        // If we have no active workspace info, don't pause (can't determine)
        if active_workspace_ids.is_empty() {
            return false;
        }

        self.toplevels.iter().any(|(_, data)| {
            let is_fullscreen = data.state.contains(&State::Fullscreen);
            let on_output = data.output_ids.contains(&output_id);

            // If toplevel has no workspace info, assume NOT on active workspace
            if data.workspace_ids.is_empty() {
                return false;
            }

            // Check if the toplevel is on any of the active workspaces
            let on_active_workspace = data
                .workspace_ids
                .iter()
                .any(|ws_id| active_workspace_ids.contains(ws_id));

            is_fullscreen && on_output && on_active_workspace
        })
    }

    /// Get the number of tracked toplevels (for debugging).
    #[allow(dead_code)]
    pub fn toplevel_count(&self) -> usize {
        self.toplevels.len()
    }

    /// Internal: add new toplevel
    pub(crate) fn add_toplevel(&mut self, handle: ZcosmicToplevelHandleV1) {
        tracing::trace!(?handle, "New toplevel added");
        self.toplevels.push((handle, ToplevelData::default()));
    }

    /// Internal: remove toplevel
    pub(crate) fn remove_toplevel(&mut self, handle: &ZcosmicToplevelHandleV1) {
        if let Some(idx) = self.toplevels.iter().position(|(h, _)| h == handle) {
            tracing::trace!(?handle, "Toplevel removed");
            self.toplevels.remove(idx);
        }
    }

    /// Internal: get mutable toplevel data
    pub(crate) fn get_data_mut(
        &mut self,
        handle: &ZcosmicToplevelHandleV1,
    ) -> Option<&mut ToplevelData> {
        self.toplevels
            .iter_mut()
            .find(|(h, _)| h == handle)
            .map(|(_, data)| data)
    }

    /// Internal: update pending state
    pub(crate) fn set_pending_state(
        &mut self,
        handle: &ZcosmicToplevelHandleV1,
        state: HashSet<State>,
    ) {
        if let Some(data) = self.get_data_mut(handle) {
            data.pending_state = state;
        }
    }

    /// Internal: add output to pending outputs (by raw object ID)
    pub(crate) fn add_pending_output(&mut self, handle: &ZcosmicToplevelHandleV1, output_id: u32) {
        if let Some(data) = self.get_data_mut(handle) {
            data.pending_output_ids.insert(output_id);
        }
    }

    /// Internal: remove output from pending outputs (by raw object ID)
    pub(crate) fn remove_pending_output(
        &mut self,
        handle: &ZcosmicToplevelHandleV1,
        output_id: u32,
    ) {
        if let Some(data) = self.get_data_mut(handle) {
            data.pending_output_ids.remove(&output_id);
        }
    }

    /// Internal: add workspace to pending workspaces (by raw object ID)
    pub(crate) fn add_pending_workspace(
        &mut self,
        handle: &ZcosmicToplevelHandleV1,
        workspace_id: u32,
    ) {
        if let Some(data) = self.get_data_mut(handle) {
            data.pending_workspace_ids.insert(workspace_id);
        }
    }

    /// Internal: remove workspace from pending workspaces (by raw object ID)
    pub(crate) fn remove_pending_workspace(
        &mut self,
        handle: &ZcosmicToplevelHandleV1,
        workspace_id: u32,
    ) {
        if let Some(data) = self.get_data_mut(handle) {
            data.pending_workspace_ids.remove(&workspace_id);
        }
    }

    /// Internal: commit pending state (called on Done event)
    /// Returns true if the fullscreen state changed for any output.
    pub(crate) fn commit_toplevel(&mut self, handle: &ZcosmicToplevelHandleV1) -> bool {
        if let Some(data) = self.get_data_mut(handle) {
            let was_fullscreen = data.state.contains(&State::Fullscreen);
            let old_output_ids = data.output_ids.clone();
            let old_workspace_ids = data.workspace_ids.clone();

            data.state = data.pending_state.clone();
            data.output_ids = data.pending_output_ids.clone();
            data.workspace_ids = data.pending_workspace_ids.clone();

            let is_fullscreen = data.state.contains(&State::Fullscreen);

            // Check if fullscreen state, outputs, or workspaces changed
            let changed = was_fullscreen != is_fullscreen
                || old_output_ids != data.output_ids
                || old_workspace_ids != data.workspace_ids;

            if changed {
                tracing::debug!(
                    ?handle,
                    was_fullscreen,
                    is_fullscreen,
                    output_ids = ?data.output_ids,
                    workspace_ids = ?data.workspace_ids,
                    "Toplevel fullscreen state changed"
                );
            }

            changed
        } else {
            false
        }
    }
}

/// Trait that must be implemented by the main state to provide access to ToplevelTracker.
pub trait AsToplevelTracker {
    fn as_toplevel_tracker(&self) -> Option<&ToplevelTracker>;
    fn as_toplevel_tracker_mut(&mut self) -> &mut ToplevelTracker;

    /// Called when a toplevel's fullscreen state changes.
    /// This can be used to resume animation if a fullscreen window exits fullscreen.
    fn on_toplevel_fullscreen_changed(&mut self);
}
