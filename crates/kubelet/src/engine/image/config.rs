//! T4.13 image store — OCI image config wire type + argv/env merge (Q53/Q54).
//!
//! Pure functions over the image's `config` blob: parse the (often
//! partial) JSON, then merge it with the CRI container request —
//! `merge_argv` resolves command/args vs entrypoint/cmd precedence and
//! `merge_env` applies image env + pod env with last-wins duplicates.

use std::collections::HashMap;

use serde::Deserialize;

/// Top-level OCI image config: `architecture`, `os` and the `config` body
/// that carries process defaults. Every field defaults so partial
/// configs (hand-built test images, minimal OCI artifacts) still parse.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageConfig {
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub config: ConfigBody,
}

/// Process section of the image config (`config` key): user, env,
/// entrypoint, cmd, workingDir. Docker/OCI write PascalCase keys
/// (`User`, `Env`, `Entrypoint`, `Cmd`, `WorkingDir`); the lowerCamelCase
/// spellings are accepted as aliases (Q54 tolerance posture).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConfigBody {
    #[serde(default, rename = "User", alias = "user")]
    pub user: Option<String>,
    #[serde(default, rename = "Env", alias = "env")]
    pub env: Vec<String>,
    #[serde(default, rename = "Entrypoint", alias = "entrypoint")]
    pub entrypoint: Vec<String>,
    #[serde(default, rename = "Cmd", alias = "cmd")]
    pub cmd: Vec<String>,
    #[serde(default, rename = "WorkingDir", alias = "workingDir")]
    pub working_dir: Option<String>,
}

/// Deserialize an image config blob (the `config` descriptor's content).
pub fn parse_image_config(bytes: &[u8]) -> Result<ImageConfig, String> {
    serde_json::from_slice(bytes).map_err(|e| format!("image config JSON: {e}"))
}

/// Resolve the container argv (Q53: feeds the T4.13 `spec::merge_launch`).
///
/// An explicit CRI `command` overrides the image: `command ++ args`
/// (args may be empty) wins outright. Without `command`, the image's
/// `entrypoint ++ cmd` is used. May return an empty vec — the caller
/// (`engine::spec::merge_launch`) then reports "no command" for the
/// container.
pub fn merge_argv(cfg: &ConfigBody, command: &[String], args: &[String]) -> Vec<String> {
    if !command.is_empty() {
        command.iter().chain(args.iter()).cloned().collect()
    } else {
        cfg.entrypoint
            .iter()
            .chain(cfg.cmd.iter())
            .cloned()
            .collect()
    }
}

/// Resolve the container env: pod env APPENDS after image env, but a pod
/// value wins on duplicate keys. Docker-style last-wins dedupe: keep the
/// LATER occurrence's value while preserving first-seen key order.
pub fn merge_env(cfg_env: &[String], pod_env: &[String]) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut values: HashMap<String, String> = HashMap::new();
    for entry in cfg_env.iter().chain(pod_env.iter()) {
        let (key, value) = split_env(entry);
        if !values.contains_key(&key) {
            order.push(key.clone());
        }
        values.insert(key, value);
    }
    order
        .into_iter()
        .map(|key| format!("{key}={}", values[&key]))
        .collect()
}

