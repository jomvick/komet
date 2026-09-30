//! Session stats — pure derivations over already-loaded entries.
//!
//! Nothing here touches the doc, the sync proto or the engine: duration and
//! counts are computed from the projection the transcript view already holds.

use crate::schema::{MessageRole, SessionMessageEntry};

/// Gaps at or above this are idle, not active work (5 min). Named constant —
/// never a magic literal in UI code.
pub const ACTIVE_GAP_IDLE_MS: i64 = 5 * 60_000;

/// Active duration: sum of consecutive `created_at` gaps below
/// [`ACTIVE_GAP_IDLE_MS`]. Entries are sorted internally (out-of-order safe);
/// large idle holes are excluded.
pub fn session_active_duration_ms(entries: &[SessionMessageEntry]) -> i64 {
    if entries.len() < 2 {
        return 0;
    }
    let mut times: Vec<i64> = entries.iter().map(|e| e.created_at).collect();
    times.sort_unstable();
    times
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|gap| *gap >= 0 && *gap < ACTIVE_GAP_IDLE_MS)
        .sum()
}

/// Message count shared by sidebar and header. System messages are excluded
/// (transport, not conversation).
pub fn message_count(entries: &[SessionMessageEntry]) -> usize {
    entries
        .iter()
        .filter(|e| !matches!(e.role, MessageRole::System))
        .count()
}

#[cfg(test)]
mod stats_tests {
    use super::{ACTIVE_GAP_IDLE_MS, message_count, session_active_duration_ms};
    use crate::parts::MessagePart;
    use crate::schema::{MessageRole, SessionMessageEntry};

    fn entry_at(role: MessageRole, created_at: i64) -> SessionMessageEntry {
        SessionMessageEntry {
            id: format!("m{created_at}"),
            role,
            parts: vec![MessagePart::Text {
                id: "p".into(),
                text: "hi".into(),
            }],
            created_at,
            device_id: "d".into(),
            status: None,
            continuation_of: None,
        }
    }

    #[test]
    fn close_messages_count_the_gap() {
        let entries = vec![entry_at(MessageRole::User, 0), entry_at(MessageRole::User, 60_000)];
        assert_eq!(session_active_duration_ms(&entries), 60_000);
    }

    #[test]
    fn idle_holes_are_excluded() {
        let entries = vec![
            entry_at(MessageRole::User, 0),
            entry_at(MessageRole::User, 60_000),
            entry_at(MessageRole::User, 60_000 + ACTIVE_GAP_IDLE_MS + 1),
        ];
        assert_eq!(session_active_duration_ms(&entries), 60_000);
    }

    #[test]
    fn out_of_order_entries_are_sorted() {
        let entries = vec![
            entry_at(MessageRole::User, 120_000),
            entry_at(MessageRole::User, 0),
            entry_at(MessageRole::User, 60_000),
        ];
        assert_eq!(session_active_duration_ms(&entries), 120_000);
    }

    #[test]
    fn message_count_excludes_system() {
        let entries = vec![
            entry_at(MessageRole::User, 0),
            entry_at(MessageRole::Assistant, 1),
            entry_at(MessageRole::System, 2),
        ];
        assert_eq!(message_count(&entries), 2);
    }
}
