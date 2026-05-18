//! Mapping between Linear-native workflow states and the normalized
//! [`SubjectStatus`] enum the Animus daemon dispatches on.
//!
//! Linear models workflow state as a per-team list of named states (e.g.
//! `"Backlog"`, `"Todo"`, `"In Progress"`, `"In Review"`, `"Done"`,
//! `"Cancelled"`, plus custom ones each team may add). The Animus protocol
//! collapses all of those into five buckets: [`SubjectStatus::Ready`],
//! [`SubjectStatus::InProgress`], [`SubjectStatus::Blocked`],
//! [`SubjectStatus::Done`], [`SubjectStatus::Cancelled`].
//!
//! # Strategy
//!
//! Instead of hardcoding the name list (which only matches Linear's default
//! team template), [`StatusMap::from_workflow_states`] consumes the team's
//! actual workflow states (queried from Linear at backend init) and maps
//! each one to a [`SubjectStatus`] using two signals:
//!
//! 1. Linear's `WorkflowState.type` field (`"triage"`, `"backlog"`,
//!    `"unstarted"`, `"started"`, `"completed"`, `"cancelled"`). This is
//!    robust to renaming because the type is fixed even when teams
//!    rename the state.
//! 2. A user-supplied override (via the `LINEAR_STATUS_MAP` env var)
//!    keyed by Linear state name. Overrides win over type-based mapping.
//!
//! When several Linear states map to the same animus status (e.g. both
//! `"Spec"` and `"Backlog"` map to [`SubjectStatus::Ready`]) we pick the
//! one with the lowest `position` for the reverse direction (write path).
//! Linear orders workflow states by `position` ascending, so the lowest
//! position is the team's default "first" state in that category.

use std::collections::HashMap;

use animus_subject_protocol::SubjectStatus;

use crate::client::LinearWorkflowState;

/// A best-effort fallback list of Linear-native status names, surfaced from
/// [`SubjectSchema::native_status_values`](animus_subject_protocol::SubjectSchema)
/// when no runtime-discovered map is available yet. The actual list returned
/// to callers after the first GraphQL call comes from
/// [`StatusMap::native_state_names`].
pub const FALLBACK_NATIVE_STATUSES: &[&str] = &[
    "Backlog",
    "Triage",
    "Todo",
    "In Progress",
    "In Review",
    "Blocked",
    "Done",
    "Cancelled",
    "Duplicate",
];

/// Runtime-discovered, override-aware bidirectional map between Linear
/// workflow states and [`SubjectStatus`].
#[derive(Debug, Clone, Default)]
pub struct StatusMap {
    /// Linear state UUID -> (state name, animus_status).
    by_state_id: HashMap<String, (String, SubjectStatus)>,
    /// Animus [`SubjectStatus`] -> Linear state UUID. When multiple states
    /// map to the same animus status, the one with the lowest `position`
    /// wins (Linear's default "first" state in that category).
    by_animus_status: HashMap<SubjectStatus, String>,
    /// Native state names in the team's workflow, in the order Linear
    /// returns them. Exposed by `schema()`.
    native_names: Vec<String>,
}

impl StatusMap {
    /// Build a [`StatusMap`] from a team's workflow states plus optional
    /// user overrides keyed by Linear state name.
    ///
    /// Resolution order, per state:
    ///
    /// 1. Override from `overrides` if the state's `name` is present.
    /// 2. Type-based mapping from [`type_to_animus`].
    ///
    /// For the reverse direction (`animus_to_linear_state_id`), when several
    /// states map to the same animus status we pick the state with the
    /// lowest `position` (Linear's default ordering).
    pub fn from_workflow_states(
        states: &[LinearWorkflowState],
        overrides: &HashMap<String, SubjectStatus>,
    ) -> Self {
        let mut by_state_id: HashMap<String, (String, SubjectStatus)> = HashMap::new();
        // Track (state_id, position) per animus status so we can pick the
        // lowest-position state at the end.
        let mut candidates: HashMap<SubjectStatus, (String, f64)> = HashMap::new();
        let mut native_names = Vec::with_capacity(states.len());

        for state in states {
            native_names.push(state.name.clone());

            let animus_status = overrides
                .get(&state.name)
                .copied()
                .unwrap_or_else(|| type_to_animus(&state.state_type));

            by_state_id.insert(state.id.clone(), (state.name.clone(), animus_status));

            candidates
                .entry(animus_status)
                .and_modify(|existing| {
                    if state.position < existing.1 {
                        *existing = (state.id.clone(), state.position);
                    }
                })
                .or_insert_with(|| (state.id.clone(), state.position));
        }

        let by_animus_status = candidates
            .into_iter()
            .map(|(status, (state_id, _pos))| (status, state_id))
            .collect();

        Self {
            by_state_id,
            by_animus_status,
            native_names,
        }
    }

