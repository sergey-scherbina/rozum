use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ParticipantId(pub String);

impl ParticipantId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for ParticipantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ParticipantKind {
    Mcp {
        client_info_name: String,
    },
    Human {
        display_name: String,
    },
    /// External-channel bridge (web, telegram, discord, …). Acts like an MCP
    /// participant but with no response timeouts: when their turn comes they
    /// either submit queued user input or silently release the turn.
    Bridge {
        client_info_name: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Participant {
    pub id: ParticipantId,
    pub kind: ParticipantKind,
}

impl Participant {
    pub fn human(display_name: impl Into<String>) -> Self {
        let name = display_name.into();
        Self {
            id: ParticipantId::new(&name),
            kind: ParticipantKind::Human { display_name: name },
        }
    }

    pub fn mcp(client_info_name: impl Into<String>) -> Self {
        let name = client_info_name.into();
        Self {
            id: ParticipantId::new(&name),
            kind: ParticipantKind::Mcp {
                client_info_name: name,
            },
        }
    }

    pub fn bridge(client_info_name: impl Into<String>) -> Self {
        let name = client_info_name.into();
        Self {
            id: ParticipantId::new(&name),
            kind: ParticipantKind::Bridge {
                client_info_name: name,
            },
        }
    }

    pub fn display_name(&self) -> &str {
        match &self.kind {
            ParticipantKind::Mcp { client_info_name } => client_info_name,
            ParticipantKind::Human { display_name } => display_name,
            ParticipantKind::Bridge { client_info_name } => client_info_name,
        }
    }

    pub fn is_human(&self) -> bool {
        matches!(self.kind, ParticipantKind::Human { .. })
    }

    pub fn is_bridge(&self) -> bool {
        matches!(self.kind, ParticipantKind::Bridge { .. })
    }
}
