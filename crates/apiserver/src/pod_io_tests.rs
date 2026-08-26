//! Unit tests for the PURE parts of the T4.2 Scope C relay
//! (`crates/pod_io.rs` resolution/validation/targets +
//! `crates/kubelet_hop.rs` request/response byte handling).
//! Router-level wire cases live in [`crate::pod_io_wire_tests`].

use serde_json::json;

use crate::kubelet_hop::{build_hop_request, parse_hop_response};
use crate::pod_io::{
    exec_target, internal_ip, kubelet_port, log_target, parse_tail_lines, validate_exec_command,
};

fn node_with_port(port_json: &str) -> serde_json::Value {
    json!({"metadata": {"annotations": {"init-pro.io/kubelet-port": port_json}}})
}

#[test]
fn internal_ip_picks_the_internalip_address_entry() {
    let node = json!({
        "status": {"addresses": [
            {"type": "Hostname", "address": "h"},
            {"type": "InternalIP", "address": "10.0.0.7"},
        ]},
    });
    assert_eq!(internal_ip(&node), Some("10.0.0.7".into()));
    // Missing / malformed shapes degrade to None instead of panicking.
    assert_eq!(internal_ip(&json!({"status": {"addresses": []}})), None);
    assert_eq!(internal_ip(&node_with_port("1")), None);
}

#[test]
fn kubelet_port_falls_back_on_missing_invalid_or_zero() {
    let valid = json!({
        "metadata": {"annotations": {"init-pro.io/kubelet-port": "12345"}},
        "status": {},
    });
    assert_eq!(kubelet_port(&valid), 12345);
    assert_eq!(
        kubelet_port(&node_with_port("not-a-port")),
        10250,
        "invalid annotation -> fallback"
    );
    assert_eq!(
        kubelet_port(&node_with_port("0")),
        10250,
        "zero -> fallback"
    );
    assert_eq!(
        kubelet_port(&node_with_port("70000")),
        10250,
        "out of u16 range -> fallback"
    );
    assert_eq!(
        kubelet_port(&json!({"metadata": {}})),
        10250,
        "absent annotation -> fallback"
    );
}

#[test]
fn tail_lines_validation() {
    assert_eq!(parse_tail_lines("").unwrap(), None, "absent param");
    assert_eq!(parse_tail_lines("5").unwrap(), Some(5));
    assert_eq!(parse_tail_lines("0").unwrap(), Some(0));
    assert!(parse_tail_lines("-1").is_err());
    assert!(parse_tail_lines("abc").is_err());
    assert!(parse_tail_lines("1.5").is_err());
    assert!(parse_tail_lines("").map_err(|_| ()).is_ok());
}

#[test]
fn exec_body_validation_matrix() {
    assert!(validate_exec_command(br#"{"command":["ls","-l"]}"#).is_ok());
    assert!(validate_exec_command(br#"{"command":["sh"],"extra":1}"#).is_ok());
    let bad_bodies: [&[u8]; 6] = [
        br#"{"command":[]}"#,
        br#"{"command":"ls"}"#,
        br#"{"command":[42]}"#,
        br#"{}"#,
        br#"[1,2]"#,
        b"not json",
    ];
    for bad in bad_bodies {
        assert!(validate_exec_command(bad).is_err(), "{:?} must fail", bad);
    }
}

#[test]
fn frozen_target_paths() {
    assert_eq!(log_target("U", "-", None), "/pods/U/containers/-/log");
    assert_eq!(
        log_target("U", "app", Some(9)),
        "/pods/U/containers/app/log?tailLines=9"
    );
    assert_eq!(exec_target("U", "-"), "/pods/U/containers/-/exec");
}

#[test]
fn hop_request_bytes_pin_the_contract() {
    let get = String::from_utf8(build_hop_request(
        "GET",
        "/pods/u1/containers/app/log?tailLines=3",
        "10.0.0.7:10250",
        None,
        None,
    ))
    .unwrap();
    assert_eq!(
        get,
        "GET /pods/u1/containers/app/log?tailLines=3 HTTP/1.1\r\n\
         Host: 10.0.0.7:10250\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    );

    let post = String::from_utf8(build_hop_request(
        "POST",
        "/pods/u1/containers/-/exec",
        "10.0.0.7:10250",
        Some("application/json"),
        Some(br#"{"command":["ls"]}"#),
    ))
    .unwrap();
    assert!(post.starts_with("POST /pods/u1/containers/-/exec HTTP/1.1\r\n"));
    assert!(
        post.contains("Content-Type: application/json\r\nContent-Length: 18\r\n"),
        "{post}"
    );
    assert!(post.ends_with("\r\n\r\n{\"command\":[\"ls\"]}"), "{post}");
}

#[test]
fn hop_response_framing_content_length_and_chunked() {
    let ok = parse_hop_response(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 3\r\n\r\nhi\n",
    )
    .unwrap();
    assert_eq!(ok.code, 200);
    assert_eq!(
        ok.content_type.as_deref(),
        Some("text/plain"),
        "header lowercased"
    );
    assert_eq!(ok.body, b"hi\n");

    // `{"a":1}` split as 5-byte + 2-byte chunks, then the 0 terminator.
    let chunked = parse_hop_response(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n",
    )
    .unwrap();
    assert_eq!(chunked.body, br#"{"a":1}"#.to_vec());

    // Truncation / garbage are hard errors -> surfaced as 502 relay failures.
    assert!(parse_hop_response(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\nshort").is_err());
    assert!(parse_hop_response(b"nope").is_err());
}
