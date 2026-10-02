use super::*;
use crate::model::{Edge, INTERNET, Topology};
use crate::units::Percent;
use std::time::Duration;

fn topo(src: &str) -> Topology {
    Topology::parse(src, "lab").unwrap()
}

/// a—b—c chain, plus d and e meshed and linked to c; d and e form group `edge`.
fn lab() -> Topology {
    topo(
        r#"
[nodes]
a = {}
b = {}
c = {}
d = {}
e = {}
[groups]
edge = ["d", "e"]
[[link]]
path = ["a", "b", "c"]
delay = "10ms"
[[link]]
from = "c"
to = "edge"
[[link]]
mesh = "edge"
"#,
    )
}

fn e(from: &str, to: &str) -> Edge {
    Edge::new(from, to)
}

fn sorted(mut edges: Vec<Edge>) -> Vec<Edge> {
    edges.sort();
    edges
}

// ---- sides -------------------------------------------------------------------------------

#[test]
fn two_sides_select_edges_between_them_in_both_directions() {
    let t = lab();
    let edges = edges_between(&t, &["a", "b"], false).unwrap();
    assert_eq!(sorted(edges), [e("a", "b"), e("b", "a")]);
}

#[test]
fn one_side_means_against_everyone_else_and_keeps_internal_links() {
    let t = lab();
    let edges = edges_between(&t, &["edge"], false).unwrap();
    assert_eq!(
        sorted(edges),
        [e("c", "d"), e("c", "e"), e("d", "c"), e("e", "c")]
    );
}

#[test]
fn star_side_is_the_complement_of_the_other_sides() {
    let t = lab();
    let edges = edges_between(&t, &["b", "*"], false).unwrap();
    assert_eq!(
        sorted(edges),
        [e("a", "b"), e("b", "a"), e("b", "c"), e("c", "b")]
    );
}

#[test]
fn separate_sides_cut_pairwise() {
    let t = lab();
    let edges = edges_between(&t, &["d", "e", "*"], false).unwrap();
    assert_eq!(
        sorted(edges),
        [
            e("c", "d"),
            e("c", "e"),
            e("d", "c"),
            e("d", "e"),
            e("e", "c"),
            e("e", "d")
        ]
    );
}

#[test]
fn oneway_selects_first_to_second_only() {
    let t = lab();
    assert_eq!(edges_between(&t, &["a", "b"], true).unwrap(), [e("a", "b")]);
    let err = edges_between(&t, &["a", "b", "c"], true).unwrap_err();
    assert!(err.to_string().contains("exactly two"), "{err}");
}

#[test]
fn overlapping_sides_and_empty_selections_are_errors() {
    let t = lab();
    let err = edges_between(&t, &["a,b", "b,c"], false).unwrap_err();
    assert!(err.to_string().contains("`b`"), "{err}");
    let err = edges_between(&t, &["a", "c"], false).unwrap_err();
    assert!(err.to_string().contains("no links"), "{err}");
    let err = edges_between(&t, &["*", "*"], false).unwrap_err();
    assert!(err.to_string().contains("`*`"), "{err}");
}

#[test]
fn oneway_to_internet_is_rejected() {
    let t = topo("[nodes]\na={}\n[[link]]\nfrom=\"a\"\nto=\"internet\"\n");
    assert_eq!(
        sorted(edges_between(&t, &["a"], false).unwrap()),
        [e("a", INTERNET), e(INTERNET, "a")]
    );
    let err = edges_between(&t, &["a", "internet"], true).unwrap_err();
    assert!(err.to_string().contains("internet"), "{err}");
}

// ---- faults ------------------------------------------------------------------------------

#[test]
fn cut_marks_edges_down_and_heal_restores_the_spec() {
    let t = lab();
    let mut f = Faults::default();
    f.cut(&[e("a", "b")], Until::FOREVER);
    let eff = effective(&t, &f, 0);
    assert!(!eff[&e("a", "b")].up);
    assert!(eff[&e("b", "a")].up);
    f.heal(&[e("a", "b")]);
    assert_eq!(effective(&t, &f, 0), effective(&t, &Faults::default(), 0));
}

