use super::*;
use crate::units::{Percent, Rate};
use std::time::Duration;

fn parse_ok(src: &str) -> Topology {
    match Topology::parse(src, "lab") {
        Ok(t) => t,
        Err(e) => panic!("expected topology to parse, got: {e}"),
    }
}

fn parse_err(src: &str) -> String {
    match Topology::parse(src, "lab") {
        Ok(_) => panic!("expected an error"),
        Err(e) => e.to_string(),
    }
}

fn edge(from: &str, to: &str) -> Edge {
    Edge::new(from, to)
}

const NODES_ABC: &str = r#"
[nodes]
a = {}
b = {}
c = {}
"#;

#[test]
fn defaults_name_mode_and_subnets() {
    let t = parse_ok(NODES_ABC);
    assert_eq!(t.name, "lab");
    assert_eq!(t.mode, Mode::Direct);
    assert_eq!(t.metric, Metric::Hops);
    assert_eq!(t.subnet.to_string(), "10.77.0.0/24");
    assert_eq!(t.subnet6.to_string(), "fd77::/64");
    assert!(t.edges.is_empty());
    assert!(!t.uses_internet());
}

#[test]
fn keeps_node_declaration_order() {
    let t = parse_ok("[nodes]\nzed = {}\nalpha = {}\nmid = {}\n");
    let names: Vec<_> = t.nodes.keys().cloned().collect();
    assert_eq!(names, ["zed", "alpha", "mid"]);
}

#[test]
fn reads_top_level_settings() {
    let t = parse_ok(
        r#"
name = "chain"
mode = "routed"
metric = "delay"
subnet = "10.9.0.0/16"
subnet6 = "fd09::/64"
[nodes]
a = {}
"#,
    );
    assert_eq!(t.name, "chain");
    assert_eq!(t.mode, Mode::Routed);
    assert_eq!(t.metric, Metric::Delay);
    assert_eq!(t.subnet.to_string(), "10.9.0.0/16");
}

#[test]
fn rejects_unknown_keys() {
    let err = parse_err("[nodes]\na = { adr = \"10.77.0.5\" }\n");
    assert!(err.contains("adr"), "{err}");
}

#[test]
fn rejects_invalid_and_reserved_node_names() {
    assert!(parse_err("[nodes]\nNode_1 = {}\n").contains("Node_1"));
    assert!(parse_err("[nodes]\ninternet = {}\n").contains("reserved"));
    assert!(parse_err("[nodes]\nhost = {}\n").contains("reserved"));
    let long = "a".repeat(64);
    assert!(parse_err(&format!("[nodes]\n{long} = {{}}\n")).contains(&long));
}

#[test]
fn rejects_group_names_that_clash_with_nodes() {
    let err = parse_err("[nodes]\na = {}\n[groups]\na = [\"a\"]\n");
    assert!(err.contains("`a`") && err.contains("both"), "{err}");
}

#[test]
fn rejects_groups_with_unknown_members() {
    let err = parse_err("[nodes]\na = {}\n[groups]\ng = [\"a\", \"nope\"]\n");
    assert!(err.contains("nope"), "{err}");
}

#[test]
fn path_links_consecutive_endpoints_in_both_directions() {
    let t = parse_ok(&format!(
        "{NODES_ABC}\n[[link]]\npath = [\"a\", \"b\", \"c\"]\n"
    ));
    let edges: Vec<_> = t.edges.keys().cloned().collect();
    assert_eq!(
        edges,
        [
            edge("a", "b"),
            edge("b", "a"),
            edge("b", "c"),
            edge("c", "b")
        ]
    );
}

#[test]
fn from_to_expands_groups_bipartitely() {
    let t = parse_ok(&format!(
        "{NODES_ABC}\nd = {{}}\n[groups]\nedge = [\"c\", \"d\"]\n[[link]]\nfrom = [\"a\", \"b\"]\nto = \"edge\"\n"
    ));
    for (f, to) in [("a", "c"), ("a", "d"), ("b", "c"), ("b", "d")] {
        assert!(t.edges.contains_key(&edge(f, to)), "{f}->{to}");
        assert!(t.edges.contains_key(&edge(to, f)), "{to}->{f}");
    }
    assert_eq!(t.edges.len(), 8);
}

