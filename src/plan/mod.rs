//! Planning: faults, the effective edge set, and the operations that realise it.

pub mod direct;
pub mod ops;
pub mod routed;

pub use ops::{Op, Target};

use crate::model::{Edge, INTERNET, LinkProps, ModelError, Topology};
use crate::units::{human_duration, parse_duration, parse_percent, parse_rate};
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};

fn err(msg: impl Into<String>) -> ModelError {
    ModelError(msg.into())
}

// ---- desired network state, by mode -------------------------------------------------------

/// The desired network state of every endpoint, in the lab's mode.
#[derive(Clone, Debug)]
pub enum Net {
    Direct(direct::Desired),
    Routed(routed::Desired),
}

impl Net {
    pub fn empty(mode: crate::model::Mode) -> Self {
        match mode {
            crate::model::Mode::Direct => Net::Direct(direct::Desired::new()),
            crate::model::Mode::Routed => Net::Routed(routed::Desired::new()),
        }
    }

    /// The desired state for `eff`; `prev` supplies routes to keep when no path exists.
    pub fn plan(ctx: &direct::Ctx, eff: &IndexMap<Edge, EffEdge>, prev: &Net) -> Self {
        match ctx.topo.mode {
            crate::model::Mode::Direct => Net::Direct(direct::desired(ctx, eff)),
            crate::model::Mode::Routed => {
                let prev = match prev {
                    Net::Routed(p) => Some(p),
                    Net::Direct(_) => None,
                };
                Net::Routed(routed::desired(ctx, eff, prev))
            }
        }
    }

    /// Operations from `self` to `new`.
    pub fn diff(&self, new: &Net) -> Vec<Op> {
        match (self, new) {
            (Net::Direct(a), Net::Direct(b)) => direct::diff(a, b),
            (Net::Routed(a), Net::Routed(b)) => routed::diff(a, b),
            (_, Net::Direct(b)) => direct::initial_ops(b),
            (_, Net::Routed(b)) => routed::initial_ops(b),
        }
    }

    pub fn initial_ops(&self) -> Vec<Op> {
        match self {
            Net::Direct(d) => direct::initial_ops(d),
            Net::Routed(d) => routed::initial_ops(d),
        }
    }

    pub fn targets(&self) -> Vec<Target> {
        match self {
            Net::Direct(d) => d.keys().cloned().collect(),
            Net::Routed(d) => d.keys().cloned().collect(),
        }
    }

    /// Nodes allowed through the internet gateway right now, as `(ipv4, ipv6)`.
    pub fn internet_allowed(
        &self,
    ) -> std::collections::BTreeSet<(std::net::Ipv4Addr, std::net::Ipv6Addr)> {
        match self {
            Net::Direct(d) => d
                .get(&Target::Gateway)
                .map(|gw| {
                    gw.allow
                        .keys()
                        .filter_map(|i| gw.neighbours.get(i))
                        .map(|n| (n.v4, n.v6))
                        .collect()
                })
                .unwrap_or_default(),
            Net::Routed(d) => d
                .values()
                .filter(|n| n.internet_path)
                .map(|n| (n.own.v4, n.own.v6))
                .collect(),
        }
    }

    /// Everything that produces `t`'s state from empty namespaces.
    pub fn explain(&self, t: &Target) -> Option<Vec<Op>> {
        match self {
            Net::Direct(d) => d.get(t).map(|n| direct::explain_ops(t, n)),
            Net::Routed(d) => d.get(t).map(|n| routed::explain_ops(t, n)),
        }
    }

    /// Rebuilds `t`'s tc/neighbour/route state from scratch.
    pub fn rebuild(&self, t: &Target) -> Option<Vec<Op>> {
        match self {
            Net::Direct(d) => d.get(t).map(|n| {
                let mut ops = direct::reset_ops(t);
                ops.extend(direct::diff_from_reset(t, n));
                ops
            }),
            Net::Routed(d) => d.get(t).map(|n| routed::rebuild_ops(t, n)),
        }
    }
}

// ---- sides -------------------------------------------------------------------------------

