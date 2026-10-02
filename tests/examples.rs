//! Every file in `examples/` is a valid topology.

#[test]
fn examples_parse_and_allocate() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut count = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "toml") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let stem = path.file_stem().unwrap().to_string_lossy();
        let topo = tpo::model::Topology::parse(&src, &stem)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        tpo::addr::allocate(&topo, None).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        count += 1;
    }
    assert!(count >= 3);
}
