//! Request handlers (spec §7.4) and reconciliation (spec §7.5).

use super::procs;
use super::{Lab, Shared, now_ms};
use crate::backend;
use crate::model::{INTERNET, Topology};
use crate::plan::direct::{self, Ctx};
use crate::plan::{
    Faults, Target, Until, describe_changes, describe_props, edges_between, effective,
    parse_assignments,
};
use crate::proto::{Request, Response};
use serde_json::{Value, json};
use std::time::Duration;

pub fn handle(shared: &Shared, request: Request) -> Response {
    let result = match request {
        Request::Show => Ok((vec![], show(&shared.lock().unwrap()))),
        Request::Explain { node } => explain(&shared.lock().unwrap(), &node).map(|d| (vec![], d)),
        Request::Cut {
            sides,
            oneway,
            for_ms,
        } => change_faults(shared, |lab, faults| {
            let edges =
                edges_between(&lab.topo, &refs(&sides), oneway).map_err(|e| e.to_string())?;
            faults.cut(&edges, until(for_ms));
            Ok(())
        }),
        Request::Set {
            sides,
            assignments,
            oneway,
            for_ms,
        } => change_faults(shared, |lab, faults| {
            let assignments = parse_assignments(&refs(&assignments)).map_err(|e| e.to_string())?;
            if assignments.is_empty() {
                return Err("give at least one `key=value`".to_owned());
            }
            let edges =
                edges_between(&lab.topo, &refs(&sides), oneway).map_err(|e| e.to_string())?;
            faults
                .set(&lab.topo, &edges, &assignments, until(for_ms))
                .map_err(|e| e.to_string())
        }),
        Request::Heal { sides, all } => change_faults(shared, |lab, faults| {
            if all {
                faults.heal_all();
                return Ok(());
            }
            let edges =
                edges_between(&lab.topo, &refs(&sides), false).map_err(|e| e.to_string())?;
            faults.heal(&edges);
            Ok(())
        }),
        Request::Stop {
            selector,
            signal,
            grace_ms,
            for_ms,
        } => stop(shared, &selector, signal, grace_ms, for_ms),
        Request::Start { selector } => each_node(shared, &selector, |s, n| {
            procs::start(s, n).unwrap_or_else(|e| format!("{n}: {e}"))
        }),
        Request::Restart { selector } => each_node(shared, &selector, |s, n| {
            let stopped = procs::stop(
                s,
                n,
                nix::sys::signal::Signal::SIGTERM,
                Duration::from_secs(5),
            );
            let started = procs::start(s, n).unwrap_or_else(|e| format!("{n}: {e}"));
            if stopped.ends_with("no command") {
                stopped
            } else {
                started
            }
        }),
        Request::Resync { selector } => resync(shared, selector.as_deref()),
        Request::Apply { .. } => Err("`apply` is not implemented yet".to_owned()),
        Request::Down | Request::Enter { .. } => unreachable!("handled by the server"),
    };
    match result {
        Ok((changes, data)) => Response::ok(changes, data),
        Err(e) => Response::err(e),
    }
}

fn refs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

fn until(for_ms: Option<u64>) -> Until {
    for_ms.map_or(Until::FOREVER, |ms| Until::at(now_ms() + ms))
}

type Outcome = Result<(Vec<String>, Value), String>;

/// Applies a change to a copy of the faults and reconciles; on any error nothing changes.
fn change_faults(
    shared: &Shared,
    f: impl FnOnce(&Lab, &mut Faults) -> Result<(), String>,
) -> Outcome {
    let mut lab = shared.lock().unwrap();
    let mut faults = lab.faults.clone();
    f(&lab, &mut faults)?;
    let previous = std::mem::replace(&mut lab.faults, faults);
    match lab.reconcile() {
        Ok(changes) => {
            lab.save();
            lab.wake.notify_all();
            Ok((changes, Value::Null))
        }
        Err(e) => {
            lab.faults = previous;
            Err(e)
        }
    }
}

impl Lab {
    /// Brings the namespaces to spec ⊕ faults, returning the human-readable changes. On failure
    /// the affected nodes are rebuilt to the previous state (spec §8).
    pub fn reconcile(&mut self) -> Result<Vec<String>, String> {
        let eff = effective(&self.topo, &self.faults, now_ms());
        let desired = direct::desired(&Ctx::new(&self.topo, &self.addrs), &eff);
        let ops = direct::diff(&self.desired, &desired);
        match backend::run(&self.netfds(), &self.tools, &ops) {
            Ok(()) => {
                let changes = describe_changes(&self.eff, &eff);
                self.eff = eff;
                self.desired = desired;
                Ok(changes)
            }
            Err(e) => {
                let targets: Vec<Target> = ops.iter().map(|o| o.target.clone()).collect();
                if let Err(again) = self.rebuild(&targets) {
                    eprintln!("tpo: rebuilding after a failed change also failed: {again}");
                }
                Err(e)
            }
        }
    }

