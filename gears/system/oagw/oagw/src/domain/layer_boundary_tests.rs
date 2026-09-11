//! The layer-boundary conformance test
//! (`cpt-cf-oagw-dod-gear-foundation-layer-boundaries`).
//!
//! The domain layer is the innermost layer of the gear's DDD-light layout and
//! must depend on neither of the two outer ones: it may not name `crate::api`
//! or `crate::infra`, and it may not reach past the crate at all. The property
//! is asserted against the delivered production sources, so a new module that
//! reaches outward fails here rather than at review time. Sibling `*_tests.rs`
//! harnesses are excluded: they drive the in-memory infrastructure store by
//! design.

#[test]
fn the_domain_layer_references_no_outer_layer() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/domain");
    let mut offenders = Vec::new();
    let mut visited = 0usize;
    let mut stack = vec![root.clone()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).expect("the domain directory is readable") {
            let path = entry.expect("the entry is readable").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
                continue;
            };
            // A sibling `*_tests.rs` module is a harness, not part of the
            // layer: the in-memory store the tests drive is the same
            // infrastructure the production sources are forbidden to name, and
            // the crate's convention puts its test modules beside the code
            // they cover.
            if !name.ends_with(".rs")
                || name == "layer_boundary_tests.rs"
                || name.ends_with("_tests.rs")
                || name == "tests.rs"
            {
                continue;
            }
            visited += 1;
            let source = std::fs::read_to_string(&path).expect("the source is readable");
            for (number, line) in source.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    // A comment is prose about the boundary, not a dependency.
                    continue;
                }
                for forbidden in ["crate::infra", "crate::api"] {
                    if line.contains(forbidden) {
                        offenders.push(format!(
                            "{}:{}: {forbidden}",
                            path.strip_prefix(&root).unwrap_or(&path).display(),
                            number + 1
                        ));
                    }
                }
            }
        }
    }
    assert!(
        visited > 10,
        "the sweep walked the delivered domain sources ({visited} files)"
    );
    assert!(
        offenders.is_empty(),
        "the domain layer reaches an outer layer: {offenders:#?}"
    );
}
