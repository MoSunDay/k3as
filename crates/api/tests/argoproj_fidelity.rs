//! JSON round-trip fidelity for `argoproj.io/v1alpha1` (T7.3 Scope A, Q30/Q10).
//!
//! Fixtures are upstream Argo Workflows example YAMLs converted to JSON (see
//! `fixtures/argoproj/convert.sh` for provenance); JSON is the sole wire
//! format (Q10). Every fixture is decoded into its typed port
//! ([`argoproj::Workflow`] or [`argoproj::CronWorkflow`], selected by
//! `.kind`), re-encoded, and asserted to be:
//!   (1) idempotent: encode(decode(encode(obj))) == encode(obj) — the wire
//!       form is byte-stable, and
//!   (2) semantically lossless: the re-decoded typed object re-encodes to the
//!       same value (and equals the first decoded object).
//! Because the port mirrors Go `omitempty` tags, fields that upstream also
//! drops (e.g. an explicit `"suspend": false`) are not compared against the
//! raw input — only against the port's own stable wire form.

use std::fs;
use std::path::PathBuf;

use api::argoproj::{self, CronWorkflow, Workflow};
use api::discovery::{api_group_list, api_resource_list};
use api::schema::SchemaRegistry;
use serde_json::{json, Value};

/// A fixture decoded into its typed kind (selected by the document's `kind`).
/// Variants are boxed: the typed ports are large structs (clippy:
/// large_enum_variant) and the enum is only a dispatch aid.
#[derive(Clone, Debug, PartialEq)]
enum FixtureKind {
    Workflow(Box<Workflow>),
    CronWorkflow(Box<CronWorkflow>),
}

/// Decode one fixture document by `.kind`; a decode failure or an unexpected
/// kind panics with the fixture file name in the message.
fn decode_fixture(name: &str, doc: &Value) -> FixtureKind {
    let kind = doc
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("<missing>");
    match kind {
        "Workflow" => serde_json::from_value(doc.clone())
            .map(|wf| FixtureKind::Workflow(Box::new(wf)))
            .unwrap_or_else(|e| panic!("{name}: decode Workflow failed: {e}")),
        "CronWorkflow" => serde_json::from_value(doc.clone())
            .map(|cwf| FixtureKind::CronWorkflow(Box::new(cwf)))
            .unwrap_or_else(|e| panic!("{name}: decode CronWorkflow failed: {e}")),
        other => panic!("{name}: unexpected kind {other:?} (want Workflow|CronWorkflow)"),
    }
}

impl FixtureKind {
    /// Encode back to the JSON wire form (Q10).
    fn encode(&self) -> Value {
        match self {
            FixtureKind::Workflow(wf) => serde_json::to_value(wf).expect("encode Workflow"),
            FixtureKind::CronWorkflow(cwf) => {
                serde_json::to_value(cwf).expect("encode CronWorkflow")
            }
        }
    }
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/argoproj")
}

/// Load every `*.json` fixture in the directory (sorted for determinism).
/// New fixture files are picked up automatically.
fn load_fixtures() -> Vec<(String, Value)> {
    let mut paths: Vec<PathBuf> = fs::read_dir(fixture_dir())
        .expect("read fixtures/argoproj dir")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "no fixtures found in {}",
        fixture_dir().display()
    );
    paths
        .into_iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{name}: read fixture failed: {e}"));
            let doc: Value = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{name}: parse fixture JSON failed: {e}"));
            (name, doc)
        })
        .collect()
}

/// Find the unique fixture whose file name contains `needle` (spot tests).
fn find_fixture(needle: &str) -> (String, Value) {
    let matches: Vec<(String, Value)> = load_fixtures()
        .into_iter()
        .filter(|(name, _)| name.contains(needle))
        .collect();
    match matches.len() {
        1 => matches.into_iter().next().unwrap(),
        0 => panic!("no fixture containing {needle:?}"),
        n => panic!("{n} fixtures contain {needle:?}; use a more specific name"),
    }
}

