//! T7.7 (Q63) hardened Agent sandbox: pure selection + spec contract.
//! Mirrors the T4.15 host_direct selection pattern: fail-closed, never a
//! silent fallback to the legacy Q37 subprocess path.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use api::agent_policy::{
    AgentSandboxSpec, EffectivePolicy, PolicyBaseline, SandboxLimits, ToolClass,
};
use engine_proto::{ContainerSpec, MountSpec};

/// Node-side enable flag: any truthy value turns the default baseline on.
const ENV_ENABLE: &str = "INIT_PRO_AGENT_SANDBOX";
/// Node-level MANDATORY hardening switch (T7.7, Q63): any truthy value
/// refuses Agents WITHOUT `spec.sandbox` outright (Pending) instead of
/// the legacy Q37 direct subprocess — and implies [`ENV_ENABLE`].
const ENV_REQUIRE: &str = "INIT_PRO_AGENT_REQUIRE_SANDBOX";
/// Broker knob: the single loopback upstream URL (never a generic proxy).
const ENV_BROKER_UPSTREAM: &str = "INIT_PRO_AGENT_BROKER_UPSTREAM";
/// Broker knob: comma-separated operation allowlist.
const ENV_BROKER_OPS: &str = "INIT_PRO_AGENT_BROKER_OPS";
/// Broker knob: upstream credential header name (default `Authorization`).
const ENV_BROKER_HEADER: &str = "INIT_PRO_AGENT_BROKER_CREDENTIAL_HEADER";
/// Broker knob: the upstream credential VALUE (operator runtime env — a
/// secret, never committed, never logged, never inside the sandbox).
const ENV_BROKER_CREDENTIAL: &str = "INIT_PRO_AGENT_BROKER_CREDENTIAL";

/// Outcome of the hardened-selection gate (Q63): `Selected` runs the
/// sandbox path; `NotSelected` keeps the legacy Q37 path; `Invalid`
/// fails closed (stays Pending) — never a silent fallback. `RequiredRefused`
/// is the node-level mandatory-hardening refusal (T7.7): the node mandates
/// sandboxes but the Agent has none to run — also Pending, never a legacy
/// spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxSelection {
    Selected,
    NotSelected,
    Invalid(String),
    RequiredRefused(String),
}

/// `node_ready` = the node-side sandbox config exists AND a runner binary
/// is staged AND the hardened agent-runner is staged (containment stack
/// available). Hardened request + not ready = `Invalid` (fail-closed
/// actionable reason) — NEVER the legacy path (Q63). `require_hardened`
/// (the node-level switch) refuses sandbox-less Agents before any of it.
pub fn select_sandbox(
    sandbox: Option<&AgentSandboxSpec>,
    node_ready: bool,
    require_hardened: bool,
) -> SandboxSelection {
    match sandbox {
        None if require_hardened => SandboxSelection::RequiredRefused(
            "node mandates hardened sandboxes (INIT_PRO_AGENT_REQUIRE_SANDBOX, T7.7 Q63) but \
             this Agent has no spec.sandbox; agent stays Pending — no legacy spawn"
                .to_string(),
        ),
        None => SandboxSelection::NotSelected,
        Some(_) if node_ready => SandboxSelection::Selected,
        Some(_) => SandboxSelection::Invalid(
            "hardened sandbox requested but node containment unavailable (Q63) — sandbox config \
             and staged runners incl. the hardened agent-runner (INIT_PRO_AGENT_HARDENED_RUNNER_\
             BIN); agent stays Pending — no fallback to the direct subprocess path"
                .to_string(),
        ),
    }
}

