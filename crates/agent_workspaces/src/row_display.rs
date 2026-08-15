//! Presentation helpers for sidebar rows.
//!
//! These are pure functions with no agent or thread concepts in them, extracted
//! from the archive view so the sidebar can keep rendering and searching
//! terminal rows once the agent surfaces are deleted.

use chrono::{DateTime, Utc};

/// Character offsets in `candidate` matching a contiguous, case-insensitive run
/// of `query`, for highlighting search hits. `None` when there is no match.
pub fn fuzzy_match_positions(query: &str, candidate: &str) -> Option<Vec<usize>> {
    let query_chars: Vec<char> = query.chars().collect();
    if query_chars.is_empty() {
        return Some(Vec::new());
    }

    let candidate_chars: Vec<(usize, char)> = candidate.char_indices().collect();
    let window_count = candidate_chars.len().checked_sub(query_chars.len() - 1)?;

    'outer: for window_start in 0..window_count {
        for (qi, &query_char) in query_chars.iter().enumerate() {
            let (_, cand_char) = candidate_chars[window_start + qi];
            if !cand_char.eq_ignore_ascii_case(&query_char) {
                continue 'outer;
            }
        }
        return Some(
            (0..query_chars.len())
                .map(|qi| candidate_chars[window_start + qi].0)
                .collect(),
        );
    }

    None
}

/// A compact age, as history rows show it: `5m`, `3h`, `2d`, `1w`, `4mo`.
pub fn format_history_entry_timestamp(entry_time: DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(entry_time);

    let minutes = duration.num_minutes();
    let hours = duration.num_hours();
    let days = duration.num_days();
    let weeks = days / 7;
    let months = days / 30;

    if minutes < 60 {
        format!("{}m", minutes.max(1))
    } else if hours < 24 {
        format!("{}h", hours.max(1))
    } else if days < 7 {
        format!("{}d", days.max(1))
    } else if weeks < 4 {
        format!("{}w", weeks.max(1))
    } else {
        format!("{}mo", months.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuzzy_match_positions_finds_a_contiguous_run() {
        assert_eq!(fuzzy_match_positions("bar", "foobar"), Some(vec![3, 4, 5]));
        assert_eq!(fuzzy_match_positions("BAR", "foobar"), Some(vec![3, 4, 5]));
        assert_eq!(fuzzy_match_positions("", "foobar"), Some(Vec::new()));
        assert_eq!(fuzzy_match_positions("bar", "foo"), None);
        assert_eq!(
            fuzzy_match_positions("longer", "no"),
            None,
            "a query longer than the candidate must not panic on the window count"
        );
    }
}
