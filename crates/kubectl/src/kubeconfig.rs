//! Read-only kubeconfig resolution (TODO **T1.3a** D3, decision **Q47**).
//!
//! The supported-subset kubectl reads exactly three fields from the
//! kubeconfig: `current-context` -> context's `cluster`/`user` ->
//! `clusters[].cluster.server` + `certificate-authority` and
//! `users[].user.token`. No client-cert auth, no exec plugins, no
//! write-back — a missing or unparseable file is simply "no kubeconfig"
//! (the default server + anonymous h2c posture stands).
//!
//! Server precedence: `--server` flag > `INIT_PRO_SERVER` env >
//! kubeconfig server > [`crate::DEFAULT_SERVER`] (`http://127.0.0.1:6443`).
//! https URLs are honored ONLY when they come from the kubeconfig itself
//! AND it carries a `certificate-authority` — a flag/env https URL stays
//! the historical hard error (Q21/G53 pin).

use serde::Deserialize;

/// The trimmed kubeconfig view this client needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Kubeconfig {
    /// Resolved cluster server URL (may be `https://`).
    pub(crate) server: String,
    /// Filesystem path of the cluster CA bundle (`certificate-authority`).
    pub(crate) certificate_authority: Option<String>,
    /// Static bearer token (`users[].user.token`).
    pub(crate) token: Option<String>,
}

/// One `clusters[]` entry (only the fields we read).
#[derive(Debug, Clone, Default, Deserialize)]
struct Cluster {
    #[serde(rename = "certificate-authority")]
    certificate_authority: Option<String>,
    server: Option<String>,
}

/// One `clusters[]` item.
#[derive(Debug, Clone, Default, Deserialize)]
struct NamedCluster {
    name: String,
    #[serde(default)]
    cluster: Cluster,
}

/// One `users[]` entry (only the token).
#[derive(Debug, Clone, Default, Deserialize)]
struct User {
    token: Option<String>,
}

/// One `users[]` item.
#[derive(Debug, Clone, Default, Deserialize)]
struct NamedUser {
    name: String,
    #[serde(default)]
    user: User,
}

/// One `contexts[]` entry (the inner `context:` map).
#[derive(Debug, Clone, Default, Deserialize)]
struct Context {
    cluster: Option<String>,
    user: Option<String>,
}

/// One `contexts[]` item.
#[derive(Debug, Clone, Default, Deserialize)]
struct NamedContext {
    name: String,
    #[serde(default)]
    context: Context,
}

/// The whole document (unknown keys ignored).
#[derive(Debug, Deserialize)]
struct Document {
    #[serde(rename = "current-context")]
    current_context: Option<String>,
    #[serde(default)]
    clusters: Vec<NamedCluster>,
    #[serde(default)]
    users: Vec<NamedUser>,
    #[serde(default)]
    contexts: Vec<NamedContext>,
}

/// Resolve the kubeconfig path: `$KUBECONFIG` (first `:`-separated entry,
/// kubectl convention) else `~/.kube/config`. `None` when unset/empty or
/// the home dir is unknown.
pub(crate) fn path(kubeconfig_env: Option<String>) -> Option<std::path::PathBuf> {
    if let Some(v) = kubeconfig_env {
        let first = v.split(':').next().unwrap_or("").trim().to_string();
        return (!first.is_empty()).then(|| first.into());
    }
    let home = std::env::var_os("HOME")?;
    let p = std::path::Path::new(&home).join(".kube").join("config");
    Some(p)
}

/// Load + trim the kubeconfig; `None` = missing/unparseable/empty (not an
/// error — the default server posture stands).
pub(crate) fn load(kubeconfig_env: Option<String>) -> Option<Kubeconfig> {
    let path = path(kubeconfig_env)?;
    let raw = std::fs::read_to_string(path).ok()?;
    let doc: Document = serde_yaml_ng::from_str(&raw).ok()?;
    // current-context -> context -> cluster + user names. Without a
    // current-context the first cluster wins (k3s write-kubeconfig shape).
    let (cluster_name, user_name) = match &doc.current_context {
        Some(current) => {
            let entry = doc.contexts.iter().find(|c| c.name == *current)?;
            (entry.context.cluster.clone(), entry.context.user.clone())
        }
        None => (None, doc.users.first().map(|u| u.name.clone())),
    };
    let cluster = match cluster_name {
        Some(name) => doc
            .clusters
            .iter()
            .find(|c| c.name == name)
            .map(|c| &c.cluster),
        None => doc.clusters.first().map(|c| &c.cluster),
    }?;
    let server = cluster.server.clone()?;
    if server.trim().is_empty() {
        return None;
    }
    let token = user_name
        .and_then(|u| doc.users.iter().find(|x| x.name == u))
        .and_then(|u| u.user.token.clone());
    Some(Kubeconfig {
        server,
        certificate_authority: cluster.certificate_authority.clone(),
        token,
    })
}

