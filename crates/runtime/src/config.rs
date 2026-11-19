//! Containerd config templating (TODO **T4.1**, decision **Q25**).
//!
//! Pure rendering of the k3s-style minimal `config.toml` proven by the Q24
//! spike (`scripts/t41-containerd-spike.sh`): CRI on the agent socket,
//! overlayfs snapshotter, `io.containerd.runc.v2` runtime. Paths are baked
//! into the file (k3s `pkg/agent/containerd/config_linux.go` parity) so the
//! supervisor only passes `-c <config>`.

use std::path::{Path, PathBuf};

/// Default pause image for CRI sandboxes (configurable via
/// `INIT_PRO_SANDBOX_IMAGE`; full airgap pre-pull arrives with T4.2).
pub const DEFAULT_SANDBOX_IMAGE: &str = "registry.k8s.io/pause:3.10";

/// Variables baked into the rendered `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerdConfigVars {
    /// containerd root (content store).
    pub root: PathBuf,
    /// containerd state (runtime state, sockets).
    pub state: PathBuf,
    /// The single CRI + ctr socket.
    pub socket: PathBuf,
    /// CNI network configuration directory.
    pub cni_conf_dir: PathBuf,
    /// CNI plugin binaries directory.
    pub cni_bin_dir: PathBuf,
    /// Sandbox (pause) image the CRI plugin pulls for pod sandboxes.
    pub sandbox_image: String,
    /// Snapshotter for the CRI plugin.
    pub snapshotter: String,
    /// Default runtime name.
    pub default_runtime_name: String,
    /// Runtime type of the default runtime.
    pub runtime_type: String,
    /// `[plugins."io.containerd.grpc.v1.cri".registry] config_path` — the
    /// `certs.d` root (T8.1, Q40). `None` keeps the block out entirely.
    pub registry_config_path: Option<PathBuf>,
}

impl ContainerdConfigVars {
    /// k3s-style layout under a data dir (Q24-proven):
    /// `<dd>/agent/containerd/{root,state}`, `<dd>/run/containerd/containerd.sock`,
    /// CNI conf under `<dd>/agent/etc/containerd/cni/net.d`.
    pub fn for_data_dir(data_dir: &Path, sandbox_image: &str) -> Self {
        let runtime_dir = data_dir.join("agent").join("containerd");
        Self {
            root: runtime_dir.join("root"),
            state: runtime_dir.join("state"),
            socket: data_dir
                .join("run")
                .join("containerd")
                .join("containerd.sock"),
            cni_conf_dir: data_dir
                .join("agent")
                .join("etc")
                .join("containerd")
                .join("cni")
                .join("net.d"),
            cni_bin_dir: runtime_dir.join("aux"),
            sandbox_image: sandbox_image.to_string(),
            snapshotter: "overlayfs".to_string(),
            default_runtime_name: "runc".to_string(),
            runtime_type: "io.containerd.runc.v2".to_string(),
            registry_config_path: Some(runtime_dir.join("certs.d")),
        }
    }

    /// The `certs.d` root paired with this config (derived from
    /// [`Self::registry_config_path`]).
    pub fn certs_dir(&self) -> Option<PathBuf> {
        self.registry_config_path.clone()
    }
}