/// Assert the wire form is stable and semantically lossless:
/// doc -> obj -> v2 -> obj' -> v3, with v2 == v3 and obj == obj'.
fn assert_round_trip_lossless(name: &str, doc: &Value) {
    let obj = decode_fixture(name, doc);
    let v2 = obj.encode();
    let redecoded = decode_fixture(&format!("{name} (re-decode of v2)"), &v2);
    let v3 = redecoded.encode();
    assert_eq!(
        v2, v3,
        "{name}: serialization must be idempotent (byte-stable wire form)"
    );
    assert_eq!(
        redecoded.encode(),
        v2,
        "{name}: to_value(&redecoded) must equal v2 (semantic losslessness)"
    );
    assert_eq!(
        obj, redecoded,
        "{name}: decoded object must survive the round-trip"
    );
}

#[test]
fn all_fixtures_round_trip_losslessly() {
    let fixtures = load_fixtures();
    // The curated set covers every required shape (T7.3 Scope A).
    assert!(
        fixtures.len() >= 8,
        "expected >= 8 fixtures, got {}",
        fixtures.len()
    );
    for (name, doc) in &fixtures {
        assert_round_trip_lossless(name, doc);
    }
}

#[test]
fn all_fixtures_are_argoproj_v1alpha1_workflow_docs() {
    for (name, doc) in load_fixtures() {
        assert_eq!(
            doc.get("apiVersion").and_then(Value::as_str),
            Some("argoproj.io/v1alpha1"),
            "{name}: unexpected apiVersion"
        );
        let kind = doc.get("kind").and_then(Value::as_str).unwrap_or_default();
        assert!(
            matches!(kind, "Workflow" | "CronWorkflow"),
            "{name}: unexpected kind {kind:?}"
        );
    }
}

#[test]
fn cron_workflow_fixture_keeps_its_schedules() {
    let (name, doc) = find_fixture("cron-workflow.json");
    let cwf = match decode_fixture(&name, &doc) {
        FixtureKind::CronWorkflow(cwf) => cwf,
        other => panic!("{name}: expected CronWorkflow, got {other:?}"),
    };
    assert_eq!(cwf.spec.schedules, vec!["* * * * *"], "{name}: schedules");
    assert_eq!(cwf.spec.timezone, "America/Los_Angeles", "{name}: timezone");
    // The schedules survive the wire round-trip untouched.
    let v2 = serde_json::to_value(&cwf).expect("encode");
    assert_eq!(
        v2["spec"]["schedules"],
        json!(["* * * * *"]),
        "{name}: v2 schedules"
    );
}

#[test]
fn cron_workflow_multiple_schedules_fixture_keeps_all_entries() {
    let (name, doc) = find_fixture("cron-workflow-multiple-schedules.json");
    let cwf = match decode_fixture(&name, &doc) {
        FixtureKind::CronWorkflow(cwf) => cwf,
        other => panic!("{name}: expected CronWorkflow, got {other:?}"),
    };
    assert_eq!(cwf.spec.schedules.len(), 2, "{name}: want 2 schedules");
    let v2 = serde_json::to_value(&cwf).expect("encode");
    assert_eq!(
        v2["spec"]["schedules"],
        json!(["*/3 * * * 1-5", "*/2 * * * *"]),
        "{name}: v2 schedules"
    );
}

#[test]
fn with_items_fixture_decodes_item_entries() {
    let (name, doc) = find_fixture("loops.json");
    let wf = match decode_fixture(&name, &doc) {
        FixtureKind::Workflow(wf) => wf,
        other => panic!("{name}: expected Workflow, got {other:?}"),
    };
    let step = wf
        .spec
        .templates
        .iter()
        .find(|t| t.name == "loop-example")
        .and_then(|t| t.steps.first())
        .and_then(|group| group.first())
        .unwrap_or_else(|| panic!("{name}: loop-example step not found"));
    assert_eq!(step.name, "print-message-loop");
    assert_eq!(
        step.with_items
            .iter()
            .map(|item| &item.0)
            .collect::<Vec<_>>(),
        vec![&json!("hello world"), &json!("goodbye world")],
        "{name}: withItems entries"
    );
}