#[test]
fn mesh_links_every_pair() {
    let t = parse_ok(&format!(
        "{NODES_ABC}\n[[link]]\nmesh = [\"a\", \"b\", \"c\"]\n"
    ));
    assert_eq!(t.edges.len(), 6);
}

#[test]
fn oneway_links_only_from_to() {
    let t = parse_ok(&format!(
        "{NODES_ABC}\n[[link]]\nfrom = \"a\"\nto = \"b\"\noneway = true\n"
    ));
    assert_eq!(
        t.edges.keys().cloned().collect::<Vec<_>>(),
        [edge("a", "b")]
    );
}

#[test]
fn oneway_is_rejected_for_path_mesh_and_internet() {
    assert!(
        parse_err(&format!(
            "{NODES_ABC}\n[[link]]\npath = [\"a\", \"b\"]\noneway = true\n"
        ))
        .contains("oneway")
    );
    assert!(
        parse_err(&format!(
            "{NODES_ABC}\n[[link]]\nmesh = [\"a\", \"b\"]\noneway = true\n"
        ))
        .contains("oneway")
    );
    assert!(
        parse_err(&format!(
            "{NODES_ABC}\n[[link]]\nfrom = \"a\"\nto = \"internet\"\noneway = true\n"
        ))
        .contains("internet")
    );
}

#[test]
fn a_link_needs_exactly_one_shape() {
    assert!(parse_err(&format!("{NODES_ABC}\n[[link]]\ndelay = \"1ms\"\n")).contains("path"));
    assert!(
        parse_err(&format!(
            "{NODES_ABC}\n[[link]]\nmesh = [\"a\",\"b\"]\npath = [\"a\",\"b\"]\n"
        ))
        .contains("only one")
    );
    assert!(parse_err(&format!("{NODES_ABC}\n[[link]]\nfrom = \"a\"\n")).contains("to"));
}

#[test]
fn self_links_are_ignored_but_empty_expansions_are_errors() {
    let t = parse_ok(&format!(
        "{NODES_ABC}\n[[link]]\nfrom = [\"a\", \"b\"]\nto = [\"a\", \"b\"]\n"
    ));
    assert_eq!(t.edges.len(), 2);
    let err = parse_err(&format!(
        "{NODES_ABC}\n[[link]]\nfrom = \"a\"\nto = \"a\"\n"
    ));
    assert!(err.contains("no links"), "{err}");
}

#[test]
fn unknown_link_endpoints_are_errors() {
    let err = parse_err(&format!("{NODES_ABC}\n[[link]]\npath = [\"a\", \"zz\"]\n"));
    assert!(err.contains("zz"), "{err}");
}

#[test]
fn properties_merge_defaults_then_links_in_order_per_direction() {
    let t = parse_ok(&format!(
        r#"{NODES_ABC}
[defaults]
delay = "1ms"
loss = "1%"
[[link]]
path = ["a", "b"]
delay = "20ms"
[[link]]
from = "a"
to = "b"
oneway = true
rate = "1mbit"
"#
    ));
    let ab = &t.edges[&edge("a", "b")];
    let ba = &t.edges[&edge("b", "a")];
    assert_eq!(ab.delay, Some(Duration::from_millis(20)));
    assert_eq!(ab.loss, Some(Percent(1.0)));
    assert_eq!(ab.rate, Some(Rate(1_000_000)));
    assert_eq!(ba.delay, Some(Duration::from_millis(20)));
    assert_eq!(ba.rate, None);
}

#[test]
fn later_oneway_entries_do_not_create_edges_on_their_own_direction_twice() {
    let t = parse_ok(&format!(
        "{NODES_ABC}\n[[link]]\npath=[\"a\",\"b\"]\n[[link]]\npath=[\"a\",\"b\"]\ndelay=\"5ms\"\n"
    ));
    assert_eq!(t.edges.len(), 2);
    assert_eq!(
        t.edges[&edge("a", "b")].delay,
        Some(Duration::from_millis(5))
    );
}

#[test]
fn property_values_are_validated() {
    let err = parse_err(&format!(
        "{NODES_ABC}\n[[link]]\npath=[\"a\",\"b\"]\ndelay=\"20\"\n"
    ));
    assert!(err.contains("delay") && err.contains("unit"), "{err}");
    let err = parse_err(&format!(
        "{NODES_ABC}\n[[link]]\npath=[\"a\",\"b\"]\njitter=\"2ms\"\n"
    ));
    assert!(err.contains("jitter") && err.contains("delay"), "{err}");
    let err = parse_err(&format!(
        "{NODES_ABC}\n[[link]]\npath=[\"a\",\"b\"]\nreorder=\"5%\"\n"
    ));
    assert!(err.contains("reorder") && err.contains("delay"), "{err}");
}

