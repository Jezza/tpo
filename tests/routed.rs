//! Routed mode, end to end (spec §4.2).

mod common;

use common::*;

const CHAIN: &str = r#"
mode = "routed"

[nodes]
a = {}
b = {}
c = {}

[[link]]
path = ["a", "b", "c"]
delay = "10ms"
"#;

const TRIANGLE: &str = r#"
mode = "routed"

[nodes]
a = {}
b = {}
c = {}

[[link]]
path = ["a", "b", "c"]
delay = "10ms"

[[link]]
from = "a"
to = "c"
delay = "50ms"
"#;

fn rtt(lab: &Lab, from: &str, to: &str, family: &str) -> Option<f64> {
    ping_rtt_ms(&lab.exec(from, &["ping", family, "-c1", "-W2", to]))
}

#[test]
fn packets_cross_intermediate_nodes_and_delays_add_up() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("multihop", CHAIN);
    lab.up();
    for family in ["-4", "-6"] {
        let r = rtt(&lab, "a", "c", family).unwrap_or_else(|| panic!("a reaches c over {family}"));
        assert!(
            (38.0..80.0).contains(&r),
            "two 10ms hops each way ≈ 40ms over {family}, got {r}"
        );
    }
}

#[test]
fn cutting_the_only_path_makes_the_far_node_unreachable() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("routedcut", CHAIN);
    lab.up();
    lab.ok(&["cut", "b", "c"]);
    assert!(rtt(&lab, "a", "c", "-4").is_none());
    assert!(
        rtt(&lab, "a", "b", "-4").is_some(),
        "the a–b link is untouched"
    );
    lab.ok(&["heal", "--all"]);
    assert!(rtt(&lab, "a", "c", "-4").is_some());
}

#[test]
fn cutting_a_link_fails_over_to_another_path() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("failover", TRIANGLE);
    lab.up();
    let direct = rtt(&lab, "a", "c", "-4").expect("a reaches c");
    assert!(
        direct > 95.0,
        "fewest hops: the direct 50ms link, got {direct}"
    );
    let out = lab.ok(&["cut", "a", "c"]);
    assert!(out.contains("cut a⇄c"), "{out}");
    let via_b = rtt(&lab, "a", "c", "-6").expect("a still reaches c, via b");
    assert!((38.0..80.0).contains(&via_b), "via b ≈ 40ms, got {via_b}");
}

#[test]
fn oneway_cuts_only_drop_one_direction() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("routed1way", CHAIN);
    lab.up();
    lab.ok(&["cut", "--oneway", "a", "b"]);
    // a's packets to b are dropped; b's to a still arrive, but no reply can come back.
    assert!(rtt(&lab, "a", "b", "-4").is_none());
    assert!(rtt(&lab, "b", "a", "-4").is_none());
    lab.ok(&["heal", "a", "b"]);
    assert!(rtt(&lab, "a", "b", "-4").is_some());
}

#[test]
fn apply_extends_a_routed_lab() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new("routedapply", CHAIN);
    lab.up();
    let next = lab.dir.path().join("next.toml");
    std::fs::write(
        &next,
        r#"
name = "routedapply"
mode = "routed"

[nodes]
a = {}
b = {}
c = {}
d = {}

[[link]]
path = ["a", "b", "c", "d"]
delay = "10ms"
"#,
    )
    .unwrap();
    lab.ok(&["apply", next.to_str().unwrap()]);
    assert!(
        rtt(&lab, "a", "d", "-4").is_some(),
        "a reaches the new node over three hops"
    );
}
