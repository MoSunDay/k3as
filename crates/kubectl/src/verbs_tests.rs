//! Unit tests for the T7.3 Scope A argoproj.io verbs (Q30).
//!
//! Pure helpers (plural/path/output/age/table/manifest) plus the lib.rs
//! flag parsing, all with no server. The wire-level cases live in
//! verbs_wire_tests.rs.

use serde_json::json;

use crate::verbs::{
    age_between, manifest_target, parse_manifest, parse_output, render_table, resolve_plural,
    resource_path, Output,
};

// ---------- pure helpers ----------

#[test]
fn resolve_plural_whitelist_and_aliases() {
    for kind in ["Workflow", "workflows", "wf", "WF"] {
        assert_eq!(resolve_plural(kind).unwrap(), "workflows", "{kind}");
    }
    for kind in ["CronWorkflow", "cronworkflows", "cwf", "cronwf"] {
        assert_eq!(resolve_plural(kind).unwrap(), "cronworkflows", "{kind}");
    }
}

#[test]
fn resolve_plural_rejects_out_of_scope_kinds() {
    for kind in ["pod", "deployment", "workflowtemplates", ""] {
        let err = resolve_plural(kind).unwrap_err();
        assert!(err.contains("argoproj.io/v1alpha1"), "{kind}: {err}");
        assert!(err.contains("workflow"), "{kind}: {err}");
        assert!(err.contains("cronworkflow"), "{kind}: {err}");
    }
}

#[test]
fn resource_path_collection_and_item() {
    assert_eq!(
        resource_path("workflows", "default", None),
        "/apis/argoproj.io/v1alpha1/namespaces/default/workflows"
    );
    assert_eq!(
        resource_path("cronworkflows", "argo", Some("nightly")),
        "/apis/argoproj.io/v1alpha1/namespaces/argo/cronworkflows/nightly"
    );
}

#[test]
fn parse_output_forms() {
    assert_eq!(parse_output("").unwrap(), Output::Table);
    assert_eq!(parse_output("table").unwrap(), Output::Table);
    assert_eq!(parse_output("json").unwrap(), Output::Json);
    assert_eq!(parse_output("yaml").unwrap(), Output::Yaml);
    assert_eq!(parse_output("name").unwrap(), Output::Name);
    let err = parse_output("wide-json").unwrap_err();
    assert!(err.contains("json, yaml, name"), "{err}");
}

#[test]
fn rfc3339_and_age_rendering() {
    let t0 = 1_768_910_400; // 2026-01-20T12:00:00Z
    assert_eq!(age_between(Some("2026-01-20T12:00:00Z"), t0), "0s");
    assert_eq!(age_between(Some("2026-01-20T11:59:55Z"), t0), "5s");
    assert_eq!(age_between(Some("2026-01-20T11:50:00Z"), t0), "10m");
    assert_eq!(age_between(Some("2026-01-20T09:00:00Z"), t0), "3h");
    assert_eq!(age_between(Some("2026-01-18T12:00:00Z"), t0), "2d");
    // Fractional suffix ignored; space separator accepted.
    assert_eq!(age_between(Some("2026-01-20T11:59:50.123Z"), t0), "10s");
    assert_eq!(age_between(Some("2026-01-20 11:59:50Z"), t0), "10s");
    // Future timestamps clamp to 0 instead of going negative.
    assert_eq!(age_between(Some("2026-01-20T12:01:00Z"), t0), "0s");
    // Missing / malformed -> <unknown>.
    assert_eq!(age_between(None, t0), "<unknown>");
    assert_eq!(age_between(Some("not-a-timestamp"), t0), "<unknown>");
    assert_eq!(age_between(Some("2026-13-40T99:99:99Z"), t0), "<unknown>");
    // Leap day sanity against the civil-date algorithm.
    let leap = age_between(Some("2024-02-29T00:00:00Z"), 1_709_164_800);
    assert_eq!(leap, "0s"); // 2024-02-29T00:00:00Z == 1709164800
}

#[test]
fn render_table_aligns_columns() {
    let items = vec![
        json!({
            "metadata": {"name": "hello-world", "creationTimestamp": "2026-01-20T11:59:55Z"},
            "status": {"phase": "Running"},
        }),
        json!({
            "metadata": {"name": "short", "creationTimestamp": "2026-01-20T09:00:00Z"},
            "status": {}, // no phase -> Unknown
        }),
    ];
    let now = 1_768_910_400; // 2026-01-20T12:00:00Z
    let table = render_table(&items, now);
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines[0], "NAME          PHASE     AGE");
    assert_eq!(lines[1], "hello-world   Running   5s");
    assert_eq!(lines[2], "short         Unknown   3h");
    assert!(table.ends_with('\n'));
}

#[test]
fn render_table_marks_missing_fields() {
    let table = render_table(&[json!({"metadata": {"name": "bare"}, "status": null})], 0);
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines[1], "bare   Unknown   <unknown>");
}

// ---------- manifests ----------

const WF_YAML: &str = "apiVersion: argoproj.io/v1alpha1\nkind: Workflow\nmetadata:\n  name: hello-world\n  namespace: argo\nspec:\n  entrypoint: main\n";

