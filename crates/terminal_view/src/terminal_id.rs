//! Identity for a terminal ZOrca owns.
//!
//! Lives outside the agent panel because it outlives it: the sidebar, the
//! terminal metadata store and the worktree archive all key on it, and stage 5
//! deletes the panel's agent surfaces around them.
//!
//! Distinct from `acp_thread::TerminalId`, which identifies a terminal an agent
//! spawned through the agent client protocol.

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TerminalId(uuid::Uuid);

impl Default for TerminalId {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    /// The form used as a database key and in serialized panel state.
    pub fn to_key_string(self) -> String {
        self.0.hyphenated().to_string()
    }

    pub fn from_key_string(key: &str) -> anyhow::Result<Self> {
        Ok(Self(uuid::Uuid::parse_str(key)?))
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_terminal_id_survives_the_key_round_trip() {
        let id = TerminalId::new();
        assert_eq!(
            TerminalId::from_key_string(&id.to_key_string()).unwrap(),
            id
        );
        assert!(
            TerminalId::from_key_string("not-a-uuid").is_err(),
            "a malformed key must not silently become a fresh id"
        );
        assert_ne!(TerminalId::new(), TerminalId::new());
    }
}