/// The outcome of the server-precedence chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// An explicit flag/env/default URL: plain-HTTP-only semantics —
    /// `https://` is the historical hard error (Q21, G53).
    Explicit(String),
    /// The kubeconfig won: its server URL + CA (+ optional token) ride
    /// along; `https://` is allowed with a mandatory CA (Q47).
    Kubeconfig(Kubeconfig),
}

/// flag/env (`explicit`, when set) > kubeconfig > [`crate::DEFAULT_SERVER`].
pub(crate) fn resolve(explicit: Option<String>, kc: Option<Kubeconfig>) -> Resolved {
    match explicit {
        Some(url) => Resolved::Explicit(url),
        None => match kc {
            Some(kc) if !kc.server.trim().is_empty() => Resolved::Kubeconfig(kc),
            _ => Resolved::Explicit(crate::DEFAULT_SERVER.to_string()),
        },
    }
}

/// Process-env convenience: load the kubeconfig from `$KUBECONFIG` or
/// `~/.kube/config`.
pub(crate) fn load_from_env() -> Option<Kubeconfig> {
    load(std::env::var("KUBECONFIG").ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_cfg(dir: &std::path::Path, body: &str) -> Option<std::path::PathBuf> {
        let p = dir.join("config");
        std::fs::write(&p, body).unwrap();
        Some(p)
    }

    #[test]
    fn resolve_precedence() {
        let kc = Kubeconfig {
            server: "https://k.example:6443".into(),
            certificate_authority: Some("/ca.crt".into()),
            token: Some("sekrit".into()),
        };
        // explicit flag/env beats kubeconfig
        assert_eq!(
            resolve(Some("http://x:1".into()), Some(kc.clone())),
            Resolved::Explicit("http://x:1".into())
        );
        // kubeconfig beats the default
        assert_eq!(resolve(None, Some(kc.clone())), Resolved::Kubeconfig(kc));
        // no kubeconfig -> default
        assert_eq!(
            resolve(None, None),
            Resolved::Explicit(crate::DEFAULT_SERVER.into())
        );
        // empty server -> default
        let empty = Kubeconfig {
            server: "  ".into(),
            certificate_authority: None,
            token: None,
        };
        assert_eq!(
            resolve(None, Some(empty)),
            Resolved::Explicit(crate::DEFAULT_SERVER.into())
        );
    }

    #[test]
    fn loads_current_context_chain() {
        let dir = std::env::temp_dir().join(format!("initpro-kc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = r#"
apiVersion: v1
kind: Config
current-context: prod
clusters:
  - name: ignored
    cluster: { server: http://ignored:1 }
  - name: prod
    cluster:
      server: https://prod.example:6443
      certificate-authority: /etc/prod-ca.crt
contexts:
  - name: prod
    context: { cluster: prod, user: admin }
users:
  - name: admin
    user: { token: tok-admin }
  - name: other
    user: { token: tok-other }
"#;
        // load() takes the env value, not the path: point KUBECONFIG at it.
        let p = write_cfg(&dir, cfg).unwrap();
        std::env::set_var("KUBECONFIG", &p);
        let kc = load_from_env().expect("parses");
        std::env::remove_var("KUBECONFIG");
        assert_eq!(kc.server, "https://prod.example:6443");
        assert_eq!(
            kc.certificate_authority.as_deref(),
            Some("/etc/prod-ca.crt")
        );
        assert_eq!(kc.token.as_deref(), Some("tok-admin"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_or_bad_file_is_none() {
        assert!(load(Some("/nonexistent/init-pro-kc-test/config".into())).is_none());
        let dir = std::env::temp_dir().join(format!("initpro-kc-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = write_cfg(&dir, "::: not yaml ][").unwrap();
        assert!(load(Some(p.to_string_lossy().into())).is_none());
        // a document without clusters is None, not a panic
        let p = write_cfg(&dir, "apiVersion: v1\nkind: Config\n").unwrap();
        assert!(load(Some(p.to_string_lossy().into())).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