#[test]
fn internet_is_a_built_in_endpoint() {
    let t = parse_ok(&format!(
        "{NODES_ABC}\n[[link]]\nfrom = [\"a\",\"b\"]\nto = \"internet\"\ndelay = \"30ms\"\n[internet]\nhost_loopback = true\n"
    ));
    assert!(t.uses_internet());
    assert!(t.internet.host_loopback);
    assert!(t.edges.contains_key(&edge("a", INTERNET)));
    assert!(t.edges.contains_key(&edge(INTERNET, "a")));
    assert!(!t.nodes.contains_key(INTERNET));
}

#[test]
fn run_resolves_most_specific_entry() {
    let t = parse_ok(
        r#"
[nodes]
a = {}
b = {}
c = {}
d = {}
[groups]
edge = ["c", "d"]
[run]
"*" = "everyone"
"edge" = "edge-cmd"
"a" = "a-cmd"
"d" = ""
"#,
    );
    assert_eq!(t.run_command("a"), Some("a-cmd"));
    assert_eq!(t.run_command("b"), Some("everyone"));
    assert_eq!(t.run_command("c"), Some("edge-cmd"));
    assert_eq!(t.run_command("d"), None);
}

#[test]
fn run_comma_lists_count_each_part_at_its_own_level() {
    let t = parse_ok(
        "[nodes]\na={}\nb={}\nc={}\n[groups]\ng=[\"b\"]\n[run]\n\"a,g\" = \"x\"\n\"b\" = \"y\"\n",
    );
    assert_eq!(t.run_command("a"), Some("x"));
    assert_eq!(t.run_command("b"), Some("y"));
    assert_eq!(t.run_command("c"), None);
}

#[test]
fn run_rejects_duplicate_node_level_entries() {
    let err = parse_err("[nodes]\na={}\nb={}\n[run]\n\"a\" = \"x\"\n\"a,b\" = \"y\"\n");
    assert!(
        err.contains("`a`") && err.contains("more than once"),
        "{err}"
    );
}

#[test]
fn run_rejects_ambiguous_group_overlap_unless_node_entry_exists() {
    let src = "[nodes]\nd={}\ne={}\n[groups]\nedge=[\"d\",\"e\"]\nsensors=[\"d\"]\n[run]\nedge=\"x\"\nsensors=\"y\"\n";
    let err = parse_err(src);
    assert!(
        err.contains("`d`") && err.contains("edge") && err.contains("sensors"),
        "{err}"
    );
    let t = parse_ok(&format!("{src}d = \"z\"\n"));
    assert_eq!(t.run_command("d"), Some("z"));
    assert_eq!(t.run_command("e"), Some("x"));
}

#[test]
fn run_rejects_unknown_names() {
    let err = parse_err("[nodes]\na={}\n[run]\nzz = \"x\"\n");
    assert!(err.contains("zz"), "{err}");
}

#[test]
fn selectors_expand_nodes_groups_and_star() {
    let t = parse_ok("[nodes]\na={}\nb={}\nc={}\n[groups]\ng=[\"c\",\"a\"]\n");
    assert_eq!(t.select("b").unwrap(), ["b"]);
    assert_eq!(t.select("g").unwrap(), ["c", "a"]);
    assert_eq!(t.select("b,g").unwrap(), ["b", "c", "a"]);
    assert_eq!(t.select("*").unwrap(), ["a", "b", "c"]);
    assert!(t.select("nope").unwrap_err().to_string().contains("nope"));
}

#[test]
fn selectors_may_name_internet_when_the_lab_uses_it() {
    let t = parse_ok(&format!(
        "{NODES_ABC}\n[[link]]\nfrom=\"a\"\nto=\"internet\"\n"
    ));
    assert_eq!(t.select("internet").unwrap(), [INTERNET]);
    assert_eq!(t.select("*").unwrap(), ["a", "b", "c", INTERNET]);
    let t = parse_ok(NODES_ABC);
    assert!(t.select("internet").is_err());
}