/// Operator-owned node-side sandbox configuration (Q63). `None` on
/// [`crate::agents::AgentRunnerConfig`] = hardened Agents stay Pending
/// (fail-closed).
#[derive(Debug, Clone)]
pub struct SandboxNodeConfig {
    pub baseline: PolicyBaseline,
    pub broker: Option<crate::agent_broker::BrokerConfig>,
    /// Node-level mandatory hardening (T7.7, Q63): sandbox-less Agents are
    /// refused (`SandboxRequired` Pending), never spawned the legacy way.
    pub require_hardened: bool,
}

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The default broker: `Some` only when upstream + credential + ops are
/// ALL present (a partial broker config is ignored, never half-enabled).
fn broker_from_env() -> Option<crate::agent_broker::BrokerConfig> {
    let upstream_url = env_non_empty(ENV_BROKER_UPSTREAM)?;
    let credential_value = env_non_empty(ENV_BROKER_CREDENTIAL)?;
    let ops_raw = env_non_empty(ENV_BROKER_OPS)?;
    let allowed_operations: Vec<String> = ops_raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if allowed_operations.is_empty() {
        return None;
    }
    Some(crate::agent_broker::BrokerConfig {
        upstream_url,
        credential_header: env_non_empty(ENV_BROKER_HEADER)
            .unwrap_or_else(|| "Authorization".to_string()),
        credential_value,
        allowed_operations,
    })
}

impl SandboxNodeConfig {
    /// `INIT_PRO_AGENT_SANDBOX=1` enables a default baseline (Ordinary +
    /// Subagent tools; empty bind allowlist — operator Stage A adds slots
    /// via env later; caps 2000 millis / 2 GiB / 256 pids). The structural
    /// workspace/scratch/runner binds are NOT spec binds, so the empty
    /// allowlist still lets a bind-less hardened Agent run. Broker knobs
    /// (all optional, see the `ENV_BROKER_*` consts above).
    /// `INIT_PRO_AGENT_REQUIRE_SANDBOX=1` additionally refuses sandbox-less
    /// Agents (T7.7) and implies enablement.
    pub fn from_env() -> Option<Self> {
        let require_hardened = truthy(&std::env::var(ENV_REQUIRE).unwrap_or_default());
        if !require_hardened && !truthy(&std::env::var(ENV_ENABLE).unwrap_or_default()) {
            return None;
        }
        let baseline = PolicyBaseline {
            tools: BTreeSet::from([ToolClass::Ordinary, ToolClass::Subagent]),
            provider_operations: BTreeSet::new(),
            bind_allowlist: Vec::new(),
            limits_cap: SandboxLimits {
                cpu_millis: 2000,
                memory_bytes: 2 * 1024 * 1024 * 1024,
                pids_max: 256,
            },
        };
        Some(Self {
            baseline,
            broker: broker_from_env(),
            require_hardened,
        })
    }
}

/// Container-internal layout (Q63): workspace rw at `/workspace` (common
/// MOUNT_TARGET convention), scratch rw at `/scratch`, runner content
/// staged INTO the private rootfs at `/runner` (root-owned => read-only
/// for the sandbox uid by ownership; nsrun `:ro` binds are noexec, so a
/// ro BIND could never exec the runner — see `stage_runner_into_rootfs`),
/// broker socket rw at `/broker/provider.sock` (connect() needs write on
/// the socket inode; the broker's operation allowlist is the access
/// control).
pub const WORKSPACE_TARGET: &str = "/workspace";
pub const SCRATCH_TARGET: &str = "/scratch";
pub const RUNNER_TARGET: &str = "/runner";
pub const BROKER_TARGET: &str = "/broker/provider.sock";
pub const BROKER_ENV: &str = "INIT_PRO_AGENT_BROKER";

/// Everything a hardened attempt needs to build its engine spec (pure).
pub struct SandboxInputs {
    pub attempt_id: String,
    /// Host-side PRIVATE rootfs dir (never the host root `/`).
    pub rootfs: PathBuf,
    pub eff: EffectivePolicy,
    pub workspace_host: PathBuf,
    pub scratch_host: PathBuf,
    /// Staged runner FILE; its parent dir is copied INTO the private
    /// rootfs at `/runner` (see `stage_runner_into_rootfs`), argv[0] =
    /// `/runner/<basename>`.
    pub runner_host: Option<PathBuf>,
    pub broker_socket: Option<PathBuf>,
    /// Full in-container argv incl. argv[0].
    pub argv: Vec<String>,
    /// Sanitized `K=V` pairs (the closed allowlist from `sanitize_env`).
    pub env: Vec<String>,
    pub hostname: Option<String>,
}

