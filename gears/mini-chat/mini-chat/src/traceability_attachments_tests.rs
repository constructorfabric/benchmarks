//! Test layering and traceability of `cpt-cf-mini-chat-feature-attachments-and-file-search`,
//! checked against the sources: no test under `testing/e2e/suites/mini_chat/`, every marker of
//! the feature names a step of the FEATURE (no invented ID), every implemented (checked) step
//! carries markers, a step with markers is checked, at the end of the feature every step carries
//! markers (the transition `inst-vsm-chat-cleanup` is realized by
//! `cpt-cf-mini-chat-feature-background-maintenance`), and every definition of done is named by the
//! `//!` header of at least one test module that verifies it.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! definition of done `cpt-cf-mini-chat-dod-attachments-and-file-search-tests-and-deviations`
//! (traceability mode FULL).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const FEATURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../docs/features/0005-attachments-and-file-search.md"
);
const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
const E2E_SUITE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../testing/e2e/suites/mini_chat"
);
const PREFIX: &str = "attachments-and-file-search-";

/// The one step this feature leaves to `cpt-cf-mini-chat-feature-background-maintenance`, which
/// realizes it.
const REALIZED_BY_MAINTENANCE: (&str, &str) = (
    "cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store",
    "inst-vsm-chat-cleanup",
);

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

/// The CDSL steps of the flows, algorithms and states of the FEATURE.
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
    for (path, text) in sources() {
        // this module quotes marker syntax in its documentation only
        if path.ends_with("traceability_attachments_tests.rs") {
            continue;
        }
        for (construct, _, inst) in markers(&text, "begin") {
            if construct.contains(PREFIX) {
                *found.entry((construct, inst)).or_insert(0) += 1;
            }
        }
    }
    found
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
    assert!(steps.len() > 300, "{} steps parsed", steps.len());
    for inst in [
        "inst-del-request",
        "inst-ch-parse",
        "inst-rp-loop",
        "inst-kl-iterations",
    ] {
        assert!(steps.iter().any(|step| step.inst == inst), "{inst}");
    }
}

// ── markers ──────────────────────────────────────────────────────────────────

#[test]
fn every_attachments_marker_names_a_step_of_the_feature() {
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

/// Fails until the author stage of slice S2-deletion-cleanup-tools: the markers of every step of
/// the delete flow, the ask-about-files flow, the admin flow, the delete transaction, the cleanup
/// enqueue, the cleanup handler, the upload reaper, the two fact algorithms, the three
/// contributions, the knowledge loop, `inst-err-locked`, the metrics and configuration steps and
/// the cleanup status state. The one step this feature left to
/// `cpt-cf-mini-chat-feature-background-maintenance` (`inst-vsm-chat-cleanup`) is realized by that
/// feature's slice S1-all and carries its markers too.
#[test]
fn at_the_end_of_the_slice_every_step_carries_markers() {
    let markers = feature_markers();
    let missing: Vec<String> = feature_steps()
        .into_iter()
        .filter(|step| !markers.contains_key(&(step.construct.clone(), step.inst.clone())))
        .map(|step| format!("{} / {}", step.construct, step.inst))
        .collect();
    assert!(missing.is_empty(), "steps without markers: {missing:#?}");
}

/// Fails until the author stage of slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`:
/// the transition "bound to absent when chat cleanup deleted the provider store" is realized by the
/// chat-cleanup handler of that feature (the vector store cleanup algorithm), and the FEATURE
/// checkbox of the step is then checked (`a_step_with_markers_is_checked_in_the_feature`).
#[test]
fn the_chat_vector_store_cleanup_step_is_realized_by_background_maintenance() {
    let markers = feature_markers();
    assert!(
        markers.contains_key(&(
            REALIZED_BY_MAINTENANCE.0.to_owned(),
            REALIZED_BY_MAINTENANCE.1.to_owned()
        )),
        "`inst-vsm-chat-cleanup` has no markers in the chat-cleanup handler"
    );
    let realized_in_maintenance = sources().into_iter().any(|(path, text)| {
        path.to_string_lossy().contains("domain/maintenance")
            && markers_of(&text).iter().any(|(construct, inst)| {
                construct == REALIZED_BY_MAINTENANCE.0 && inst == REALIZED_BY_MAINTENANCE.1
            })
    });
    assert!(
        realized_in_maintenance,
        "the markers belong to `domain/maintenance`"
    );
}

/// The `(construct, step)` pairs of the begin markers of a source text.
fn markers_of(text: &str) -> Vec<(String, String)> {
    markers(text, "begin")
        .into_iter()
        .map(|(construct, _, inst)| (construct, inst))
        .collect()
}

// ── definitions of done ──────────────────────────────────────────────────────

/// The `cpt-cf-mini-chat-dod-attachments-and-file-search-*` IDs of the FEATURE.
fn definitions_of_done() -> BTreeSet<String> {
    let text = fs::read_to_string(FEATURE).unwrap();
    text.lines()
        .filter_map(|line| line.split("**ID**: `").nth(1))
        .map(|rest| rest.split('`').next().unwrap().to_owned())
        .filter(|id| id.starts_with("cpt-cf-mini-chat-dod-attachments-and-file-search-"))
        .collect()
}

/// The `//!` header of a source file.
fn module_doc(text: &str) -> String {
    text.lines()
        .take_while(|line| line.starts_with("//!") || line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_definition_of_done_is_named_by_a_test_module_doc() {
    let ids = definitions_of_done();
    assert_eq!(ids.len(), 14, "{ids:#?}");
    let docs: Vec<String> = sources()
        .into_iter()
        .filter(|(path, _)| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("_tests.rs"))
        })
        .map(|(_, text)| module_doc(&text))
        .collect();
    for dod in &ids {
        let named = docs.iter().filter(|doc| doc.contains(dod.as_str())).count();
        assert!(
            named >= 1,
            "`{dod}` is named by the `//!` header of no test module"
        );
    }
}

#[test]
fn every_slice_s2_definition_of_done_is_verified_by_a_test_module() {
    // the DoDs this slice owns, each named by the header of a test module of the slice
    let docs: Vec<String> = sources()
        .into_iter()
        .filter(|(path, _)| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("_tests.rs"))
        })
        .map(|(_, text)| module_doc(&text))
        .filter(|doc| doc.contains("S2-deletion-cleanup-tools"))
        .collect();
    for suffix in [
        "delete-api",
        "cleanup-handler",
        "upload-reaper",
        "tool-contributions",
        "knowledge-loop",
        "errors-observability",
        "deferred-scope",
        "tests-and-deviations",
    ] {
        let id = format!("cpt-cf-mini-chat-dod-attachments-and-file-search-{suffix}");
        assert!(
            docs.iter().any(|doc| doc.contains(&id)),
            "`{id}` is named by no S2 test module"
        );
    }
}
