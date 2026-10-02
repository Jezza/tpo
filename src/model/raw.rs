//! The file format as written, before validation and expansion.

use super::{
    Edge, INTERNET, InternetSpec, LinkProps, Metric, Mode, ModelError, NodeSpec, Topology,
    validate_name,
};
use crate::units::{parse_duration, parse_percent, parse_rate};
use indexmap::{IndexMap, IndexSet};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawFile {
    name: Option<String>,
    #[serde(default)]
    mode: Mode,
    #[serde(default)]
    metric: Metric,
    subnet: Option<String>,
    subnet6: Option<String>,
    #[serde(default)]
    defaults: RawProps,
    #[serde(default)]
    nodes: IndexMap<String, RawNode>,
    #[serde(default)]
    groups: IndexMap<String, Vec<String>>,
    #[serde(default, rename = "link")]
    links: Vec<RawLink>,
    #[serde(default)]
    internet: RawInternet,
    #[serde(default)]
    run: IndexMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawNode {
    addr: Option<String>,
    addr6: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawInternet {
    #[serde(default)]
    host_loopback: bool,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawProps {
    delay: Option<String>,
    jitter: Option<String>,
    loss: Option<String>,
    duplicate: Option<String>,
    reorder: Option<String>,
    corrupt: Option<String>,
    rate: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Endpoints {
    One(String),
    Many(Vec<String>),
}

impl Endpoints {
    fn names(&self) -> Vec<&str> {
        match self {
            Endpoints::One(n) => vec![n.as_str()],
            Endpoints::Many(ns) => ns.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLink {
    path: Option<Vec<Endpoints>>,
    from: Option<Endpoints>,
    to: Option<Endpoints>,
    mesh: Option<Endpoints>,
    #[serde(default)]
    oneway: bool,
    delay: Option<String>,
    jitter: Option<String>,
    loss: Option<String>,
    duplicate: Option<String>,
    reorder: Option<String>,
    corrupt: Option<String>,
    rate: Option<String>,
}

impl RawLink {
    fn props(&self) -> RawProps {
        RawProps {
            delay: self.delay.clone(),
            jitter: self.jitter.clone(),
            loss: self.loss.clone(),
            duplicate: self.duplicate.clone(),
            reorder: self.reorder.clone(),
            corrupt: self.corrupt.clone(),
            rate: self.rate.clone(),
        }
    }
}

impl RawProps {
    fn parse(&self, context: &str) -> Result<LinkProps, ModelError> {
        fn field<T>(
            context: &str,
            key: &str,
            value: &Option<String>,
            parse: fn(&str) -> Result<T, String>,
        ) -> Result<Option<T>, ModelError> {
            value
                .as_deref()
                .map(|v| parse(v).map_err(|e| ModelError::new(format!("{context}: `{key}`: {e}"))))
                .transpose()
        }
        Ok(LinkProps {
            delay: field(context, "delay", &self.delay, parse_duration)?,
            jitter: field(context, "jitter", &self.jitter, parse_duration)?,
            loss: field(context, "loss", &self.loss, parse_percent)?,
            duplicate: field(context, "duplicate", &self.duplicate, parse_percent)?,
            reorder: field(context, "reorder", &self.reorder, parse_percent)?,
            corrupt: field(context, "corrupt", &self.corrupt, parse_percent)?,
            rate: field(context, "rate", &self.rate, parse_rate)?,
        })
    }
}

impl RawFile {
    pub(super) fn build(self, default_name: &str) -> Result<Topology, ModelError> {
        let name = self.name.unwrap_or_else(|| default_name.to_owned());
        validate_name(&name, "lab").map_err(|e| ModelError::new(format!("{e} (set `name`)")))?;

        let subnet = self
            .subnet
            .as_deref()
            .unwrap_or("10.77.0.0/24")
            .parse()
            .map_err(|e| ModelError::new(format!("`subnet`: {e}")))?;
        let subnet6 = self
            .subnet6
            .as_deref()
            .unwrap_or("fd77::/64")
            .parse()
            .map_err(|e| ModelError::new(format!("`subnet6`: {e}")))?;

        let mut nodes = IndexMap::new();
        for (node, raw) in self.nodes {
            validate_name(&node, "node")?;
            let addr = raw
                .addr
                .map(|a| {
                    a.parse().map_err(|_| {
                        ModelError::new(format!(
                            "node `{node}`: `addr` `{a}` is not an IPv4 address"
                        ))
                    })
                })
                .transpose()?;
            let addr6 = raw
                .addr6
                .map(|a| {
                    a.parse().map_err(|_| {
                        ModelError::new(format!(
                            "node `{node}`: `addr6` `{a}` is not an IPv6 address"
                        ))
                    })
                })
                .transpose()?;
            nodes.insert(node, NodeSpec { addr, addr6 });
        }

        let mut groups = IndexMap::new();
        for (group, members) in self.groups {
            validate_name(&group, "group")?;
            if nodes.contains_key(&group) {
                return Err(ModelError::new(format!(
                    "`{group}` is both a node and a group"
                )));
            }
            for m in &members {
                if !nodes.contains_key(m) {
                    return Err(ModelError::new(format!(
                        "group `{group}` has unknown member `{m}`"
                    )));
                }
            }
            let unique: IndexSet<String> = members.into_iter().collect();
            groups.insert(group, unique.into_iter().collect());
        }

        let mut topo = Topology {
            name,
            mode: self.mode,
            metric: self.metric,
            subnet,
            subnet6,
            nodes,
            groups,
            edges: IndexMap::new(),
            internet: InternetSpec {
                host_loopback: self.internet.host_loopback,
            },
            run: IndexMap::new(),
        };

        let defaults = self.defaults.parse("[defaults]")?;
        for (i, link) in self.links.iter().enumerate() {
            let context = format!("link {}", i + 1);
            let props = link.props().parse(&context)?;
            let pairs = expand_link(&topo, link, &context)?;
            if pairs.is_empty() {
                return Err(ModelError::new(format!(
                    "{context} produces no links (only self-links)"
                )));
            }
            for edge in pairs {
                topo.edges
                    .entry(edge)
                    .or_insert_with(|| defaults.clone())
                    .merge(&props);
            }
        }
        for (edge, props) in &topo.edges {
            props
                .validate()
                .map_err(|e| ModelError::new(format!("link {edge}: {e}")))?;
        }

        topo.run = resolve_run(&topo, &self.run)?;
        Ok(topo)
    }
}

/// Resolves link endpoints (nodes, groups, `internet`) to endpoint names.
fn endpoints(topo: &Topology, names: &[&str], context: &str) -> Result<Vec<String>, ModelError> {
    let mut out = IndexSet::new();
    for &name in names {
        if name == INTERNET {
            out.insert(INTERNET.to_owned());
        } else if topo.nodes.contains_key(name) {
            out.insert(name.to_owned());
        } else if let Some(members) = topo.groups.get(name) {
            out.extend(members.iter().cloned());
        } else {
            return Err(ModelError::new(format!(
                "{context}: unknown node or group `{name}`"
            )));
        }
    }
    Ok(out.into_iter().collect())
}

fn expand_link(topo: &Topology, link: &RawLink, context: &str) -> Result<Vec<Edge>, ModelError> {
    let shapes = [
        link.path.is_some(),
        link.from.is_some() || link.to.is_some(),
        link.mesh.is_some(),
    ];
    match shapes.iter().filter(|s| **s).count() {
        0 => {
            return Err(ModelError::new(format!(
                "{context}: needs `path`, `from`/`to` or `mesh`"
            )));
        }
        1 => {}
        _ => {
            return Err(ModelError::new(format!(
                "{context}: use only one of `path`, `from`/`to` and `mesh`"
            )));
        }
    }

    let mut edges = Vec::new();
    let both = |a: &[String], b: &[String], edges: &mut Vec<Edge>| {
        for x in a {
            for y in b {
                if x != y {
                    edges.push(Edge::new(x, y));
                    edges.push(Edge::new(y, x));
                }
            }
        }
    };

    if let Some(path) = &link.path {
        if link.oneway {
            return Err(ModelError::new(format!(
                "{context}: `oneway` only works with `from`/`to`"
            )));
        }
        let steps = path
            .iter()
            .map(|e| endpoints(topo, &e.names(), context))
            .collect::<Result<Vec<_>, _>>()?;
        for pair in steps.windows(2) {
            both(&pair[0], &pair[1], &mut edges);
        }
    } else if let Some(mesh) = &link.mesh {
        if link.oneway {
            return Err(ModelError::new(format!(
                "{context}: `oneway` only works with `from`/`to`"
            )));
        }
        let members = endpoints(topo, &mesh.names(), context)?;
        for (i, x) in members.iter().enumerate() {
            for y in &members[i + 1..] {
                edges.push(Edge::new(x, y));
                edges.push(Edge::new(y, x));
            }
        }
    } else {
        let (Some(from), Some(to)) = (&link.from, &link.to) else {
            return Err(ModelError::new(format!(
                "{context}: `from` and `to` must be used together"
            )));
        };
        let from = endpoints(topo, &from.names(), context)?;
        let to = endpoints(topo, &to.names(), context)?;
        if link.oneway {
            if from.iter().chain(&to).any(|n| n == INTERNET) {
                return Err(ModelError::new(format!(
                    "{context}: `oneway` links to `internet` aren't supported"
                )));
            }
            for x in &from {
                for y in &to {
                    if x != y {
                        edges.push(Edge::new(x, y));
                    }
                }
            }
        } else {
            both(&from, &to, &mut edges);
        }
    }
    Ok(edges)
}

/// Resolves `[run]` per node: node entry > group entry > `*` > nothing.
fn resolve_run(
    topo: &Topology,
    entries: &IndexMap<String, String>,
) -> Result<IndexMap<String, Option<String>>, ModelError> {
    let mut star: Option<(&str, &str)> = None;
    let mut by_node: IndexMap<&str, (&str, &str)> = IndexMap::new();
    let mut by_group: Vec<(&str, &str)> = Vec::new();

    for (key, cmd) in entries {
        for part in key.split(',').map(str::trim) {
            if part == "*" {
                if let Some((prev, _)) = star {
                    return Err(ModelError::new(format!(
                        "[run]: `*` appears in both `{prev}` and `{key}`"
                    )));
                }
                star = Some((key, cmd));
            } else if topo.nodes.contains_key(part) {
                if let Some((prev, _)) = by_node.insert(part, (key, cmd)) {
                    return Err(ModelError::new(format!(
                        "[run]: node `{part}` is named more than once (`{prev}` and `{key}`)"
                    )));
                }
            } else if topo.groups.contains_key(part) {
                by_group.push((part, cmd));
            } else {
                return Err(ModelError::new(format!(
                    "[run]: unknown node or group `{part}` in `{key}`"
                )));
            }
        }
    }

    let mut run = IndexMap::new();
    for node in topo.nodes.keys() {
        let cmd = if let Some((_, cmd)) = by_node.get(node.as_str()) {
            Some(*cmd)
        } else {
            let matching: Vec<_> = by_group
                .iter()
                .filter(|(g, _)| topo.groups[*g].contains(node))
                .collect();
            match matching.as_slice() {
                [] => star.map(|(_, c)| c),
                [(_, cmd)] => Some(*cmd),
                many => {
                    let names: Vec<_> = many.iter().map(|(g, _)| format!("`{g}`")).collect();
                    return Err(ModelError::new(format!(
                        "[run]: node `{node}` is matched by groups {}; add a \"{node}\" entry or remove one",
                        names.join(" and ")
                    )));
                }
            }
        };
        run.insert(
            node.clone(),
            cmd.filter(|c| !c.is_empty()).map(str::to_owned),
        );
    }
    Ok(run)
}
