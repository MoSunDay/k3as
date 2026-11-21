//! Bearer-token authentication middleware (TODO **T1.3a**, decision **Q47**).
//!
//! Static token file, kubeadm CSV format (P2): one identity per line,
//!
//! ```text
//! token,user,uid,"group1,group2"
//! ```
//!
//! (the `"groups"` field is optional; groups are comma-separated inside the
//! quotes). Loaded once at startup — SIGHUP reload is deliberately out of
//! the slice (restart applies policy). No `--token-file` = the middleware
//! is not installed at all and the h2c no-auth posture is byte-identical
//! (T1.3a default-off red line, golden G01-G04).

use std::collections::HashMap;
use std::path::Path;

use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::state::AppState;

/// One authenticated identity (from the token row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub user: String,
    pub uid: String,
    pub groups: Vec<String>,
}

impl Identity {
    /// The k8s-conventional service-account group form (D7 subset: static
    /// tokens carry SA identity as a group; no SA token controller, T3.3).
    pub fn service_account_group(namespace: &str, name: &str) -> String {
        format!("system:serviceaccount:{namespace}:{name}")
    }
}

/// The static token table (token -> identity).
#[derive(Clone, Debug, Default)]
pub struct TokenAuth {
    by_token: HashMap<String, Identity>,
}

impl TokenAuth {
    /// Parse the kubeadm-style CSV. Blank lines and `#` comments are
    /// skipped; malformed rows are a fatal config error (fail closed).
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let mut by_token = HashMap::new();
        for (n, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields = split_csv_row(line);
            match fields.as_slice() {
                [token, user, uid] => {
                    by_token.insert(
                        token.clone(),
                        Identity {
                            user: user.clone(),
                            uid: uid.clone(),
                            groups: Vec::new(),
                        },
                    );
                }
                [token, user, uid, groups] => {
                    let groups: Vec<String> = groups
                        .split(',')
                        .map(|g| g.trim().to_string())
                        .filter(|g| !g.is_empty())
                        .collect();
                    by_token.insert(
                        token.clone(),
                        Identity {
                            user: user.clone(),
                            uid: uid.clone(),
                            groups,
                        },
                    );
                }
                _ => {
                    return Err(std::io::Error::other(format!(
                        "{}:{}: token file row must be token,user,uid[,\"groups\"]",
                        path.display(),
                        n + 1
                    )));
                }
            }
        }
        Ok(Self { by_token })
    }

    /// Resolve a presented bearer token.
    pub fn verify(&self, token: &str) -> Option<&Identity> {
        self.by_token.get(token)
    }

    /// Number of loaded identities (startup log).
    pub fn len(&self) -> usize {
        self.by_token.len()
    }

    /// True when no tokens are loaded.
    pub fn is_empty(&self) -> bool {
        self.by_token.is_empty()
    }
}

/// Split `token,user,uid,"g1,g2"` on commas, honoring one quoting level.
fn split_csv_row(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in line.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                out.push(cur.trim().to_string());
                cur = String::new();
            }
            _ => cur.push(ch),
        }
    }
    out.push(cur.trim().to_string());
    out
}

/// Extract the bearer token from an `Authorization` header value.
fn bearer(header: &str) -> Option<&str> {
    let rest = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))?;
    let tok = rest.trim();
    (!tok.is_empty()).then_some(tok)
}

/// Kubernetes-style `Status` body for auth failures.
fn status_body(code: StatusCode, reason: &str, message: &str) -> Response {
    let body = json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": message,
        "reason": reason,
        "code": code.as_u16(),
    });
    (code, axum::Json(body)).into_response()
}

/// The authn middleware: `Authorization: Bearer <token>` against the
/// static table; on success the [`Identity`] rides the request extensions
/// into [`crate::authz`]. 401 (not the kube `Anonymous` face) on missing
/// or unknown tokens.
pub(crate) async fn middleware(
    State(state): State<AppState>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let auth = match &state.auth {
        Some(a) => a.clone(),
        // Not armed: unreachable (the layer is only installed with auth),
        // but pass through rather than lock the API shut.
        None => return next.run(req).await,
    };
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let identity = match header.and_then(bearer) {
        None => return status_body(StatusCode::UNAUTHORIZED, "Unauthorized", "Unauthorized"),
        Some(token) => match auth.verify(token) {
            Some(id) => id.clone(),
            None => return status_body(StatusCode::UNAUTHORIZED, "Unauthorized", "Unauthorized"),
        },
    };
    req.extensions_mut().insert(identity);
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(rows: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("initpro-authn-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("tokens.csv");
        std::fs::write(&p, rows).unwrap();
        p
    }

    #[test]
    fn parses_kubeadm_rows() {
        let p = write_tmp(
            "# admin token\n\
             abc,admin,1,\"system:masters\"\n\
             nodetok,kubelet-node1,u2,\"system:nodes,beta\"\n\
             bare,user3,u3\n",
        );
        let auth = TokenAuth::load(&p).unwrap();
        assert_eq!(auth.len(), 3);
        let admin = auth.verify("abc").unwrap();
        assert_eq!(admin.user, "admin");
        assert_eq!(admin.groups, vec!["system:masters".to_string()]);
        let node = auth.verify("nodetok").unwrap();
        assert_eq!(
            node.groups,
            vec!["system:nodes".to_string(), "beta".to_string()]
        );
        assert!(auth.verify("bare").unwrap().groups.is_empty());
        assert!(auth.verify("nope").is_none());
    }

    #[test]
    fn malformed_row_fails_closed() {
        let p = write_tmp("only-one-field\n");
        assert!(TokenAuth::load(&p).is_err());
    }

    #[test]
    fn bearer_extraction() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("Bearer   spaced  "), Some("spaced"));
        assert_eq!(bearer("bearer lower"), Some("lower"));
        assert_eq!(bearer("Basic zzz"), None);
        assert_eq!(bearer("Bearer "), None);
    }
}