/// The spec edges between different sides (spec §7.4). One side means `<side> *`; `*` is every
/// endpoint not named in another side. With `oneway`, only first → second.
pub fn edges_between(
    topo: &Topology,
    sides: &[&str],
    oneway: bool,
) -> Result<Vec<Edge>, ModelError> {
    if sides.is_empty() {
        return Err(err("give at least one side"));
    }
    if oneway && sides.len() != 2 {
        return Err(err("`--oneway` needs exactly two sides"));
    }
    let mut sides: Vec<&str> = sides.to_vec();
    if sides.len() == 1 {
        sides.push("*");
    }
    if sides.iter().filter(|s| **s == "*").count() > 1 {
        return Err(err("`*` can only be used as one side"));
    }

    let mut owner: IndexMap<String, usize> = IndexMap::new();
    for (i, side) in sides.iter().enumerate() {
        if *side == "*" {
            continue;
        }
        for name in topo.select(side)? {
            if let Some(prev) = owner.insert(name.clone(), i)
                && prev != i
            {
                return Err(err(format!(
                    "`{name}` is in more than one side; sides must not overlap"
                )));
            }
        }
    }
    if let Some(star) = sides.iter().position(|s| *s == "*") {
        for name in topo.select("*")? {
            owner.entry(name).or_insert(star);
        }
    }

    if oneway
        && sides.iter().any(|s| {
            topo.select(s)
                .is_ok_and(|n| n.iter().any(|n| n == INTERNET))
        })
    {
        return Err(err("`--oneway` isn't supported for links to `internet`"));
    }

    let edges: Vec<Edge> = topo
        .edges
        .keys()
        .filter(|edge| match (owner.get(&edge.from), owner.get(&edge.to)) {
            (Some(&f), Some(&t)) if oneway => f == 0 && t == 1,
            (Some(&f), Some(&t)) => f != t,
            _ => false,
        })
        .cloned()
        .collect();
    if edges.is_empty() {
        return Err(err(format!(
            "there are no links between {}",
            sides.join(" / ")
        )));
    }
    Ok(edges)
}

// ---- faults ------------------------------------------------------------------------------

/// When a fault expires, in unix milliseconds; `None` lasts until healed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Until(pub Option<u64>);

impl Until {
    pub const FOREVER: Until = Until(None);

    pub fn at(unix_ms: u64) -> Self {
        Until(Some(unix_ms))
    }