#[test]
fn artifacts_fixture_keeps_artifact_names() {
    let (name, doc) = find_fixture("artifact-passing.json");
    let wf = match decode_fixture(&name, &doc) {
        FixtureKind::Workflow(wf) => wf,
        other => panic!("{name}: expected Workflow, got {other:?}"),
    };
    let mut names: Vec<&str> = wf
        .spec
        .templates
        .iter()
        .flat_map(|t| {
            t.inputs
                .artifacts
                .iter()
                .chain(t.outputs.artifacts.iter())
                .map(|a| a.name.as_str())
        })
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["hello-art", "message"],
        "{name}: artifacts[].name"
    );
    // And the names are still on the wire after re-encoding.
    let v2 = serde_json::to_value(&wf).expect("encode");
    let wire_names: Vec<&str> = v2["spec"]["templates"]
        .as_array()
        .expect("templates on the wire")
        .iter()
        .flat_map(|t| {
            ["inputs", "outputs"].into_iter().flat_map(move |io| {
                t[io]["artifacts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| a["name"].as_str())
            })
        })
        .collect();
    assert_eq!(
        wire_names,
        vec!["hello-art", "message"],
        "{name}: wire artifacts[].name"
    );
}

#[test]
fn legacy_singular_schedule_reencodes_as_schedules() {
    // v3 compat: upstream accepted a singular `schedule` string; the port
    // decodes it into `schedules` and re-encodes only the v4 plural key.
    let v3_doc = json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "CronWorkflow",
        "metadata": {"name": "legacy-cron"},
        "spec": {
            "schedule": "* * * * *",
            "workflowSpec": {
                "entrypoint": "main",
                "templates": [{"name": "main", "container": {"image": "busybox"}}]
            }
        }
    });
    let cwf: CronWorkflow = serde_json::from_value(v3_doc).expect("decode legacy schedule");
    assert_eq!(cwf.spec.schedules, vec!["* * * * *"]);
    let v2 = serde_json::to_value(&cwf).expect("encode");
    assert_eq!(
        v2["spec"]["schedules"],
        json!(["* * * * *"]),
        "v4 key carries the schedule"
    );
    assert!(
        v2["spec"].get("schedule").is_none(),
        "legacy alias must not leak: {v2}"
    );
    // The v4 form is the stable wire form from here on.
    assert_round_trip_lossless("legacy-schedule (inline)", &v2);
}

#[test]
fn discovery_lists_argoproj_group_with_short_names() {
    let mut reg = SchemaRegistry::new();
    argoproj::register(&mut reg);

    let groups = api_group_list(&reg);
    let group = groups
        .groups
        .iter()
        .find(|g| g.name == "argoproj.io")
        .expect("argoproj.io group in /apis");
    assert_eq!(
        group.preferred_version.as_ref().unwrap().version,
        "v1alpha1"
    );
    assert_eq!(
        group.preferred_version.as_ref().unwrap().group_version,
        "argoproj.io/v1alpha1"
    );

    let resources =
        api_resource_list(&reg, "argoproj.io", "v1alpha1").expect("argoproj.io/v1alpha1 served");
    let wf = resources
        .resources
        .iter()
        .find(|r| r.name == "workflows")
        .unwrap();
    assert_eq!(wf.kind, "Workflow");
    assert!(wf.namespaced);
    assert_eq!(wf.short_names.as_deref(), Some(&["wf".to_string()][..]));
    let cwf = resources
        .resources
        .iter()
        .find(|r| r.name == "cronworkflows")
        .unwrap();
    assert_eq!(cwf.kind, "CronWorkflow");
    assert!(cwf.namespaced);
    assert_eq!(
        cwf.short_names.as_deref(),
        Some(&["cwf".to_string(), "cronwf".to_string()][..])
    );
}
