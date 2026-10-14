//! Test layering and traceability of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`,
//! checked against the sources: no test under `testing/e2e/suites/mini_chat/`, every `@cpt-begin`
//! has its `@cpt-end`, every marker of the feature names a step of the FEATURE (no invented ID),
//! every implemented (checked) step carries markers, a step with markers is checked, at the end of
//! the feature every step carries markers, every flow, algorithm and state of the FEATURE and
//! every definition of done is named by the `//!` header of at least one test module that
//! verifies it.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: definition of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-tests-and-deviations` (traceability mode
//! FULL).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const FEATURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../docs/features/0006-turn-mutations-and-reactions.md"
);
const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
const E2E_SUITE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../testing/e2e/suites/mini_chat"
);
const PREFIX: &str = "turn-mutations-and-reactions-";

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn sources() -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    rust_files(Path::new(SRC), &mut files);
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let text = fs::read_to_string(&path).unwrap();
            (path, text)
        })
        .collect()
}

/// `(construct id, phase, step id)` of the markers `kind` (`begin` or `end`) in `text`.
fn markers(text: &str, kind: &str) -> Vec<(String, String, String)> {
    let needle = format!("@cpt-{kind}:");
    let mut found = Vec::new();
    for line in text.lines() {
        let Some(at) = line.find(&needle) else {
            continue;
        };
        let parts: Vec<&str> = line[at + needle.len()..]
            .split(|c: char| c.is_whitespace())
            .next()
            .unwrap()
            .split(':')
            .collect();
        if parts.len() == 3 {
            found.push((
                parts[0].to_owned(),
                parts[1].to_owned(),
                parts[2].to_owned(),
            ));
        }
    }
    found
}

struct Step {
    construct: String,
    inst: String,
    checked: bool,
}

/// The `**ID**` of every construct of the FEATURE (flows, algorithms, states, definitions of done).
fn construct_ids() -> Vec<String> {
    let text = fs::read_to_string(FEATURE).unwrap();
    text.lines()
        .filter_map(|line| line.split("**ID**: `").nth(1))
        .map(|rest| rest.split('`').next().unwrap().to_owned())
        .filter(|id| id.contains(PREFIX))
        .collect()
}

/// The CDSL steps of the FEATURE flows, algorithms and states.
fn feature_steps() -> Vec<Step> {
    let text = fs::read_to_string(FEATURE).unwrap();
    let mut current: Option<String> = None;
    let mut steps = Vec::new();
    for line in text.lines() {
        if let Some(at) = line.find("**ID**: `") {
            let id = line[at + 9..].split('`').next().unwrap();
            let is_construct = ["-flow-", "-algo-", "-state-"]
                .iter()
                .any(|kind| id.contains(kind) && id.contains(PREFIX));
            current = is_construct.then(|| id.to_owned());
            continue;
        }
        let Some(construct) = &current else {
            continue;
        };
        let trimmed = line.trim_start();
        let after_bullet = trimmed
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches('.')
            .trim_start_matches('-')
            .trim_start();
        let checked = after_bullet.starts_with("[x]");
        if !(checked || after_bullet.starts_with("[ ]")) {
            continue;
        }
        let Some(inst) = line
            .trim_end()
            .strip_suffix('`')
            .and_then(|head| head.rsplit('`').next())
            .filter(|token| token.starts_with("inst-"))
        else {
            continue;
        };
        steps.push(Step {
            construct: construct.clone(),
            inst: inst.to_owned(),
            checked,
        });
    }
    steps
}

fn feature_markers() -> BTreeMap<(String, String), usize> {
    let mut found = BTreeMap::new();
    for (_, text) in sources() {
        for (construct, _, inst) in markers(&text, "begin") {
            if construct.contains(PREFIX) {
                *found.entry((construct, inst)).or_insert(0) += 1;
            }
        }
    }
    found
}

/// The `//!` header of a source file.
fn module_doc(text: &str) -> String {
    text.lines()
        .take_while(|line| line.starts_with("//!") || line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `//!` headers of the test modules of the gear.
fn test_docs() -> Vec<String> {
    sources()
        .into_iter()
        .filter(|(path, _)| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("_tests.rs"))
        })
        .map(|(_, text)| module_doc(&text))
        .collect()
}

// ── layering ─────────────────────────────────────────────────────────────────

#[test]
fn no_test_is_added_under_the_e2e_mini_chat_suite() {
    assert!(
        !Path::new(E2E_SUITE).exists(),
        "`testing/e2e/suites/mini_chat/` must not exist: the feature is tested in the gear crate"
    );
}