/// alnum/`-`/`_`/`.` else `-`, <=48 chars, non-empty fallback `attempt`
/// (same rules as the engine convert sanitize + agent_broker names).
pub fn sanitize_attempt_id(raw: &str) -> String {
    let out: String = raw
        .chars()
        .take(48)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if out.is_empty() {
        "attempt".to_string()
    } else {
        out
    }
}

/// cgroup leaf name (engine convert `sanitize` rules, local mirror — the
/// engine crate cannot be a dependency here and vice versa).
fn cgroup_name(attempt_id: &str) -> String {
    sanitize_attempt_id(attempt_id)
}

/// CRI `cpu.max` quota string: `<quota> <period>` with the period fixed
/// at 100000 (µs) and quota = millis * 100 (500m = 0.5 CPU = 50000µs
/// per 100000µs period — Kubernetes CPU-milli semantics).
fn cpu_max(cpu_millis: u64) -> String {
    format!("{} 100000", cpu_millis.saturating_mul(100))
}

/// Pure spec assembly (Q63): private rootfs, non-root identity, empty
/// capability set, fresh netns, no_new_privs, mandatory cgroup limits.
pub fn sandbox_spec(inputs: &SandboxInputs) -> ContainerSpec {
    let mut mounts = vec![
        MountSpec {
            source: inputs.workspace_host.display().to_string(),
            target: WORKSPACE_TARGET.to_string(),
            readonly: false,
        },
        MountSpec {
            source: inputs.scratch_host.display().to_string(),
            target: SCRATCH_TARGET.to_string(),
            readonly: false,
        },
    ];
    if let Some(socket) = &inputs.broker_socket {
        mounts.push(MountSpec {
            source: socket.display().to_string(),
            target: BROKER_TARGET.to_string(),
            readonly: false,
        });
    }
    for bind in &inputs.eff.binds {
        mounts.push(MountSpec {
            source: bind.source.clone(),
            target: bind.target.clone(),
            readonly: bind.readonly,
        });
    }
    ContainerSpec {
        id: inputs.attempt_id.clone(),
        rootfs: inputs.rootfs.display().to_string(),
        argv: inputs.argv.clone(),
        env: inputs.env.clone(),
        hostname: inputs.hostname.clone(),
        cwd: WORKSPACE_TARGET.to_string(),
        uid: inputs.eff.uid,
        gid: inputs.eff.gid,
        mounts,
        new_net: true,
        netns: None,
        overlay: None,
        host_root: false,
        pause: false,
        log_path: None,
        no_new_privs: true,
        caps: 0,
        cgroup_name: Some(cgroup_name(&inputs.attempt_id)),
        memory_max: Some(inputs.eff.limits.memory_bytes),
        cpu_max: Some(cpu_max(inputs.eff.limits.cpu_millis)),
        pids_max: Some(inputs.eff.limits.pids_max),
    }
}

