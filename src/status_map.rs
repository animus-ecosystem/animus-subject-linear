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
//! For v0.1.0 the mapping is hard-coded against Linear's default workflow
//! template. v0.2 will allow workflow YAML to override it (see the README
//! example).

use animus_subject_protocol::SubjectStatus;

/// All Linear-native status strings the v0.1.0 mapping recognizes. Listed in
/// `SubjectSchema::native_status_values` so workflow authors can see what's
/// available without poking at Linear directly.
pub const KNOWN_NATIVE_STATUSES: &[&str] = &[
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

/// Translate a Linear-native status string to a normalized [`SubjectStatus`].
///
/// Unknown values fall back to [`SubjectStatus::Ready`]. Matching is
/// case-insensitive because Linear's workflow editor lets teams rename states
/// to anything they want.
pub fn linear_to_animus(native: &str) -> SubjectStatus {
    let normalized = native.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "backlog" | "triage" | "todo" => SubjectStatus::Ready,
        "in progress" | "started" | "in review" => SubjectStatus::InProgress,
        "blocked" | "on hold" => SubjectStatus::Blocked,
        "done" | "completed" => SubjectStatus::Done,
        "cancelled" | "canceled" | "duplicate" => SubjectStatus::Cancelled,
        _ => SubjectStatus::Ready,
    }
}

/// Translate a normalized [`SubjectStatus`] back to a Linear-native status
/// string. The returned value is the *canonical* native label Animus will use
/// when writing an update; alternative spellings (e.g. `"Canceled"`) are
/// recognized on read but not emitted on write.
pub fn animus_to_linear(status: SubjectStatus) -> &'static str {
    match status {
        SubjectStatus::Ready => "Todo",
        SubjectStatus::InProgress => "In Progress",
        SubjectStatus::Blocked => "Blocked",
        SubjectStatus::Done => "Done",
        SubjectStatus::Cancelled => "Cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_natives_map_to_expected_buckets() {
        assert_eq!(linear_to_animus("Backlog"), SubjectStatus::Ready);
        assert_eq!(linear_to_animus("Todo"), SubjectStatus::Ready);
        assert_eq!(linear_to_animus("In Progress"), SubjectStatus::InProgress);
        assert_eq!(linear_to_animus("In Review"), SubjectStatus::InProgress);
        assert_eq!(linear_to_animus("Blocked"), SubjectStatus::Blocked);
        assert_eq!(linear_to_animus("Done"), SubjectStatus::Done);
        assert_eq!(linear_to_animus("Cancelled"), SubjectStatus::Cancelled);
        assert_eq!(linear_to_animus("Canceled"), SubjectStatus::Cancelled);
        assert_eq!(linear_to_animus("Duplicate"), SubjectStatus::Cancelled);
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(linear_to_animus("in progress"), SubjectStatus::InProgress);
        assert_eq!(linear_to_animus("IN PROGRESS"), SubjectStatus::InProgress);
        assert_eq!(
            linear_to_animus("  In Progress  "),
            SubjectStatus::InProgress
        );
    }

    #[test]
    fn unknown_natives_default_to_ready() {
        assert_eq!(linear_to_animus("Awaiting Triage"), SubjectStatus::Ready);
        assert_eq!(linear_to_animus(""), SubjectStatus::Ready);
    }

    #[test]
    fn animus_to_linear_round_trips_canonical_values() {
        for status in [
            SubjectStatus::Ready,
            SubjectStatus::InProgress,
            SubjectStatus::Blocked,
            SubjectStatus::Done,
            SubjectStatus::Cancelled,
        ] {
            let native = animus_to_linear(status);
            assert_eq!(
                linear_to_animus(native),
                status,
                "round-trip for {status:?}"
            );
        }
    }
}