#[test]
fn the_feature_file_has_steps_to_trace() {
    let steps = feature_steps();
    assert!(steps.len() > 250, "{} steps parsed", steps.len());
    assert!(steps.iter().any(|step| step.inst == "inst-mc-atomic"));
    assert!(steps.iter().any(|step| step.inst == "inst-ae-enqueue"));
    assert_eq!(
        construct_ids().len(),
        44,
        "the feature status entry, 6 flows, 20 algorithms, 3 states and 14 definitions of done"
    );
}

// ── markers ──────────────────────────────────────────────────────────────────

#[test]
fn every_begin_marker_has_its_end_marker_and_a_phase_token() {
    let mut begins: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut ends: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (path, text) in sources() {
        for (kind, table) in [("begin", &mut begins), ("end", &mut ends)] {
            for (construct, phase, inst) in markers(&text, kind) {
                assert!(
                    phase.starts_with('p') && phase[1..].chars().all(|c| c.is_ascii_digit()),
                    "{}: `{construct}:{phase}:{inst}` has no phase token",
                    path.display()
                );
                assert!(inst.starts_with("inst-"), "{}: {inst}", path.display());
                *table.entry((construct, inst)).or_insert(0) += 1;
            }
        }
    }
    assert_eq!(begins, ends, "unbalanced @cpt-begin / @cpt-end markers");
}

#[test]
fn every_marker_of_the_feature_names_a_step_of_the_feature() {
    let known: BTreeSet<(String, String)> = feature_steps()
        .into_iter()
        .map(|step| (step.construct, step.inst))
        .collect();
    for (construct, inst) in feature_markers().keys() {
        assert!(
            known.contains(&(construct.clone(), inst.clone())),
            "`{construct}:{inst}` is not a step of the FEATURE (no invented marker IDs)"
        );
    }
}

#[test]
fn every_implemented_step_of_the_feature_carries_markers() {
    let markers = feature_markers();
    for step in feature_steps().into_iter().filter(|step| step.checked) {
        assert!(
            markers.contains_key(&(step.construct.clone(), step.inst.clone())),
            "the checked step `{}` of `{}` has no @cpt-begin/@cpt-end markers",
            step.inst,
            step.construct
        );
    }
}

#[test]
fn a_step_with_markers_is_checked_in_the_feature() {
    let markers = feature_markers();
    for step in feature_steps().into_iter().filter(|step| !step.checked) {
        assert!(
            !markers.contains_key(&(step.construct.clone(), step.inst.clone())),
            "`{}` of `{}` is implemented (marked) but unchecked in the FEATURE",
            step.inst,
            step.construct
        );
    }
}

/// Fails until the author stage of slice S1-all: no step of the FEATURE carries markers yet.
#[test]
fn at_the_end_of_the_feature_every_step_carries_markers() {
    let markers = feature_markers();
    let missing: Vec<String> = feature_steps()
        .into_iter()
        .filter(|step| !markers.contains_key(&(step.construct.clone(), step.inst.clone())))
        .map(|step| format!("{} / {}", step.construct, step.inst))
        .collect();
    assert!(
        missing.is_empty(),
        "{} steps without markers: {missing:#?}",
        missing.len()
    );
}

// ── flows, algorithms, states and definitions of done ────────────────────────

#[test]
fn every_flow_algorithm_and_state_is_named_by_a_test_module_doc() {
    let docs = test_docs();
    let constructs: Vec<String> = construct_ids()
        .into_iter()
        .filter(|id| {
            ["-flow-", "-algo-", "-state-"]
                .iter()
                .any(|kind| id.contains(kind))
        })
        .collect();
    assert_eq!(constructs.len(), 29, "{constructs:#?}");
    for id in &constructs {
        assert!(
            docs.iter().any(|doc| doc.contains(id.as_str())),
            "`{id}` is named by the `//!` header of no test module"
        );
    }
}

#[test]
fn every_definition_of_done_is_named_by_a_test_module_doc() {
    let ids: BTreeSet<String> = construct_ids()
        .into_iter()
        .filter(|id| id.contains("-dod-"))
        .collect();
    assert_eq!(ids.len(), 14, "{ids:#?}");
    let docs = test_docs();
    for dod in &ids {
        let named = docs.iter().filter(|doc| doc.contains(dod.as_str())).count();
        assert!(
            named >= 1,
            "`{dod}` is named by the `//!` header of no test module"
        );
    }
}

#[test]
fn every_step_of_the_feature_is_named_by_a_test_or_a_marker_site() {
    // the test headers name the steps they verify; a step that no test names has no test
    let text: String = sources()
        .into_iter()
        .filter(|(path, _)| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.ends_with("_tests.rs")
            })
        })
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n");
    let unnamed: Vec<String> = feature_steps()
        .into_iter()
        .filter(|step| !text.contains(&step.inst))
        .map(|step| format!("{} / {}", step.construct, step.inst))
        .collect();
    assert!(
        unnamed.is_empty(),
        "{} steps named by no test: {unnamed:#?}",
        unnamed.len()
    );
}
