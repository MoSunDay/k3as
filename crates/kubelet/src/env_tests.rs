//! Pure-helper cases split out of env.rs (file size limit; the
//! server/server_tests precedent). Covers the T4.7 parse + resolve
//! matrix: camelCase wire keys, literal env, envFrom configMap/secret,
/// ordering + duplicate-name precedence, `optional` semantics, and the
//! data-value convention shared with the volume projection.
#![cfg(test)]

use serde_json::{json, Value};

use crate::env::{
    config_error_event, parse_env, parse_env_from, resolve_env, EnvFromSource, EnvSources,
    EnvValue, RefKind,
};

fn cm(name: &str, data: Value) -> (String, Option<Value>) {
    (name.to_string(), Some(json!({"data": data})))
}

fn sources_with(configmaps: Vec<(String, Option<Value>)>) -> EnvSources {
    EnvSources {
        configmaps: configmaps.into_iter().collect(),
        ..Default::default()
    }
}

#[test]
fn parses_camel_case_env_and_env_from() {
    let c = json!({
        "env": [
            {"name": "A", "value": "1"},
            {"name": "B", "valueFrom": {"configMapKeyRef": {"name": "cm", "key": "k", "optional": true}}},
            {"name": "C", "valueFrom": {"secretKeyRef": {"name": "s", "key": "sk"}}},
            {"name": ""},
            {"name": "D"},
            {"value": "x"},
            {"name": "E", "valueFrom": {"fieldRef": {"fieldPath": "metadata.name"}},
        ],
        "envFrom": [
            {"configMapRef": {"name": "cm1"}},
            {"secretRef": {"name": "s1", "optional": true}},
            {"configMapRef": {"name": "", "optional": true}},
            {"secretRef": {}},
            {},
        ],
    });
    let env = parse_env(&c);
    assert_eq!(
        env,
        vec![
            crate::env::EnvVar {
                name: "A".into(),
                value: EnvValue::Literal("1".into()),
            },
            crate::env::EnvVar {
                name: "B".into(),
                value: EnvValue::ConfigMapKeyRef {
                    name: "cm".into(),
                    key: "k".into(),
                    optional: true,
                },
            },
            crate::env::EnvVar {
                name: "C".into(),
                value: EnvValue::SecretKeyRef {
                    name: "s".into(),
                    key: "sk".into(),
                    optional: false,
                },
            },
        ],
        "malformed rows (no name/value, empty name, unsupported valueFrom) are skipped"
    );
    let env_from = parse_env_from(&c);
    assert_eq!(
        env_from,
        vec![
            EnvFromSource::ConfigMap {
                name: "cm1".into(),
                optional: false,
            },
            EnvFromSource::Secret {
                name: "s1".into(),
                optional: true,
            },
        ]
    );
    // Absent env/envFrom parse to empty, never fatal.
    assert!(parse_env(&json!({})).is_empty());
    assert!(parse_env_from(&json!({})).is_empty());
}

#[test]
fn literal_env_resolves() {
    let env = parse_env(&json!({"env": [
        {"name": "GREETING", "value": "hello"},
        {"name": "COUNT", "value": "42"},
    ]}));
    let got = resolve_env(&env, &[], &EnvSources::default()).unwrap();
    assert_eq!(
        got,
        vec![
            ("GREETING".to_string(), "hello".to_string()),
            ("COUNT".to_string(), "42".to_string()),
        ]
    );
}

#[test]
fn env_from_configmap_and_secret_project_sorted_data() {
    let mut s = sources_with(vec![cm("cm1", json!({"ZED": "z", "ALPHA": "a"}))]);
    s.secrets.insert(
        "s1".to_string(),
        Some(json!({"data": {"TOKEN": "t", "MID": "m"}})),
    );
    let env_from = parse_env_from(&json!({"envFrom": [
        {"configMapRef": {"name": "cm1"}},
        {"secretRef": {"name": "s1"}},
    ]}));
    let got = resolve_env(&[], &env_from, &s).unwrap();
    assert_eq!(
        got,
        vec![
            ("ALPHA".to_string(), "a".to_string()), // sorted within a source
            ("ZED".to_string(), "z".to_string()),
            ("MID".to_string(), "m".to_string()),
            ("TOKEN".to_string(), "t".to_string()),
        ]
    );
}

#[test]
fn env_from_precedes_env_and_explicit_env_wins_duplicates() {
    // k8s makeEnvironmentVariables: envFrom entries first, then explicit
    // env; a duplicate name takes the LAST value (explicit env overrides
    // envFrom) at the first position.
    let s = sources_with(vec![cm("cm1", json!({"K": "from-cm", "ONLY": "cm"}))]);
    let c = json!({
        "env": [{"name": "K", "value": "explicit"}, {"name": "E", "value": "e"}],
        "envFrom": [{"configMapRef": {"name": "cm1"}}],
    });
    let got = resolve_env(&parse_env(&c), &parse_env_from(&c), &s).unwrap();
    assert_eq!(
        got,
        vec![
            ("K".to_string(), "explicit".to_string()),
            ("ONLY".to_string(), "cm".to_string()),
            ("E".to_string(), "e".to_string()),
        ]
    );
}

#[test]
fn optional_missing_refs_contribute_nothing() {
    // configMap absent from sources + optional=true -> Ok, fewer vars.
    let c = json!({
        "env": [{"name": "A", "value": "1"},
                {"name": "B", "valueFrom": {"configMapKeyRef": {"name": "cm", "key": "k", "optional": true}}}],
        "envFrom": [
            {"configMapRef": {"name": "gone", "optional": true}},
            {"secretRef": {"name": "gone-secret", "optional": true}},
        ],
    });
    let got = resolve_env(&parse_env(&c), &parse_env_from(&c), &EnvSources::default()).unwrap();
    assert_eq!(got, vec![("A".to_string(), "1".to_string())]);
}

#[test]
fn required_missing_refs_error() {
    let c = json!({"envFrom": [{"configMapRef": {"name": "cm"}}]});
    let err = resolve_env(&[], &parse_env_from(&c), &EnvSources::default()).unwrap_err();
    assert_eq!(err, "configmap \"cm\" not found");

    let c = json!({"envFrom": [{"secretRef": {"name": "s"}}]});
    let err = resolve_env(&[], &parse_env_from(&c), &EnvSources::default()).unwrap_err();
    assert_eq!(err, "secret \"s\" not found");

    // 404-shaped absence (explicit None entry) behaves the same.
    let mut s = EnvSources::default();
    s.configmaps.insert("cm".into(), None);
    let err = resolve_env(&[], &parse_env_from(&c), &s);
    assert!(err.is_ok(), "secret absence must not trip on configmaps");
    let c = json!({"envFrom": [{"configMapRef": {"name": "cm"}}]});
    let err = resolve_env(&[], &parse_env_from(&c), &s).unwrap_err();
    assert_eq!(err, "configmap \"cm\" not found");
}

#[test]
fn key_ref_missing_object_and_missing_key_semantics() {
    let s = sources_with(vec![cm("cm", json!({"k": "v"}))]);
    // required object missing -> Err; optional object missing -> skipped.
    let e = parse_env(&json!({"env": [
        {"name": "X", "valueFrom": {"secretKeyRef": {"name": "s", "key": "k"}}}
    ]}));
    assert_eq!(
        resolve_env(&e, &[], &EnvSources::default()).unwrap_err(),
        "secret \"s\" not found"
    );
    let e = parse_env(&json!({"env": [
        {"name": "X", "valueFrom": {"configMapKeyRef": {"name": "cm", "key": "k"}}}
    ]}));
    assert_eq!(
        resolve_env(&e, &[], &s).unwrap(),
        vec![("X".to_string(), "v".to_string())]
    );
    // Key present but requested key absent: optional skips, required errs.
    let e = parse_env(&json!({"env": [
        {"name": "X", "valueFrom": {"configMapKeyRef": {"name": "cm", "key": "nope", "optional": true}}}
    ]}));
    assert_eq!(resolve_env(&e, &[], &s).unwrap(), vec![]);
    let e = parse_env(&json!({"env": [
        {"name": "X", "valueFrom": {"configMapKeyRef": {"name": "cm", "key": "nope"}}}
    ]}));
    assert_eq!(
        resolve_env(&e, &[], &s).unwrap_err(),
        "key \"nope\" not found in configmap \"cm\""
    );
}

#[test]
fn non_string_data_rows_are_skipped_like_volumes() {
    // The volume projection's data_map convention: only string values
    // count (JSON numbers from a plain-JSON store are dropped, not
    // stringified; secrets are plain strings, never base64 here).
    let s = sources_with(vec![cm(
        "cm1",
        json!({"GOOD": "ok", "NUM": 7, "BOOL": true, "NULL": null}),
    )]);
    let c = json!({"envFrom": [{"configMapRef": {"name": "cm1"}}]});
    assert_eq!(
        resolve_env(&[], &parse_env_from(&c), &s).unwrap(),
        vec![("GOOD".to_string(), "ok".to_string())]
    );
    // Invalid env names from data keys contribute nothing.
    let s = sources_with(vec![cm("cm1", json!({"A=B": "x", "": "y", "OK": "z"}))]);
    assert_eq!(
        resolve_env(&[], &parse_env_from(&c), &s).unwrap(),
        vec![("OK".to_string(), "z".to_string())]
    );
}

#[test]
fn fetch_errors_fail_even_optional_refs() {
    let mut s = EnvSources::default();
    s.fetch_errors.insert(
        "configmap/cm".into(),
        "GET /api/v1/namespaces/default/configmaps/cm -> HTTP 500".into(),
    );
    assert_eq!(
        s.fetch_error(RefKind::ConfigMap, "cm").as_deref(),
        Some("GET /api/v1/namespaces/default/configmaps/cm -> HTTP 500")
    );
    let c = json!({"envFrom": [{"configMapRef": {"name": "cm", "optional": true}}]});
    assert_eq!(
        resolve_env(&[], &parse_env_from(&c), &s).unwrap_err(),
        "GET /api/v1/namespaces/default/configmaps/cm -> HTTP 500"
    );
}

#[test]
fn config_error_event_shape_is_upstream_parity() {
    let ev = config_error_event("default", "web", "u1", "app", "configmap \"cm\" not found", "n1", "2026-11-15T00:00:00Z");
    assert_eq!(ev["kind"], "Event");
    assert_eq!(ev["metadata"]["namespace"], "default");
    assert_eq!(ev["metadata"]["generateName"], "web.app.");
    assert_eq!(ev["involvedObject"]["kind"], "Pod");
    assert_eq!(ev["involvedObject"]["name"], "web");
    assert_eq!(ev["involvedObject"]["uid"], "u1");
    assert_eq!(ev["reason"], "CreateContainerConfigError");
    assert_eq!(ev["type"], "Warning");
    assert_eq!(ev["message"], "configmap \"cm\" not found");
    assert_eq!(ev["source"]["component"], "kubelet");
    assert_eq!(ev["source"]["host"], "n1");
    assert_eq!(ev["count"], 1);
}