    fn live(self, now: u64) -> bool {
        self.0.is_none_or(|t| now < t)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Prop {
    Delay,
    Jitter,
    Loss,
    Duplicate,
    Reorder,
    Corrupt,
    Rate,
}

impl Prop {
    const ALL: [Prop; 7] = [
        Prop::Delay,
        Prop::Jitter,
        Prop::Loss,
        Prop::Duplicate,
        Prop::Reorder,
        Prop::Corrupt,
        Prop::Rate,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Prop::Delay => "delay",
            Prop::Jitter => "jitter",
            Prop::Loss => "loss",
            Prop::Duplicate => "duplicate",
            Prop::Reorder => "reorder",
            Prop::Corrupt => "corrupt",
            Prop::Rate => "rate",
        }
    }

    /// A `LinkProps` with only this property set to `value`.
    fn parse_value(self, value: &str) -> Result<LinkProps, String> {
        let mut p = LinkProps::default();
        match self {
            Prop::Delay => p.delay = Some(parse_duration(value)?),
            Prop::Jitter => p.jitter = Some(parse_duration(value)?),
            Prop::Loss => p.loss = Some(parse_percent(value)?),
            Prop::Duplicate => p.duplicate = Some(parse_percent(value)?),
            Prop::Reorder => p.reorder = Some(parse_percent(value)?),
            Prop::Corrupt => p.corrupt = Some(parse_percent(value)?),
            Prop::Rate => p.rate = Some(parse_rate(value)?),
        }
        Ok(p)
    }

    /// The property's value in `props`, for humans; `None` when unset.
    fn show(self, props: &LinkProps) -> Option<String> {
        match self {
            Prop::Delay => props.delay.map(human_duration),
            Prop::Jitter => props.jitter.map(human_duration),
            Prop::Loss => props.loss.map(|v| v.to_string()),
            Prop::Duplicate => props.duplicate.map(|v| v.to_string()),
            Prop::Reorder => props.reorder.map(|v| v.to_string()),
            Prop::Corrupt => props.corrupt.map(|v| v.to_string()),
            Prop::Rate => props.rate.map(|v| v.to_string()),
        }
    }
}

/// `key=value` from `tpo set`; `None` drops the override for `key`.
#[derive(Clone, Debug, PartialEq)]
pub struct Assignment {
    pub prop: Prop,
    pub value: Option<LinkProps>,
}

pub fn parse_assignments(args: &[&str]) -> Result<Vec<Assignment>, ModelError> {
    args.iter()
        .map(|arg| {
            let (key, value) = arg.split_once('=').ok_or_else(|| {
                err(format!(
                    "`{arg}` must be `key=value` (or `key=` to drop an override)"
                ))
            })?;
            let prop = Prop::ALL
                .into_iter()
                .find(|p| p.name() == key)
                .ok_or_else(|| {
                    let names: Vec<_> = Prop::ALL.iter().map(|p| p.name()).collect();
                    err(format!(
                        "unknown property `{key}` (use {})",
                        names.join(", ")
                    ))
                })?;
            let value = if value.is_empty() {
                None
            } else {
                Some(
                    prop.parse_value(value)
                        .map_err(|e| err(format!("`{key}`: {e}")))?,
                )
            };
            Ok(Assignment { prop, value })
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EdgeFault {
    pub cut: Option<Until>,
    pub props: IndexMap<Prop, (LinkProps, Until)>,
}

impl EdgeFault {
    fn is_empty(&self) -> bool {
        self.cut.is_none() && self.props.is_empty()
    }
}

/// Runtime faults layered over the spec, per directed edge (spec §7.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Faults {
    #[serde(with = "crate::model::edge_map")]
    pub edges: IndexMap<Edge, EdgeFault>,
}

impl Faults {
    pub fn cut(&mut self, edges: &[Edge], until: Until) {
        for edge in edges {
            self.edges.entry(edge.clone()).or_default().cut = Some(until);
        }
    }

    /// Applies assignments to `edges`, or changes nothing if any resulting edge is invalid.
    pub fn set(
        &mut self,
        topo: &Topology,
        edges: &[Edge],
        assignments: &[Assignment],
        until: Until,
    ) -> Result<(), ModelError> {
        let mut next = self.clone();
        for edge in edges {
            let fault = next.edges.entry(edge.clone()).or_default();
            for a in assignments {
                match &a.value {
                    Some(v) => {
                        fault.props.insert(a.prop, (v.clone(), until));
                    }
                    None => {
                        fault.props.shift_remove(&a.prop);
                    }
                }
            }
        }
        next.edges.retain(|_, f| !f.is_empty());
        for edge in edges {
            let props = next.props_for(topo, edge, 0);
            props
                .validate()
                .map_err(|e| err(format!("link {edge}: {e}")))?;
        }
        *self = next;
        Ok(())
    }

    pub fn heal(&mut self, edges: &[Edge]) {
        for edge in edges {
            self.edges.shift_remove(edge);
        }
    }

    pub fn heal_all(&mut self) {
        self.edges.clear();
    }

    /// Drops faults for edges the spec no longer has.
    pub fn retain_edges(&mut self, topo: &Topology) {
        self.edges.retain(|edge, _| topo.edges.contains_key(edge));
    }

    /// Removes faults that have expired at `now`; returns whether anything changed.
    pub fn expire(&mut self, now: u64) -> bool {
        let before = self.clone();
        for fault in self.edges.values_mut() {
            if fault.cut.is_some_and(|u| !u.live(now)) {
                fault.cut = None;
            }
            fault.props.retain(|_, (_, until)| until.live(now));
        }
        self.edges.retain(|_, f| !f.is_empty());
        *self != before
    }

    pub fn next_deadline(&self) -> Option<u64> {
        self.edges
            .values()
            .flat_map(|f| f.cut.iter().chain(f.props.values().map(|(_, u)| u)))
            .filter_map(|u| u.0)
            .min()
    }

    fn props_for(&self, topo: &Topology, edge: &Edge, now: u64) -> LinkProps {
        let mut props = topo.edges.get(edge).cloned().unwrap_or_default();
        if let Some(fault) = self.edges.get(edge) {
            for (value, until) in fault.props.values() {
                if until.live(now) {
                    props.merge(value);
                }
            }
        }
        props
    }
}

// ---- effective state ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffEdge {
    pub props: LinkProps,
    pub up: bool,
}

/// Spec ⊕ faults at time `now` (unix ms).
pub fn effective(topo: &Topology, faults: &Faults, now: u64) -> IndexMap<Edge, EffEdge> {
    topo.edges
        .keys()
        .map(|edge| {
            let up = !faults
                .edges
                .get(edge)
                .and_then(|f| f.cut)
                .is_some_and(|u| u.live(now));
            (
                edge.clone(),
                EffEdge {
                    props: faults.props_for(topo, edge, now),
                    up,
                },
            )
        })
        .collect()
}

/// Human-readable changes between two effective states, in edge order. A change that applies
/// to both directions of a pair is reported once with `⇄`.
pub fn describe_changes(
    before: &IndexMap<Edge, EffEdge>,
    after: &IndexMap<Edge, EffEdge>,
) -> Vec<String> {
    let up_change = |edge: &Edge| match (before.get(edge), after.get(edge)) {
        (Some(b), Some(a)) if b.up != a.up => Some(a.up),
        _ => None,
    };
    let presence = |edge: &Edge| (before.contains_key(edge), after.contains_key(edge));

    let mut lines = Vec::new();
    let mut reported = IndexSet::new();
    let mut props_reported = IndexSet::new();
    let removed = before.keys().filter(|e| !after.contains_key(*e));
    for edge in after.keys().chain(removed) {
        let rev = edge.reversed();
        let both = |line: &str| format!("{line} {}⇄{}", edge.from, edge.to);
        let one = |line: &str| format!("{line} {edge}");

        let (was, is) = presence(edge);
        if was != is {
            let verb = if is { "link" } else { "unlink" };
            if reported.contains(edge) {
                continue;
            }
            if presence(&rev) == (was, is) {
                reported.insert(rev.clone());
                lines.push(both(verb));
            } else {
                lines.push(one(verb));
            }
            continue;
        }

        if let Some(up) = up_change(edge)
            && !reported.contains(edge)
        {
            let verb = if up { "heal" } else { "cut" };
            if up_change(&rev) == Some(up) {
                reported.insert(rev.clone());
                lines.push(both(verb));
            } else {
                lines.push(one(verb));
            }
        }
        if props_reported.contains(edge) {
            continue;
        }
        let changes = prop_changes(edge, before, after);
        if !changes.is_empty() && prop_changes(&rev, before, after) == changes {
            props_reported.insert(rev.clone());
            lines.extend(
                changes
                    .iter()
                    .map(|c| format!("{}⇄{} {c}", edge.from, edge.to)),
            );
        } else {
            lines.extend(changes.iter().map(|c| format!("{edge} {c}")));
        }
    }
    lines
}

/// `delay 20ms loss 2%`: every property that is set, for humans.
pub fn describe_props(props: &LinkProps) -> String {
    Prop::ALL
        .iter()
        .filter_map(|p| p.show(props).map(|v| format!("{} {v}", p.name())))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `delay 10ms→200ms`-style changes of one edge's properties.
fn prop_changes(
    edge: &Edge,
    before: &IndexMap<Edge, EffEdge>,
    after: &IndexMap<Edge, EffEdge>,
) -> Vec<String> {
    let (Some(b), Some(a)) = (before.get(edge), after.get(edge)) else {
        return Vec::new();
    };
    let show = |v: Option<String>| v.unwrap_or_else(|| "none".to_owned());
    Prop::ALL
        .iter()
        .filter_map(|prop| {
            let (old, new) = (prop.show(&b.props), prop.show(&a.props));
            (old != new).then(|| format!("{} {}→{}", prop.name(), show(old), show(new)))
        })
        .collect()
}

#[cfg(test)]
mod tests;