#[test]
fn set_overrides_properties_and_empty_value_drops_the_override() {
    let t = lab();
    let mut f = Faults::default();
    let ab = [e("a", "b")];
    f.set(
        &t,
        &ab,
        &parse_assignments(&["delay=200ms", "loss=5%"]).unwrap(),
        Until::FOREVER,
    )
    .unwrap();
    let eff = effective(&t, &f, 0);
    assert_eq!(
        eff[&e("a", "b")].props.delay,
        Some(Duration::from_millis(200))
    );
    assert_eq!(eff[&e("a", "b")].props.loss, Some(Percent(5.0)));

    f.set(
        &t,
        &ab,
        &parse_assignments(&["delay="]).unwrap(),
        Until::FOREVER,
    )
    .unwrap();
    let eff = effective(&t, &f, 0);
    assert_eq!(
        eff[&e("a", "b")].props.delay,
        Some(Duration::from_millis(10)),
        "back to the spec"
    );
    assert_eq!(eff[&e("a", "b")].props.loss, Some(Percent(5.0)));
}

#[test]
fn set_zero_forces_zero() {
    let t = lab();
    let mut f = Faults::default();
    f.set(
        &t,
        &[e("a", "b")],
        &parse_assignments(&["delay=0"]).unwrap(),
        Until::FOREVER,
    )
    .unwrap();
    assert_eq!(
        effective(&t, &f, 0)[&e("a", "b")].props.delay,
        Some(Duration::ZERO)
    );
}

#[test]
fn set_rejects_unknown_keys_bad_values_and_invalid_combinations() {
    let t = lab();
    assert!(
        parse_assignments(&["latency=5ms"])
            .unwrap_err()
            .to_string()
            .contains("latency")
    );
    assert!(
        parse_assignments(&["delay=5"])
            .unwrap_err()
            .to_string()
            .contains("unit")
    );
    assert!(
        parse_assignments(&["delay"])
            .unwrap_err()
            .to_string()
            .contains("=")
    );
    let mut f = Faults::default();
    let err = f
        .set(
            &t,
            &[e("c", "d")],
            &parse_assignments(&["jitter=5ms"]).unwrap(),
            Until::FOREVER,
        )
        .unwrap_err();
    assert!(err.to_string().contains("jitter"), "{err}");
    assert_eq!(f, Faults::default(), "a rejected set changes nothing");
}

#[test]
fn timed_faults_expire() {
    let t = lab();
    let mut f = Faults::default();
    f.cut(&[e("a", "b")], Until::at(1_000));
    f.set(
        &t,
        &[e("b", "c")],
        &parse_assignments(&["loss=50%"]).unwrap(),
        Until::at(2_000),
    )
    .unwrap();
    assert_eq!(f.next_deadline(), Some(1_000));
    assert!(!effective(&t, &f, 999)[&e("a", "b")].up);
    assert!(f.expire(1_000));
    assert!(effective(&t, &f, 1_000)[&e("a", "b")].up);
    assert_eq!(f.next_deadline(), Some(2_000));
    assert!(f.expire(2_000));
    assert_eq!(f, Faults::default());
    assert!(!f.expire(3_000));
}

#[test]
fn faults_for_edges_that_no_longer_exist_are_dropped_on_apply() {
    let mut f = Faults::default();
    f.cut(&[e("a", "b"), e("b", "c")], Until::FOREVER);
    let smaller = topo("[nodes]\na={}\nb={}\n[[link]]\npath=[\"a\",\"b\"]\n");
    f.retain_edges(&smaller);
    assert_eq!(f.edges.keys().cloned().collect::<Vec<_>>(), [e("a", "b")]);
}

#[test]
fn describes_changes_between_effective_states() {
    let t = lab();
    let before = effective(&t, &Faults::default(), 0);
    let mut f = Faults::default();
    f.cut(&[e("a", "b"), e("b", "a")], Until::FOREVER);
    f.set(
        &t,
        &[e("b", "c")],
        &parse_assignments(&["delay=200ms"]).unwrap(),
        Until::FOREVER,
    )
    .unwrap();
    let after = effective(&t, &f, 0);
    let lines = describe_changes(&before, &after);
    assert_eq!(lines, ["cut a⇄b", "b→c delay 10ms→200ms"]);
}