/// Q63 invariants of a hardened sandbox spec (unit + e2e assertion face).
/// Every message carries an actionable "(Q63)" remedy.
pub fn validate_hardened(spec: &ContainerSpec) -> Result<(), String> {
    if spec.rootfs == "/" {
        return Err("rootfs must be the private attempt dir, not the host root \"/\" (Q63)".into());
    }
    if spec.host_root {
        return Err(
            "host_root must be false — the host root is never the sandbox root (Q63)".into(),
        );
    }
    if !spec.new_net || spec.netns.is_some() {
        return Err("new_net must be true with no netns join — a fresh network namespace is mandatory (Q63)".into());
    }
    if spec.uid == 0 || spec.gid == 0 {
        return Err("uid and gid must both be non-root (Q63)".into());
    }
    if spec.caps != 0 {
        return Err("caps must be 0 — the sandbox runs with an empty capability set (Q63)".into());
    }
    if !spec.no_new_privs {
        return Err("no_new_privs must be true (Q63)".into());
    }
    if spec.memory_max.is_none() {
        return Err("memory_max is mandatory — sandbox memory must be bounded (Q63)".into());
    }
    if spec.cpu_max.is_none() {
        return Err("cpu_max is mandatory — sandbox CPU must be bounded (Q63)".into());
    }
    if spec.pids_max.is_none() {
        return Err("pids_max is mandatory — sandbox process count must be bounded (Q63)".into());
    }
    if spec.cgroup_name.as_deref().unwrap_or("").is_empty() {
        return Err(
            "cgroup_name is mandatory — the sandbox cgroup is the descendant-kill surface (Q63)"
                .into(),
        );
    }
    for m in &spec.mounts {
        if m.source == "/" {
            return Err(format!(
                "mount {} -> {}: source is the host root \"/\" (Q63)",
                m.source, m.target
            ));
        }
        for surface in ["/dev", "/proc", "/sys"] {
            if m.source == surface || m.source.starts_with(&format!("{surface}/")) {
                return Err(format!(
                    "mount {} -> {}: source is under the host control surface \"{surface}\" (Q63)",
                    m.source, m.target
                ));
            }
        }
        if m.readonly
            && [WORKSPACE_TARGET, SCRATCH_TARGET, BROKER_TARGET].contains(&m.target.as_str())
        {
            return Err(format!(
                "mount {} -> {}: the writable surfaces {WORKSPACE_TARGET}/{SCRATCH_TARGET}/{BROKER_TARGET} must not be readonly (Q63)",
                m.source, m.target
            ));
        }
    }
    Ok(())
}

/// The sandbox env is a CLOSED allowlist: exactly the spawn-plan envs
/// (HOME/XDG_DATA_HOME/PATH rewritten to container paths by the caller)
/// plus the broker socket var. Inherited process env — including any
/// operator credential keys — is dropped entirely (Q63 supersedes the
/// Q37 child-env credential transport on this path).
pub fn sanitize_env(plan_envs: &[(String, String)], broker_socket: Option<&Path>) -> Vec<String> {
    let mut out: Vec<String> = plan_envs.iter().map(|(k, v)| format!("{k}={v}")).collect();
    if let Some(socket) = broker_socket {
        out.push(format!("{BROKER_ENV}={}", socket.display()));
    }
    out
}

/// Private per-attempt rootfs skeleton under `<data>/agent-sandbox/<id>/`:
/// `rootfs/` (empty private root, NEVER the host root), workspace+scratch
/// live with the attempt dirs. Idempotent. Returns `(rootfs, scratch)`.
/// `scratch/` is the designated writable surface besides the workspace, so
/// it is opened to the sandbox uid (Q63); `rootfs/` STAYS root-owned 0755 —
/// writes outside workspace/scratch must fail for the non-root identity.
pub fn prepare_sandbox_dirs(data_dir: &Path, attempt_id: &str) -> io::Result<(PathBuf, PathBuf)> {
    let base = data_dir
        .join("agent-sandbox")
        .join(sanitize_attempt_id(attempt_id));
    let rootfs = base.join("rootfs");
    let scratch = base.join("scratch");
    std::fs::create_dir_all(&rootfs)?;
    std::fs::create_dir_all(&scratch)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o777))?;
    Ok((rootfs, scratch))
}

/// Best-effort recursive chmod opening the per-agent workspace tree to the
/// sandbox uid (Q63: the workspace is a designated writable surface). The
/// legacy Q37 path runs as the agent's own user and never calls this.
/// Failures are ignored (NFS mode semantics are server-side, Q36).
pub(crate) fn open_workspace_to_sandbox(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let walk = |d: &Path| {
        let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o777));
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                if let Ok(ft) = e.file_type() {
                    if ft.is_dir() {
                        let _ = std::fs::set_permissions(
                            e.path(),
                            std::fs::Permissions::from_mode(0o777),
                        );
                    }
                }
            }
        }
    };
    walk(dir);
}