    /// Translate a Linear `WorkflowState.id` (UUID) to a [`SubjectStatus`].
    /// Returns `None` if the id wasn't present in the team's workflow at
    /// init time.
    pub fn linear_to_animus(&self, state_id: &str) -> Option<SubjectStatus> {
        self.by_state_id.get(state_id).map(|(_, s)| *s)
    }

    /// Translate a Linear `WorkflowState.name` to a [`SubjectStatus`].
    /// Provided for the read path where the GraphQL `Issue` node carries
    /// `state.name` (not the id). Matching is case-sensitive — Linear's
    /// state names are case-stable within a team.
    pub fn linear_name_to_animus(&self, state_name: &str) -> Option<SubjectStatus> {
        self.by_state_id.values().find_map(|(name, status)| {
            if name == state_name {
                Some(*status)
            } else {
                None
            }
        })
    }

    /// Translate a [`SubjectStatus`] to a Linear `WorkflowState.id` (UUID)
    /// suitable for `issueUpdate(input: { stateId: ... })`. Returns `None`
    /// if no state in the team's workflow maps to `status`.
    pub fn animus_to_linear_state_id(&self, status: SubjectStatus) -> Option<&str> {
        self.by_animus_status.get(&status).map(String::as_str)
    }

    /// All native state names discovered in the team's workflow.
    pub fn native_state_names(&self) -> &[String] {
        &self.native_names
    }

    /// Iterate over every `(state_id, state_name, animus_status)` tuple known
    /// to the map. Order is unspecified.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, SubjectStatus)> {
        self.by_state_id
            .iter()
            .map(|(id, (name, status))| (id.as_str(), name.as_str(), *status))
    }
}

