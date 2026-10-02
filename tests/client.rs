//! Driving a lab from Rust with `tpo::Client` (spec §9).

mod common;

use common::*;
use std::time::Duration;
use tpo::client::Client;

#[test]
fn a_harness_can_drive_a_lab_through_the_library() {
    if !userns_available() {
        return;
    }
    let lab = Lab::new(
        "library",
        "[nodes]\na = {}\nb = {}\n[[link]]\npath = [\"a\", \"b\"]\n[run]\n\"*\" = \"sleep 600\"\n",
    );
    lab.up();
    // SAFETY: this test binary has a single test, so nothing else reads the environment.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", lab.runtime_dir()) };

    let client = Client::connect(Some("library")).unwrap();
    let show = client.show().unwrap();
    assert_eq!(show["name"], "library");
    assert_eq!(show["nodes"].as_array().unwrap().len(), 2);

    let changes = client.cut(&["a", "b"], false, None).unwrap();
    assert_eq!(changes, ["cut a⇄b"]);
    assert!(!lab.exec("a", &["ping", "-c1", "-W1", "b"]).status.success());

    let changes = client.heal(&["a", "b"]).unwrap();
    assert_eq!(changes, ["heal a⇄b"]);

    let changes = client
        .set(
            &["a", "b"],
            &["delay=5ms"],
            true,
            Some(Duration::from_secs(60)),
        )
        .unwrap();
    assert_eq!(changes, ["a→b delay none→5ms"]);

    let lines = client.stop("b", None).unwrap();
    assert_eq!(lines, ["b: stopped"]);
    let lines = client.start("b").unwrap();
    assert!(lines[0].starts_with("b: started"), "{lines:?}");

    assert!(
        client
            .cut(&["a", "zz"], false, None)
            .unwrap_err()
            .contains("zz")
    );
    client.down().unwrap();
}