    /// Rebuilds the tc/neighbour/route state of `targets` from scratch to `self.desired`.
    pub fn rebuild(&mut self, targets: &[Target]) -> Result<(), String> {
        let mut ops = Vec::new();
        for t in targets.iter().filter(|t| **t != Target::Hub) {
            let Some(net) = self.desired.get(t) else {
                continue;
            };
            ops.extend(direct::reset_ops(t));
            ops.extend(direct::diff_from_reset(t, net));
        }
        backend::run(&self.netfds(), &self.tools, &ops)
    }
}

fn resync(shared: &Shared, selector: Option<&str>) -> Outcome {
    let mut lab = shared.lock().unwrap();
    let targets: Vec<Target> = match selector {
        None => lab.desired.keys().cloned().collect(),
        Some(sel) => lab
            .topo
            .select(sel)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|n| {
                if n == INTERNET {
                    Target::Gateway
                } else {
                    Target::node(n)
                }
            })
            .collect(),
    };
    lab.rebuild(&targets)?;
    Ok((
        targets.iter().map(|t| format!("{t}: rebuilt")).collect(),
        Value::Null,
    ))
}

fn nodes_of(topo: &Topology, selector: &str) -> Result<Vec<String>, String> {
    let nodes = topo.select(selector).map_err(|e| e.to_string())?;
    Ok(nodes.into_iter().filter(|n| n != INTERNET).collect())
}

fn each_node(
    shared: &Shared,
    selector: &str,
    f: impl Fn(&Shared, &str) -> String + Sync,
) -> Outcome {
    let nodes = nodes_of(&shared.lock().unwrap().topo, selector)?;
    let lines = std::thread::scope(|scope| {
        let handles: Vec<_> = nodes.iter().map(|n| scope.spawn(|| f(shared, n))).collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_default())
            .collect()
    });
    Ok((lines, Value::Null))
}

fn stop(
    shared: &Shared,
    selector: &str,
    signal: Option<String>,
    grace_ms: Option<u64>,
    for_ms: Option<u64>,
) -> Outcome {
    let signal = match signal {
        None => nix::sys::signal::Signal::SIGTERM,
        Some(s) => {
            let name = s.to_uppercase();
            let name = if name.starts_with("SIG") {
                name
            } else {
                format!("SIG{name}")
            };
            name.parse().map_err(|_| format!("unknown signal `{s}`"))?
        }
    };
    let grace = Duration::from_millis(grace_ms.unwrap_or(5_000));
    let outcome = each_node(shared, selector, |s, n| procs::stop(s, n, signal, grace))?;
    if let Some(ms) = for_ms {
        let shared = shared.clone();
        let nodes = nodes_of(&shared.lock().unwrap().topo, selector)?;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            for n in nodes {
                let _ = procs::start(&shared, &n);
            }
        });
    }
    Ok(outcome)
}

fn show(lab: &Lab) -> Value {
    let nodes: Vec<Value> = lab
        .topo
        .nodes
        .keys()
        .map(|n| {
            let a = &lab.addrs.nodes[n];
            let process = lab.procs.get(n).map_or(Value::Null, |p| json!(p));
            json!({ "name": n, "ipv4": a.v4, "ipv6": a.v6, "process": process })
        })
        .collect();
    let edges: Vec<Value> = lab
        .eff
        .iter()
        .map(|(e, eff)| {
            json!({
                "from": e.from,
                "to": e.to,
                "up": eff.up,
                "props": describe_props(&eff.props),
                "faulted": lab.faults.edges.contains_key(e),
            })
        })
        .collect();
    json!({
        "name": lab.topo.name,
        "mode": lab.topo.mode,
        "file": lab.file,
        "nodes": nodes,
        "edges": edges,
        "internet": lab.topo.uses_internet(),
    })
}

fn explain(lab: &Lab, node: &str) -> Result<Value, String> {
    let target = if node == INTERNET || node == "gateway" {
        Target::Gateway
    } else {
        Target::node(node)
    };
    let net = lab
        .desired
        .get(&target)
        .ok_or_else(|| format!("unknown node `{node}`"))?;
    let lines: Vec<String> = direct::explain_ops(&target, net)
        .iter()
        .map(ToString::to_string)
        .collect();
    Ok(json!({ "ops": lines }))
}
