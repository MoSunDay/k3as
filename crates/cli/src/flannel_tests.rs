//! Pure cases for [`crate::flannel`] (TODO **T4.3 Scope A**, decision
//! **Q34**) - split out of the module for the file-size cap.

use super::*;

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("init-pro-flannel-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn kubeconfig_body_and_validation() {
    // T1.3a (Q47 P3): token lands in `user.token`, server in the
    // cluster; hostile tokens are rejected before any file write.
    let body = kubeconfig_body("http://127.0.0.1:6443", "abc123");
    assert!(body.contains("server: http://127.0.0.1:6443"));
    assert!(body.contains("token: abc123"));
    assert!(body.contains("current-context: init-pro"));

    let d = tmp_dir("kc");
    let p = ensure_kubeconfig(&d, "http://127.0.0.1:6443", "abc123").unwrap();
    assert_eq!(p, kubeconfig_path(&d));
    // Idempotent rewrite with the same content.
    assert!(ensure_kubeconfig(&d, "http://127.0.0.1:6443", "abc123").is_ok());
    assert!(ensure_kubeconfig(&d, "http://127.0.0.1:6443", "bad token").is_err());
    assert!(ensure_kubeconfig(&d, "http://127.0.0.1:6443", "").is_err());
    assert!(ensure_kubeconfig(&d, "http://127.0.0.1:6443", "quote\"tok").is_err());
}

#[test]
fn backend_normalization_and_disabled() {
    assert!(is_enabled(&FlannelNet::default()));
    assert!(!is_enabled(&FlannelNet {
        backend: Some("none".into()),
        ..Default::default()
    }));
    assert_eq!(normalize_backend(&FlannelNet::default()), ("vxlan", None));
    assert_eq!(
        normalize_backend(&FlannelNet {
            backend: Some("host-gw".into()),
            ..Default::default()
        }),
        ("host-gw", None)
    );
    let (_, warn) = normalize_backend(&FlannelNet {
        backend: Some("wireguard".into()),
        ..Default::default()
    });
    assert_eq!(warn.as_deref(), Some("wireguard"));
}

#[test]
fn net_conf_body_default_and_custom() {
    let body = net_conf_body(&FlannelNet::default());
    assert_eq!(
        body,
        r#"{"Backend":{"Type":"vxlan"},"Network":"10.42.0.0/16"}"#
    );
    let body = net_conf_body(&FlannelNet {
        cluster_cidr: Some("10.244.0.0/16".into()),
        backend: Some("host-gw".into()),
        ..Default::default()
    });
    assert_eq!(
        body,
        r#"{"Backend":{"Type":"host-gw"},"Network":"10.244.0.0/16"}"#
    );
}

#[test]
fn paths_follow_agent_layout() {
    let dd = Path::new("/tmp/dd");
    assert_eq!(
        net_conf_path(dd),
        Path::new("/tmp/dd/agent/etc/flannel/net-conf.json")
    );
    assert_eq!(
        subnet_env_path(dd),
        Path::new("/tmp/dd/agent/flannel/subnet.env")
    );
}

#[test]
fn ensure_net_conf_writes_idempotently_and_honors_user_file() {
    let dd = tmp_dir("netconf");
    let net = FlannelNet {
        cluster_cidr: Some("10.43.0.0/16".into()),
        ..Default::default()
    };
    let p = ensure_net_conf(&dd, &net).unwrap();
    assert_eq!(p, net_conf_path(&dd));
    let first = std::fs::read_to_string(&p).unwrap();
    assert_eq!(first, net_conf_body(&net));
    // Second call: no rewrite churn (content identical).
    let mtime1 = std::fs::metadata(&p).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    ensure_net_conf(&dd, &net).unwrap();
    let mtime2 = std::fs::metadata(&p).unwrap().modified().unwrap();
    assert_eq!(mtime1, mtime2, "identical body must not rewrite");

    // Changed flags DO rewrite.
    let net2 = FlannelNet {
        backend: Some("host-gw".into()),
        ..net
    };
    ensure_net_conf(&dd, &net2).unwrap();
    assert_eq!(std::fs::read_to_string(&p).unwrap(), net_conf_body(&net2));

    // --flannel-conf passes a user file through verbatim (no write).
    let user = dd.join("user-net-conf.json");
    std::fs::write(&user, r#"{"Network":"192.168.0.0/16"}"#).unwrap();
    let net3 = FlannelNet {
        conf: Some(user.clone()),
        ..Default::default()
    };
    assert_eq!(ensure_net_conf(&dd, &net3).unwrap(), user);

    let _ = std::fs::remove_dir_all(&dd);
}

#[test]
fn c8_restart_delay_doubles_then_caps() {
    // C8: 250 ms doubling on the restart streak, 5 s cap, never zero.
    assert_eq!(restart_delay(0), std::time::Duration::from_millis(250));
    assert_eq!(restart_delay(1), std::time::Duration::from_millis(500));
    assert_eq!(restart_delay(2), std::time::Duration::from_secs(1));
    assert_eq!(restart_delay(4), std::time::Duration::from_secs(4));
    assert_eq!(restart_delay(5), std::time::Duration::from_secs(5));
    assert_eq!(restart_delay(40), std::time::Duration::from_secs(5));
}

#[test]
fn c8_restart_streak_resets_after_a_stable_run() {
    // Crash-loop runs grow the streak; a 30 s+ run resets it to 0 so the
    // next retry is back at the base delay instead of the 5 s cap.
    assert_eq!(next_restart_streak(0, std::time::Duration::from_secs(1)), 1);
    assert_eq!(next_restart_streak(6, std::time::Duration::from_secs(2)), 7);
    assert_eq!(next_restart_streak(6, STABLE_RUN), 0);
    assert_eq!(
        next_restart_streak(6, std::time::Duration::from_secs(600)),
        0
    );
    assert_eq!(
        next_restart_streak(u32::MAX, std::time::Duration::from_secs(1)),
        u32::MAX,
        "saturating, never wraps into a short delay"
    );
}