/// Resolve the sandbox image: env override, else the pinned default.
pub fn sandbox_image() -> String {
    std::env::var("INIT_PRO_SANDBOX_IMAGE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SANDBOX_IMAGE.to_string())
}

/// Quote a value as a TOML basic string (escape `\` and `"`; drop raw
/// control chars, which never appear in the paths we render).
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Render the containerd `config.toml` (version 2 schema).
pub fn render(v: &ContainerdConfigVars) -> String {
    let cri = "plugins.\"io.containerd.grpc.v1.cri\"";
    format!(
        "version = 2\n\
         root = {root}\n\
         state = {state}\n\
         \n\
         [grpc]\n\
         \x20 address = {socket}\n\
         \n\
         [{cri}]\n\
         \x20 sandbox_image = {sandbox}\n\
         [{cri}.cni]\n\
         \x20 conf_dir = {conf}\n\
         \x20 bin_dir = {bin}\n\
         [{cri}.containerd]\n\
         \x20 snapshotter = {snap}\n\
         \x20 default_runtime_name = {drn}\n\
         \x20 [{cri}.containerd.runtimes.runc]\n\
         \x20   runtime_type = {rt}\n\
         {registry}",
        root = quote(&v.root.display().to_string()),
        state = quote(&v.state.display().to_string()),
        socket = quote(&v.socket.display().to_string()),
        cri = cri,
        sandbox = quote(&v.sandbox_image),
        conf = quote(&v.cni_conf_dir.display().to_string()),
        bin = quote(&v.cni_bin_dir.display().to_string()),
        snap = quote(&v.snapshotter),
        drn = quote(&v.default_runtime_name),
        rt = quote(&v.runtime_type),
        registry = registry_block(cri, v.registry_config_path.as_deref()),
    )
}

/// The `[plugins."io.containerd.grpc.v1.cri".registry]` stanza, or empty
/// when no `certs.d` root is configured (T8.1, Q40).
fn registry_block(cri: &str, config_path: Option<&Path>) -> String {
    match config_path {
        Some(path) => format!(
            "[{cri}.registry]\n\x20 config_path = {}\n",
            quote(&path.display().to_string())
        ),
        None => String::new(),
    }
}

/// The `certs.d` directory name for a registry address: scheme-free and
/// slash-free (`http://h:15000/` -> `h:15000`), containerd host-key form.
pub fn registry_host_key(addr: &str) -> String {
    addr.trim()
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string()
}

/// Render a containerd `certs.d/<host>/hosts.toml` for one plain-HTTP
/// registry host (T8.1, Q38+Q40): the host is used as-is when it already
/// carries a scheme, else `http://` is assumed (no auth/TLS in v1).
pub fn render_hosts_toml(host: &str) -> String {
    let host = host.trim().trim_end_matches('/');
    let server = if host.contains("://") {
        host.to_string()
    } else {
        format!("http://{host}")
    };
    format!(
        "server = {server_quoted}\n\n         [host.{server_quoted}]\n\
         \x20 capabilities = [\"pull\", \"resolve\", \"push\"]\n",
        server_quoted = quote(&server),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> ContainerdConfigVars {
        ContainerdConfigVars::for_data_dir(Path::new("/dd"), DEFAULT_SANDBOX_IMAGE)
    }

    #[test]
    fn for_data_dir_lays_out_k3s_paths() {
        let v = vars();
        assert_eq!(v.root, Path::new("/dd/agent/containerd/root"));
        assert_eq!(v.state, Path::new("/dd/agent/containerd/state"));
        assert_eq!(v.socket, Path::new("/dd/run/containerd/containerd.sock"));
        assert_eq!(
            v.cni_conf_dir,
            Path::new("/dd/agent/etc/containerd/cni/net.d")
        );
        assert_eq!(v.cni_bin_dir, Path::new("/dd/agent/containerd/aux"));
    }

    #[test]
    fn render_round_trips_through_toml_parser() {
        // Parse-back proves the template is valid TOML with the values where
        // containerd expects them (the parser is the same `toml` crate the
        // vendor manifest uses).
        let text = render(&vars());
        let doc: toml::Value = text.parse().expect("rendered config parses as TOML");

        assert_eq!(doc["version"].as_integer(), Some(2));
        assert_eq!(doc["root"].as_str(), Some("/dd/agent/containerd/root"));
        assert_eq!(doc["state"].as_str(), Some("/dd/agent/containerd/state"));
        assert_eq!(
            doc["grpc"]["address"].as_str(),
            Some("/dd/run/containerd/containerd.sock")
        );
        let cri = &doc["plugins"]["io.containerd.grpc.v1.cri"];
        assert_eq!(
            cri["sandbox_image"].as_str(),
            Some("registry.k8s.io/pause:3.10")
        );
        assert_eq!(
            cri["cni"]["conf_dir"].as_str(),
            Some("/dd/agent/etc/containerd/cni/net.d")
        );
        assert_eq!(
            cri["cni"]["bin_dir"].as_str(),
            Some("/dd/agent/containerd/aux")
        );
        assert_eq!(cri["containerd"]["snapshotter"].as_str(), Some("overlayfs"));
        assert_eq!(
            cri["containerd"]["default_runtime_name"].as_str(),
            Some("runc")
        );
        assert_eq!(
            cri["containerd"]["runtimes"]["runc"]["runtime_type"].as_str(),
            Some("io.containerd.runc.v2")
        );
    }

    #[test]
    fn render_reflects_a_custom_sandbox_image() {
        let v = ContainerdConfigVars::for_data_dir(Path::new("/dd"), "ghcr.io/x/pause:1");
        let doc: toml::Value = render(&v).parse().unwrap();
        assert_eq!(
            doc["plugins"]["io.containerd.grpc.v1.cri"]["sandbox_image"].as_str(),
            Some("ghcr.io/x/pause:1")
        );
    }

    #[test]
    fn quote_escapes_toml_specials_and_drops_controls() {
        assert_eq!(quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(quote("x\u{1}y"), "\"xy\"");
    }

    #[test]
    fn render_emits_registry_config_path() {
        let v = vars();
        let doc: toml::Value = render(&v).parse().expect("rendered config parses");
        assert_eq!(
            doc["plugins"]["io.containerd.grpc.v1.cri"]["registry"]["config_path"].as_str(),
            Some("/dd/agent/containerd/certs.d")
        );
    }

    #[test]
    fn render_omits_registry_block_when_disabled() {
        let mut v = vars();
        v.registry_config_path = None;
        let text = render(&v);
        assert!(!text.contains(".registry]"));
        let doc: toml::Value = text.parse().expect("rendered config parses");
        assert!(doc["plugins"]["io.containerd.grpc.v1.cri"]
            .get("registry")
            .is_none());
    }

    #[test]
    fn hosts_toml_is_plain_http_and_push_capable() {
        let text = render_hosts_toml("192.168.31.57:15000");
        let doc: toml::Value = text.parse().expect("hosts.toml parses");
        assert_eq!(doc["server"].as_str(), Some("http://192.168.31.57:15000"));
        let host = &doc["host"]["http://192.168.31.57:15000"];
        let caps: Vec<&str> = host["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(caps, vec!["pull", "resolve", "push"]);
    }

    #[test]
    fn hosts_toml_keeps_an_explicit_scheme() {
        assert!(render_hosts_toml("https://r.example:443")
            .contains("server = \"https://r.example:443\""));
        assert_eq!(registry_host_key("http://h:1/"), "h:1");
        assert_eq!(registry_host_key(" h:2 "), "h:2");
    }
}
