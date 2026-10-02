//! The `ctl.sock` protocol: one JSON request line, one JSON response line (spec §7.2).

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Show,
    Down,
    /// Namespace fds for `exec`/`shell`, sent with `SCM_RIGHTS` in user, mnt, net, uts, pid
    /// order.
    Enter {
        node: String,
    },
    Explain {
        node: String,
    },
    Cut {
        sides: Vec<String>,
        oneway: bool,
        for_ms: Option<u64>,
    },
    Set {
        sides: Vec<String>,
        assignments: Vec<String>,
        oneway: bool,
        for_ms: Option<u64>,
    },
    Heal {
        sides: Vec<String>,
        all: bool,
    },
    Apply {
        path: String,
        source: String,
    },
    Stop {
        selector: String,
        signal: Option<String>,
        grace_ms: Option<u64>,
        for_ms: Option<u64>,
    },
    Start {
        selector: String,
    },
    Restart {
        selector: String,
    },
    Resync {
        selector: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Human-readable effective changes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

impl Response {
    pub fn ok(changes: Vec<String>, data: serde_json::Value) -> Self {
        Response {
            v: VERSION,
            ok: true,
            error: None,
            changes,
            data,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Response {
            v: VERSION,
            ok: false,
            error: Some(msg.into()),
            ..Default::default()
        }
    }
}