/// Map a Linear `WorkflowState.type` literal to a [`SubjectStatus`].
///
/// Linear's documented set is `triage`, `backlog`, `unstarted`, `started`,
/// `completed`, `cancelled` (note the British spelling on the wire — Linear's
/// SDL uses `cancelled`). Anything else falls back to [`SubjectStatus::Ready`]
/// so workflows don't grind to a halt on a future Linear-side addition we
/// haven't seen yet.
pub fn type_to_animus(state_type: &str) -> SubjectStatus {
    match state_type {
        "triage" | "backlog" | "unstarted" => SubjectStatus::Ready,
        "started" => SubjectStatus::InProgress,
        "completed" => SubjectStatus::Done,
        "cancelled" | "canceled" => SubjectStatus::Cancelled,
        _ => SubjectStatus::Ready,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(id: &str, name: &str, state_type: &str, position: f64) -> LinearWorkflowState {
        LinearWorkflowState {
            id: id.to_string(),
            name: name.to_string(),
            state_type: state_type.to_string(),
            position,
        }
    }

    #[test]
    fn type_maps_to_expected_buckets() {
        assert_eq!(type_to_animus("triage"), SubjectStatus::Ready);
        assert_eq!(type_to_animus("backlog"), SubjectStatus::Ready);
        assert_eq!(type_to_animus("unstarted"), SubjectStatus::Ready);
        assert_eq!(type_to_animus("started"), SubjectStatus::InProgress);
        assert_eq!(type_to_animus("completed"), SubjectStatus::Done);
        assert_eq!(type_to_animus("cancelled"), SubjectStatus::Cancelled);
        assert_eq!(type_to_animus("unknown-future-type"), SubjectStatus::Ready);
    }

    #[test]
    fn from_workflow_states_builds_both_directions() {
        let states = vec![
            state("uuid-spec", "Spec", "backlog", 1.0),
            state("uuid-prog", "In Progress", "started", 2.0),
            state("uuid-done", "Shipped", "completed", 3.0),
            state("uuid-canc", "Won't Do", "cancelled", 4.0),
        ];
        let overrides = HashMap::new();
        let map = StatusMap::from_workflow_states(&states, &overrides);

        assert_eq!(
            map.linear_to_animus("uuid-spec"),
            Some(SubjectStatus::Ready)
        );
        assert_eq!(
            map.linear_to_animus("uuid-prog"),
            Some(SubjectStatus::InProgress)
        );
        assert_eq!(map.linear_to_animus("uuid-done"), Some(SubjectStatus::Done));
        assert_eq!(
            map.linear_to_animus("uuid-canc"),
            Some(SubjectStatus::Cancelled)
        );

        assert_eq!(
            map.animus_to_linear_state_id(SubjectStatus::Ready),
            Some("uuid-spec")
        );
        assert_eq!(
            map.animus_to_linear_state_id(SubjectStatus::InProgress),
            Some("uuid-prog")
        );
        assert_eq!(
            map.animus_to_linear_state_id(SubjectStatus::Done),
            Some("uuid-done")
        );
        assert_eq!(
            map.animus_to_linear_state_id(SubjectStatus::Cancelled),
            Some("uuid-canc")
        );

        assert_eq!(map.native_state_names().len(), 4);
        assert!(map.native_state_names().iter().any(|n| n == "Spec"));
    }

    #[test]
    fn override_wins_over_type_based_mapping() {
        let states = vec![
            state("uuid-spec", "Spec", "backlog", 1.0),
            state("uuid-impl", "Implementation", "started", 2.0),
        ];
        let mut overrides = HashMap::new();
        // Force "Spec" -> InProgress instead of the default Ready.
        overrides.insert("Spec".to_string(), SubjectStatus::InProgress);
        let map = StatusMap::from_workflow_states(&states, &overrides);

        assert_eq!(
            map.linear_to_animus("uuid-spec"),
            Some(SubjectStatus::InProgress)
        );
        // Two states now both map to InProgress; the lower-position one
        // wins for the reverse lookup.
        assert_eq!(
            map.animus_to_linear_state_id(SubjectStatus::InProgress),
            Some("uuid-spec")
        );
    }

    #[test]
    fn ambiguous_status_resolves_to_lowest_position() {
        let states = vec![
            // Two "Ready" buckets — position 5.0 should NOT win.
            state("uuid-backlog", "Backlog", "backlog", 5.0),
            state("uuid-todo", "Todo", "unstarted", 1.0),
            state("uuid-prog", "In Progress", "started", 2.0),
        ];
        let map = StatusMap::from_workflow_states(&states, &HashMap::new());
        assert_eq!(
            map.animus_to_linear_state_id(SubjectStatus::Ready),
            Some("uuid-todo"),
            "lowest-position Ready candidate must win"
        );
    }

    #[test]
    fn unmapped_status_returns_none_for_reverse_lookup() {
        // Workflow has no cancelled state. SubjectStatus::Cancelled should
        // have no reverse mapping.
        let states = vec![
            state("uuid-todo", "Todo", "unstarted", 1.0),
            state("uuid-prog", "In Progress", "started", 2.0),
            state("uuid-done", "Done", "completed", 3.0),
        ];
        let map = StatusMap::from_workflow_states(&states, &HashMap::new());
        assert!(map
            .animus_to_linear_state_id(SubjectStatus::Cancelled)
            .is_none());
    }

    #[test]
    fn linear_name_to_animus_matches_by_state_name() {
        let states = vec![
            state("uuid-spec", "Spec", "backlog", 1.0),
            state("uuid-prog", "Implementation", "started", 2.0),
        ];
        let map = StatusMap::from_workflow_states(&states, &HashMap::new());
        assert_eq!(
            map.linear_name_to_animus("Spec"),
            Some(SubjectStatus::Ready)
        );
        assert_eq!(
            map.linear_name_to_animus("Implementation"),
            Some(SubjectStatus::InProgress)
        );
        assert!(map.linear_name_to_animus("Nonexistent").is_none());
    }
}