#[test]
fn parse_manifest_accepts_yaml_and_json() {
    let obj = parse_manifest(WF_YAML, "wf.yaml").unwrap();
    assert_eq!(obj["kind"], "Workflow");
    let json_obj = parse_manifest(
        r#"{"kind":"CronWorkflow","metadata":{"name":"c"}}"#,
        "c.json",
    )
    .unwrap();
    assert_eq!(json_obj["kind"], "CronWorkflow");
}

#[test]
fn parse_manifest_rejects_multi_doc_and_empty() {
    let multi = format!("{WF_YAML}---\nkind: Workflow\nmetadata:\n  name: second\n");
    let err = parse_manifest(&multi, "multi.yaml").unwrap_err();
    assert!(err.contains("multi-document"), "{err}");
    let empty = parse_manifest("", "empty.yaml").unwrap_err();
    assert!(empty.contains("no YAML document"), "{empty}");
    // A comment-only file parses to Null — rejected as not-a-mapping.
    let null_doc = parse_manifest("# only a comment\n", "c.yaml").unwrap_err();
    assert!(null_doc.contains("must be a YAML mapping"), "{null_doc}");
    let broken = parse_manifest("metadata: [unclosed", "bad.yaml").unwrap_err();
    assert!(broken.contains("invalid YAML"), "{broken}");
}

#[test]
fn manifest_target_extracts_kind_name_namespace() {
    let obj = parse_manifest(WF_YAML, "wf.yaml").unwrap();
    let (kind, name, gen, ns) = manifest_target(&obj).unwrap();
    assert_eq!(
        (
            kind.as_str(),
            name.as_deref(),
            gen.as_deref(),
            ns.as_deref()
        ),
        ("Workflow", Some("hello-world"), None, Some("argo"))
    );
    // generateName-only manifests are allowed (name -> None, POST create).
    let gen_only =
        manifest_target(&json!({"kind": "Workflow", "metadata": {"generateName": "hello-"}}))
            .unwrap();
    assert_eq!(
        (gen_only.1.as_deref(), gen_only.2.as_deref()),
        (None, Some("hello-"))
    );
    // Neither name nor generateName errors; missing kind errors; empty
    // namespace treated as absent.
    let no_name = manifest_target(&json!({"kind": "Workflow", "metadata": {}})).unwrap_err();
    assert!(no_name.contains(".metadata.name"), "{no_name}");
    let no_kind = manifest_target(&json!({"metadata": {"name": "x"}})).unwrap_err();
    assert!(no_kind.contains(".kind"), "{no_kind}");
    let empty_ns =
        manifest_target(&json!({"kind": "Workflow", "metadata": {"name": "x", "namespace": ""}}))
            .unwrap();
    assert_eq!(empty_ns.3, None);
}

// ---------- flag parsing (lib.rs) ----------

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn get_flags_and_positionals() {
    let (flags, pos) = crate::parse_verb_flags(
        &args(&["get", "wf/hello", "-n", "argo", "-o", "name"]),
        true,
        false,
    )
    .unwrap();
    assert_eq!(flags.namespace, "argo");
    assert_eq!(flags.output, Output::Name);
    assert_eq!(flags.server, "http://127.0.0.1:6443");
    assert_eq!(pos, ["wf/hello"]);
    // --flag=value and long forms.
    let (flags, pos) = crate::parse_verb_flags(
        &args(&["get", "--namespace=argo", "--output=json", "cwf"]),
        true,
        false,
    )
    .unwrap();
    assert_eq!(flags.namespace, "argo");
    assert_eq!(flags.output, Output::Json);
    assert_eq!(pos, ["cwf"]);
}

#[test]
fn verb_flags_reject_the_out_of_verb_ones() {
    let err = crate::parse_verb_flags(&args(&["delete", "-f", "x"]), false, false).unwrap_err();
    assert!(err.contains("only supported by \"kubectl apply\""), "{err}");
    let err = crate::parse_verb_flags(&args(&["delete", "-o", "json"]), false, false).unwrap_err();
    assert!(err.contains("only supported by \"kubectl get\""), "{err}");
    let err = crate::parse_verb_flags(&args(&["get", "wf", "--wat"]), true, false).unwrap_err();
    assert!(err.contains("unknown flag"), "{err}");
    let err = crate::parse_verb_flags(&args(&["get", "wf", "--server"]), true, false).unwrap_err();
    assert!(err.contains("needs an argument"), "{err}");
}

#[test]
fn apply_requires_dash_f() {
    let (flags, pos) =
        crate::parse_verb_flags(&args(&["apply", "-f", "wf.yaml"]), false, true).unwrap();
    assert_eq!(flags.file.as_deref(), Some("wf.yaml"));
    assert!(pos.is_empty());
    let (flags, _) = crate::parse_verb_flags(
        &args(&["apply", "--filename=wf.yaml", "extra"]),
        false,
        true,
    )
    .unwrap();
    assert_eq!(flags.file.as_deref(), Some("wf.yaml"));
    // The -f requirement itself is enforced in run_apply; here we only
    // assert the flag never lands for get/delete (previous test).
}