/// Split `KEY=VALUE` at the first `=`; a bare key maps to an empty value.
fn split_env(entry: &str) -> (String, String) {
    match entry.split_once('=') {
        Some((key, value)) => (key.to_string(), value.to_string()),
        None => (entry.to_string(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn busybox_config() -> &'static str {
        r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "User": "",
                "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
                "Entrypoint": ["/bin/sh"],
                "Cmd": [],
                "WorkingDir": ""
            },
            "rootfs": {"type": "layers", "diff_ids": ["sha256:aa", "sha256:bb"]},
            "history": []
        }"#
    }

    #[test]
    fn parses_busybox_style_config() {
        let cfg = parse_image_config(busybox_config().as_bytes()).expect("config parses");
        assert_eq!(cfg.architecture, "amd64");
        assert_eq!(cfg.os, "linux");
        let body = &cfg.config;
        assert_eq!(body.user.as_deref(), Some(""));
        assert_eq!(body.entrypoint, s(&["/bin/sh"]));
        assert!(body.cmd.is_empty());
        assert_eq!(body.working_dir.as_deref(), Some(""));
        assert_eq!(body.env.len(), 1);
        assert!(body.env[0].starts_with("PATH="));
    }

    #[test]
    fn parses_partial_config_with_defaults() {
        let cfg = parse_image_config(br#"{"architecture":"arm64"}"#).expect("partial parses");
        assert_eq!(cfg.architecture, "arm64");
        assert_eq!(cfg.os, "");
        assert!(cfg.config.env.is_empty());
        assert!(cfg.config.entrypoint.is_empty());
        assert_eq!(cfg.config.user, None);
        // Even a bare object parses.
        assert!(parse_image_config(b"{}").is_ok());
    }

    #[test]
    fn rejects_non_json_config() {
        assert!(parse_image_config(b"not json").is_err());
        // A JSON array is not an object: fields default, but serde still
        // rejects it — config blobs are objects by construction.
        assert!(parse_image_config(b"[1,2,3]").is_err());
    }

    fn body(entrypoint: &[&str], cmd: &[&str]) -> ConfigBody {
        ConfigBody {
            user: None,
            env: Vec::new(),
            entrypoint: s(entrypoint),
            cmd: s(cmd),
            working_dir: None,
        }
    }

    #[test]
    fn explicit_command_and_args_override_image() {
        let cfg = body(&["/bin/sh"], &["-c", "sleep 1"]);
        let argv = merge_argv(&cfg, &s(&["/usr/bin/env"]), &s(&["FOO=bar"]));
        assert_eq!(argv, s(&["/usr/bin/env", "FOO=bar"]));
    }

    #[test]
    fn explicit_command_alone_overrides_image() {
        let cfg = body(&["/bin/sh"], &["-c", "echo hi"]);
        let argv = merge_argv(&cfg, &s(&["/bin/date"]), &[]);
        assert_eq!(argv, s(&["/bin/date"]));
    }

    #[test]
    fn image_entrypoint_and_cmd_used_without_command() {
        let cfg = body(&["/docker-entrypoint.sh"], &["nginx", "-g", "daemon off;"]);
        let argv = merge_argv(&cfg, &[], &s(&["--extra"]));
        assert_eq!(
            argv,
            s(&["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"])
        );
        // args without an explicit command do NOT override (T4.12 parity:
        // the CRI layer concatenates command+args before this call).
        let bare = merge_argv(&cfg, &[], &[]);
        assert_eq!(bare.len(), 4);
    }

    #[test]
    fn empty_everything_yields_empty_argv() {
        let cfg = body(&[], &[]);
        assert!(merge_argv(&cfg, &[], &[]).is_empty());
    }

    #[test]
    fn env_merge_appends_and_pod_wins_last() {
        let cfg_env = s(&["A=1", "B=2", "A=2"]);
        let pod_env = s(&["B=3", "C=4"]);
        assert_eq!(merge_env(&cfg_env, &pod_env), s(&["A=2", "B=3", "C=4"]));
    }

    #[test]
    fn env_merge_preserves_first_seen_order() {
        let cfg_env = s(&["Z=26", "A=1"]);
        let pod_env = s(&["M=13", "Z=99"]);
        // Z keeps its first-seen slot but takes the later (pod) value.
        assert_eq!(merge_env(&cfg_env, &pod_env), s(&["Z=99", "A=1", "M=13"]));
        // Empty inputs stay empty; a bare key is treated as KEY="".
        assert!(merge_env(&[], &[]).is_empty());
        assert_eq!(merge_env(&s(&["BARE"]), &[]), s(&["BARE="]));
        assert_eq!(merge_env(&s(&["X=1", "X=2"]), &[]), s(&["X=2"]));
    }
}
