//! The topology model: parsing and validating topology files.
//!
//! A [`Topology`] is the validated, fully expanded form of a file: groups are resolved, every
//! link is expanded into directed [`Edge`]s with merged [`LinkProps`], and every node's `run`
//! command is resolved.

mod props;
mod raw;

pub use props::LinkProps;

use crate::net::{Ipv4Net, Ipv6Net};
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

/// The built-in node that represents the internet uplink.
pub const INTERNET: &str = "internet";
const RESERVED: [&str; 3] = [INTERNET, "host", "*"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ModelError(pub String);

impl ModelError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Reachability = adjacency, on one shared segment.
    #[default]
    Direct,
    /// Reachability = path, nodes route for each other.
    Routed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    #[default]
    Hops,
    Delay,
}

/// A directed edge between two endpoints (nodes or [`INTERNET`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
}

impl Edge {
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
        }
    }

    pub fn reversed(&self) -> Self {
        Self::new(self.to.clone(), self.from.clone())
    }
}

impl fmt::Display for Edge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}→{}", self.from, self.to)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeSpec {
    pub addr: Option<Ipv4Addr>,
    pub addr6: Option<Ipv6Addr>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InternetSpec {
    pub host_loopback: bool,
}

/// Serialises `IndexMap<Edge, T>` as a list of `[edge, value]` pairs (JSON object keys must be
/// strings).
pub mod edge_map {
    use super::Edge;
    use indexmap::IndexMap;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T: Serialize, S: Serializer>(
        map: &IndexMap<Edge, T>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.collect_seq(map.iter())
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        d: D,
    ) -> Result<IndexMap<Edge, T>, D::Error> {
        Ok(Vec::<(Edge, T)>::deserialize(d)?.into_iter().collect())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Topology {
    pub name: String,
    pub mode: Mode,
    pub metric: Metric,
    pub subnet: Ipv4Net,
    pub subnet6: Ipv6Net,
    pub nodes: IndexMap<String, NodeSpec>,
    pub groups: IndexMap<String, Vec<String>>,
    #[serde(with = "edge_map")]
    pub edges: IndexMap<Edge, LinkProps>,
    pub internet: InternetSpec,
    /// Resolved `run` command per node; `None` means run nothing.
    pub run: IndexMap<String, Option<String>>,
}

impl Topology {
    /// Parses and validates a topology file. `default_name` is used when the file has no `name`
    /// (normally the file stem).
    pub fn parse(src: &str, default_name: &str) -> Result<Self, ModelError> {
        let raw: raw::RawFile = toml::from_str(src)
            .map_err(|e| ModelError::new(e.to_string().trim_end().to_owned()))?;
        raw.build(default_name)
    }

    pub fn uses_internet(&self) -> bool {
        self.edges
            .keys()
            .any(|e| e.from == INTERNET || e.to == INTERNET)
    }

    pub fn run_command(&self, node: &str) -> Option<&str> {
        self.run.get(node).and_then(|c| c.as_deref())
    }

    /// Expands a selector (`*`, node, group, or a comma list of those) to endpoint names in
    /// order, without duplicates. `*` and `internet` include the internet node only when the
    /// lab uses it.
    pub fn select(&self, selector: &str) -> Result<Vec<String>, ModelError> {
        let mut out = IndexSet::new();
        for part in selector.split(',').map(str::trim) {
            match part {
                "*" => {
                    out.extend(self.nodes.keys().cloned());
                    if self.uses_internet() {
                        out.insert(INTERNET.to_owned());
                    }
                }
                INTERNET if self.uses_internet() => {
                    out.insert(INTERNET.to_owned());
                }
                name if self.nodes.contains_key(name) => {
                    out.insert(name.to_owned());
                }
                name => match self.groups.get(name) {
                    Some(members) => out.extend(members.iter().cloned()),
                    None => {
                        return Err(ModelError::new(format!(
                            "unknown node or group `{name}` in selector `{selector}`"
                        )));
                    }
                },
            }
        }
        Ok(out.into_iter().collect())
    }
}

fn validate_name(name: &str, what: &str) -> Result<(), ModelError> {
    if RESERVED.contains(&name) {
        return Err(ModelError::new(format!(
            "`{name}` is a reserved name and can't be used as a {what}"
        )));
    }
    let valid = !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !valid {
        return Err(ModelError::new(format!(
            "invalid {what} name `{name}`: use 1-63 lowercase letters, digits and `-`"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
