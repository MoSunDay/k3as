//! Pure-grammar + follow-diff unit tests for [`crate::logs`] (T4.2 Scope C).
//! Wire-level cases (canned TCP servers) live in `logs_wire_tests.rs`.
#![forbid(unsafe_code)]

use crate::logs::{
    advance, count_lines, log_path, next_batch, parse_logs_args, status_error_text, FollowState,
    SHRINK_WARNING,
};

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn parse_accepts_the_grammar() {
    let inv = parse_logs_args(&args(&["logs", "web"])).unwrap();
    assert_eq!(inv.name, "web");
    assert_eq!(inv.namespace, "default");
    assert!(!inv.follow);
    assert_eq!(inv.tail, None);
    assert_eq!(inv.server, crate::DEFAULT_SERVER);

    let inv = parse_logs_args(&args(&[
        "logs",
        "pods/web",
        "-n",
        "kube-system",
        "--server=http://h:1",
        "--container=side",
        "--tail=50",
        "--follow",
    ]))
    .unwrap();
    assert_eq!(inv.name, "web");
    assert_eq!(inv.namespace, "kube-system");
    assert_eq!(inv.container.as_deref(), Some("side"));
    assert_eq!(inv.tail, Some(50));
    assert!(inv.follow);

    let inv = parse_logs_args(&args(&["logs", "pod/w", "-f"])).unwrap();
    assert_eq!(inv.name, "w");
    assert!(inv.follow);
}

#[test]
fn parse_rejects_bad_invocations() {
    for bad in [
        vec!["logs"],
        vec!["logs", "a", "b"],
        vec!["logs", "deployment/w"],
        vec!["logs", "pods/"],
        vec!["logs", "w", "--wat"],
        vec!["logs", "w", "--tail"],
        vec!["logs", "w", "--tail=0"],
        vec!["logs", "w", "--tail=-2"],
        vec!["logs", "w", "--tail=abc"],
        vec!["logs", "w", "--container"],
        vec!["logs", "w", "-c"],
    ] {
        let argv = args(&bad);
        assert!(parse_logs_args(&argv).is_err(), "{bad:?} must fail");
    }
}

#[test]
fn frozen_log_path_shapes() {
    assert_eq!(
        log_path("default", "web", None, None),
        "/api/v1/namespaces/default/pods/web/log"
    );
    assert_eq!(
        log_path("ns", "w", Some("-"), None),
        "/api/v1/namespaces/ns/pods/w/log?container=-"
    );
    assert_eq!(
        log_path("ns", "w", Some("app"), Some(7)),
        "/api/v1/namespaces/ns/pods/w/log?container=app&tailLines=7"
    );
    assert_eq!(
        log_path("ns", "w", None, Some(7)),
        "/api/v1/namespaces/ns/pods/w/log?tailLines=7"
    );
}

#[test]
fn status_errors_render_upstream_shaped() {
    let body = r#"{"kind":"Status","status":"Failure","message":"pods \"x\" not found","reason":"NotFound","code":404}"#;
    assert_eq!(
        status_error_text(404, body),
        "Error from server (NotFound): pods \"x\" not found"
    );
    assert_eq!(
        status_error_text(404, "not json"),
        "Error from server (404): not json",
        "non-JSON bodies fall back to the raw text"
    );
    assert_eq!(status_error_text(500, "{}"), "Error from server (500): {}");
}

#[test]
fn batch_diff_grows_steadies_and_resets() {
    // Seed/growth prints just the NEW lines with one terminator each.
    assert_eq!(next_batch(0, "a\nb\n"), (2, Some("a\nb\n".to_string())));
    assert_eq!(next_batch(2, "a\nb\nc\nd"), (4, Some("c\nd\n".to_string())));
    // Empty snapshot / counters.
    assert_eq!(count_lines(""), 0);
    assert_eq!(count_lines("\n"), 1);
    assert_eq!(count_lines("a\nb\n"), 2);

    let state = FollowState {
        seen: 0,
        warned_shrink: false,
    };
    let (state, out, err) = advance(&state, "line1\n");
    assert_eq!(out, "line1\n");
    assert_eq!(err, "");
    assert_eq!(state.seen, 1);

    // Steady poll emits nothing.
    let (state, out, err) = advance(&state, "line1\n");
    assert_eq!(out, "");
    assert_eq!(err, "");

    // Growth resumes after the watermark.
    let (state, out, _) = advance(&state, "line1\nline2\nline3");
    assert_eq!(out, "line2\nline3\n");

    // Shrink warns exactly ONCE, prints nothing, resets the watermark.
    let (state, out, err) = advance(&state, "rotated\n");
    assert_eq!(out, "");
    assert_eq!(err, format!("{SHRINK_WARNING}\n"));
    assert_eq!(state.seen, 1);
    let (_, out, err) = advance(&state, "rotated\nmore\n");
    assert_eq!(out, "more\n");
    assert_eq!(err, "", "warn once only");
}
