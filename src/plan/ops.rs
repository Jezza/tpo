//! Operations: the `ip`/`tc`/`nft`/sysctl steps that change a namespace.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The namespace an operation runs in.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Target {
    /// The lab init's netns, which holds the bridge.
    Hub,
    /// The internet gateway's netns.
    Gateway,
    Node(String),
    /// The host's own namespaces (root mode only).
    Host,
}

impl Target {
    pub fn node(name: impl Into<String>) -> Self {
        Target::Node(name.into())
    }
}

impl Target {
    /// The inverse of `Display`.
    pub fn parse(s: &str) -> Self {
        match s {
            "hub" => Target::Hub,
            "gateway" => Target::Gateway,
            "host" => Target::Host,
            n => Target::node(n),
        }
    }
}

/// The prefix of an `ip` argument the backend replaces with a path to that target's netns.
pub const NETNS_OF: &str = "@netns:";

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Hub => f.write_str("hub"),
            Target::Gateway => f.write_str("gateway"),
            Target::Node(n) => f.write_str(n),
            Target::Host => f.write_str("host"),
        }
    }
}

/// The token in `ip` arguments that the backend replaces with the hub's pid.
pub const HUB_PID: &str = "@hub";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Ip(Vec<String>),
    Tc(Vec<String>),
    /// Writes `value` to `/proc/sys/<key with . replaced by />`.
    Sysctl {
        key: String,
        value: String,
    },
    /// An `nft -f -` script.
    Nft(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    pub target: Target,
    pub action: Action,
}

fn words(args: &str) -> Vec<String> {
    args.split_whitespace().map(str::to_owned).collect()
}

impl Op {
    /// `ip` with whitespace-separated `args`.
    pub fn ip(target: &Target, args: impl AsRef<str>) -> Self {
        Op {
            target: target.clone(),
            action: Action::Ip(words(args.as_ref())),
        }
    }

    /// `tc` with whitespace-separated `args`.
    pub fn tc(target: &Target, args: impl AsRef<str>) -> Self {
        Op {
            target: target.clone(),
            action: Action::Tc(words(args.as_ref())),
        }
    }

    pub fn sysctl(target: &Target, key: &str, value: &str) -> Self {
        Op {
            target: target.clone(),
            action: Action::Sysctl {
                key: key.to_owned(),
                value: value.to_owned(),
            },
        }
    }

    pub fn nft(target: &Target, script: impl Into<String>) -> Self {
        Op {
            target: target.clone(),
            action: Action::Nft(script.into()),
        }
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.target)?;
        match &self.action {
            Action::Ip(args) => write!(f, "ip {}", args.join(" ")),
            Action::Tc(args) => write!(f, "tc {}", args.join(" ")),
            Action::Sysctl { key, value } => write!(f, "sysctl {key}={value}"),
            Action::Nft(script) => write!(
                f,
                "nft {}",
                script.lines().map(str::trim).collect::<Vec<_>>().join(" ")
            ),
        }
    }
}
