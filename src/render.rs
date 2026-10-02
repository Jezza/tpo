//! Human-readable output for `tpo show`.

use serde_json::Value;
use std::collections::HashSet;
use std::fmt::Write;

fn process(p: &Value) -> String {
    match p["state"].as_str() {
        None => "-".to_owned(),
        Some("running") => format!("running (pid {})", p["pid"]),
        Some("exited") => format!("exited({})", p["code"]),
        Some(other) => other.to_owned(),
    }
}

pub fn show(data: &Value) -> String {
    let mut out = String::new();
    let nodes = data["nodes"].as_array().cloned().unwrap_or_default();
    let _ = writeln!(
        out,
        "lab {} ({}, {} nodes{})",
        data["name"].as_str().unwrap_or("?"),
        data["mode"].as_str().unwrap_or("?"),
        nodes.len(),
        if data["internet"].as_bool() == Some(true) {
            ", internet"
        } else {
            ""
        }
    );
    if data["internet"].as_bool() == Some(true) {
        let up = &data["uplink"];
        let line = match (up["alive"].as_bool(), up["pasta_pid"].as_i64()) {
            (Some(true), Some(pid)) => format!("internet: up (pasta pid {pid})"),
            (Some(false), Some(pid)) => format!("internet: DEGRADED (pasta pid {pid} has exited)"),
            _ => "internet: DEGRADED (no uplink)".to_owned(),
        };
        let _ = writeln!(out, "{line}");
    }
    let width = nodes
        .iter()
        .filter_map(|n| n["name"].as_str())
        .map(str::len)
        .max()
        .unwrap_or(4)
        .max(4);
    let _ = writeln!(
        out,
        "\n{:width$}  {:15}  {:24}  PROCESS",
        "NODE", "IPV4", "IPV6"
    );
    for n in &nodes {
        let _ = writeln!(
            out,
            "{:width$}  {:15}  {:24}  {}",
            n["name"].as_str().unwrap_or("?"),
            n["ipv4"].as_str().unwrap_or("?"),
            n["ipv6"].as_str().unwrap_or("?"),
            process(&n["process"])
        );
    }

    let edges = data["edges"].as_array().cloned().unwrap_or_default();
    if edges.is_empty() {
        return out;
    }
    let _ = writeln!(out, "\nLINKS");
    let key = |e: &Value| {
        (
            e["from"].as_str().unwrap_or("").to_owned(),
            e["to"].as_str().unwrap_or("").to_owned(),
        )
    };
    let mut done = HashSet::new();
    for e in &edges {
        let (from, to) = key(e);
        if !done.insert((from.clone(), to.clone())) {
            continue;
        }
        let describe = |e: &Value| {
            let mut s = e["props"].as_str().unwrap_or("").to_owned();
            if e["up"].as_bool() == Some(false) {
                s = if s.is_empty() {
                    "CUT".to_owned()
                } else {
                    format!("CUT ({s})")
                };
            }
            if e["faulted"].as_bool() == Some(true) && e["up"].as_bool() != Some(false) {
                s.push_str(" *");
            }
            s
        };
        let reverse = edges.iter().find(|r| key(r) == (to.clone(), from.clone()));
        match reverse {
            Some(r) if describe(r) == describe(e) => {
                done.insert((to.clone(), from.clone()));
                let _ = writeln!(
                    out,
                    "  {}",
                    format!("{from}⇄{to}  {}", describe(e)).trim_end()
                );
            }
            _ => {
                let _ = writeln!(
                    out,
                    "  {}",
                    format!("{from}→{to}  {}", describe(e)).trim_end()
                );
            }
        }
    }
    out
}
