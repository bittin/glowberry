// SPDX-License-Identifier: MPL-2.0

//! Workspace tracking for fullscreen detection using ext_workspace_v1 protocol.
//!
//! This module tracks workspace state to determine which workspace is active on each
//! output. Combined with toplevel workspace tracking, this allows us to only pause
//! shader animation when a fullscreen window is on the *active* workspace for an output.

use std::collections::{HashMap, HashSet};

use sctk::reexports::client::{globals::GlobalList, Dispatch, Proxy, QueueHandle};
use sctk::reexports::protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
    ext_workspace_handle_v1::ExtWorkspaceHandleV1, ext_workspace_manager_v1::ExtWorkspaceManagerV1,
};

/// Data tracked for each workspace.
#[derive(Debug, Default)]
pub struct WorkspaceData {
    /// Whether this workspace is currently active.
    pub is_active: bool,
    /// Pending active state (accumulated until Done event).
    pending_is_active: bool,
}

/// Data tracked for each workspace group.
#[derive(Debug, Default)]
pub struct WorkspaceGroupData {
    /// Output IDs associated with this workspace group.
    pub output_ids: HashSet<u32>,
    /// Pending output IDs (accumulated until Done event).
    pending_output_ids: HashSet<u32>,
    /// Workspace handles in this group.
    pub workspaces: Vec<ExtWorkspaceHandleV1>,
}

/// Tracks workspaces to determine which workspace is active on each output.
pub struct WorkspaceTracker {
    /// The workspace manager protocol.
    #[allow(dead_code)]
    manager: ExtWorkspaceManagerV1,
    /// Workspace groups (each group is associated with one or more outputs).
    pub groups: Vec<(ExtWorkspaceGroupHandleV1, WorkspaceGroupData)>,
    /// Individual workspace data.
    pub workspaces: HashMap<ExtWorkspaceHandleV1, WorkspaceData>,
}

impl std::fmt::Debug for WorkspaceTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceTracker")
            .field("groups_count", &self.groups.len())
            .field("workspaces_count", &self.workspaces.len())
            .finish()
    }
}

impl WorkspaceTracker {
    /// Try to bind to the workspace manager protocol.
    /// Returns None if the protocol is not available.
    pub fn try_new<D>(globals: &GlobalList, qh: &QueueHandle<D>) -> Option<Self>
    where
        D: Dispatch<ExtWorkspaceManagerV1, ()>
            + Dispatch<ExtWorkspaceGroupHandleV1, ()>
            + Dispatch<ExtWorkspaceHandleV1, ()>
            + 'static,
    {
        let manager = globals
            .bind::<ExtWorkspaceManagerV1, _, _>(qh, 1..=1, ())
            .ok()?;

        tracing::info!("Bound ext_workspace_manager_v1 protocol");

        Some(Self {
            manager,
            groups: Vec::new(),
            workspaces: HashMap::new(),
        })
    }

    /// Add a new workspace group.
    pub fn add_group(&mut self, handle: ExtWorkspaceGroupHandleV1) {
        tracing::trace!(?handle, "New workspace group added");
        self.groups.push((handle, WorkspaceGroupData::default()));
    }

    /// Remove a workspace group.
    pub fn remove_group(&mut self, handle: &ExtWorkspaceGroupHandleV1) {
        if let Some(idx) = self.groups.iter().position(|(h, _)| h == handle) {
            let (_, data) = self.groups.remove(idx);
            // Remove associated workspaces
            for ws in data.workspaces {
                self.workspaces.remove(&ws);
            }
            tracing::trace!(?handle, "Workspace group removed");
        }
    }

    /// Get mutable group data.
    fn get_group_data_mut(
        &mut self,
        handle: &ExtWorkspaceGroupHandleV1,
    ) -> Option<&mut WorkspaceGroupData> {
        self.groups
            .iter_mut()
            .find(|(h, _)| h == handle)
            .map(|(_, data)| data)
    }

    /// Add output to a workspace group (pending).
    pub fn add_pending_group_output(&mut self, group: &ExtWorkspaceGroupHandleV1, output_id: u32) {
        if let Some(data) = self.get_group_data_mut(group) {
            data.pending_output_ids.insert(output_id);
        }
    }

    /// Remove output from a workspace group (pending).
    pub fn remove_pending_group_output(
        &mut self,
        group: &ExtWorkspaceGroupHandleV1,
        output_id: u32,
    ) {
        if let Some(data) = self.get_group_data_mut(group) {
            data.pending_output_ids.remove(&output_id);
        }
    }

    /// Add workspace to a group.
    pub fn add_workspace_to_group(
        &mut self,
        group: &ExtWorkspaceGroupHandleV1,
        workspace: ExtWorkspaceHandleV1,
    ) {
        if let Some(data) = self.get_group_data_mut(group) {
            data.workspaces.push(workspace.clone());
        }
        self.workspaces
            .entry(workspace)
            .or_insert_with(WorkspaceData::default);
    }

    /// Remove workspace from a group.
    pub fn remove_workspace_from_group(
        &mut self,
        group: &ExtWorkspaceGroupHandleV1,
        workspace: &ExtWorkspaceHandleV1,
    ) {
        if let Some(data) = self.get_group_data_mut(group) {
            data.workspaces.retain(|w| w != workspace);
        }
        self.workspaces.remove(workspace);
    }

    /// Set pending active state for a workspace.
    pub fn set_workspace_pending_active(&mut self, workspace: &ExtWorkspaceHandleV1, active: bool) {
        if let Some(data) = self.workspaces.get_mut(workspace) {
            data.pending_is_active = active;
        }
    }

    /// Commit a workspace group's pending state.
    pub fn commit_group(&mut self, group: &ExtWorkspaceGroupHandleV1) {
        if let Some(data) = self.get_group_data_mut(group) {
            data.output_ids = data.pending_output_ids.clone();
        }
    }

    /// Commit a workspace's pending state.
    /// Returns true if the active state changed.
    pub fn commit_workspace(&mut self, workspace: &ExtWorkspaceHandleV1) -> bool {
        if let Some(data) = self.workspaces.get_mut(workspace) {
            let was_active = data.is_active;
            data.is_active = data.pending_is_active;
            was_active != data.is_active
        } else {
            false
        }
    }

    /// Get all active workspace IDs for a given output.
    ///
    /// Returns the protocol IDs of workspaces that are active on the given output.
    pub fn get_active_workspace_ids_for_output(&self, output_id: u32) -> HashSet<u32> {
        let mut active_ids = HashSet::new();

        // Find groups that contain this output
        for (_, group_data) in &self.groups {
            if group_data.output_ids.contains(&output_id) {
                // Find active workspaces in this group
                for ws in &group_data.workspaces {
                    if let Some(ws_data) = self.workspaces.get(ws) {
                        if ws_data.is_active {
                            active_ids.insert(ws.id().protocol_id());
                        }
                    }
                }
            }
        }

        active_ids
    }

    /// Check if a workspace is active on a given output.
    pub fn is_workspace_active_on_output(&self, workspace_id: u32, output_id: u32) -> bool {
        let active_ids = self.get_active_workspace_ids_for_output(output_id);
        active_ids.contains(&workspace_id)
    }
}

/// Trait that must be implemented by the main state to provide access to WorkspaceTracker.
pub trait AsWorkspaceTracker {
    fn as_workspace_tracker(&self) -> Option<&WorkspaceTracker>;
    fn as_workspace_tracker_mut(&mut self) -> Option<&mut WorkspaceTracker>;

    /// Called when a workspace's active state changes.
    fn on_workspace_active_changed(&mut self);
}
