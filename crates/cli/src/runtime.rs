//! Subcommand implementations (Phase 1).
//!
//! `server` installs the graceful-shutdown handler, builds the served schema
//! registry, and serves the T1.2a HTTP discovery endpoints (`/api`, `/apis`,
//! `/api/v1`, `/apis/<g>/<v>`) on the bind address — draining on signal.
//! Since T3.1a the server also runs the controller manager against the same
//! storage Arc as the apiserver, in-process (decision **Q19**), stopping it
//! after the API surface has drained.
//! `agent` installs the handler, supervises the bundled containerd runtime
//! (T4.1, Q25), and runs the kubelet equivalent (T4.2 Scope A) when a join
//! URL is given. `stage` exposes the T0.2 manifest contract + B5 runtime
//! staging.
//!
//! **T1.3a** pre-split (Sprint 50, decision **Q47**): the server-plane
//! wiring (`ServerBind`/`RegistryBind`, `run_server`, and the apiserver/
//! controllers/scheduler/service-plane/anyhome/registry spawn block) lives
//! in `runtime_server.rs`, re-exported below (file-size limit, the
//! `storage_dsn.rs` precedent).

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use ::runtime as node_runtime;
use infra::{Config, Shutdown};

pub use crate::runtime_server::{run_server, RegistryBind, ServerBind};

// T4.12 (Q49/Q51): the agent's CLI flag surface is positional by design
// (k3s parity); bundling it into a struct mid-T4.12 would churn every
// caller for no behavioral gain.
#[allow(clippy::too_many_arguments)]
pub fn run_agent(
    cfg: Config,
    server_url: Option<String>,
    node_name: Option<String>,
    node_ip: Option<String>,
    kubelet_port: u16,
    flannel: crate::flannel::FlannelNet,
    // T4.12 (Q48/Q51): "builtin" runs the node on the built-in engine
    // (after the boot probe); anything else means the staged containerd.
    container_engine: String,
    // T7.1 Phase B (Q37): `Some(anyhome_addr)` enables the node-side Agent
    // execution loop (see `run_supervised`).
    agent_exec_addr: Option<String>,
    // T4.13 M2 (Q54): the default registry the built-in engine pulls
    // host-less image references from (`--registry-addr`).
    registry_addr: String,
    // T4.13 M5 (Q53): soft builtin image-store quota in bytes (0 = off).
    builtin_image_quota: u64,
    // T4.14 (Q55): rootfs source for the built-in engine (host|image).
    builtin_rootfs: String,
    // T7.2 (Q61): whole-GPU inventory source (auto|none|fake:<r>:<n>).
    gpu_inventory: String,
    // T1.3a (Q47 P3): static bearer token the kubelet/Agent/flannel
    // loops present to a token-file-authenticated apiserver.
    server_token: Option<String>,
    // T4.4 (Q64): NetworkPolicy enforcement on/off (`--disable-network-
    // policy` opts out; enforced only for kubelet+flannel nodes).
    netpol_enabled: bool,
    // T4.5 (Q65): reverse relay tunnel on/off (`--disable-proxy-tunnel`
    // opts out; the node then serves hops over the direct dial only).
    tunnel_enabled: bool,
) -> ExitCode {
    // T7.2 (Q61): fail fast on a bad --gpu-inventory value — the GPU
    // plane's ledger/publication contract must never boot half-parsed.
    let gpu_inventory = match kubelet::parse_inventory(&gpu_inventory) {
        Ok(src) => src,
        Err(e) => {
            eprintln!("init-pro agent: invalid --gpu-inventory value: {e}");
            return ExitCode::FAILURE;
        }
    };
    // T4.5 (Q65): `--node-ip` unset -> auto-detect the primary outbound IP
    // towards the apiserver host (no packet leaves; fallback loopback) so
    // BOTH the direct hop and the reverse tunnel address the kubelet.
    let node_ip = node_ip.unwrap_or_else(|| {
        let detected = server_url
            .as_deref()
            .map(kubelet::detect_outbound_ip)
            .unwrap_or_else(|| "127.0.0.1".to_string());
        tracing::info!(target: "init-pro", node_ip = %detected, "auto-detected --node-ip (T4.5)");
        detected
    });
    run_supervised(
        "agent",
        cfg,
        None,
        server_url,
        node_name,
        Some((node_ip, kubelet_port)),
        Some(flannel),
        agent_exec_addr,
        container_engine,
        registry_addr,
        builtin_image_quota,
        builtin_rootfs,
        gpu_inventory,
        server_token,
        netpol_enabled,
        tunnel_enabled,
    )
}

/// `server` serves discovery when given a [`ServerBind`]; `agent` supervises
/// the bundled containerd runtime (T4.1) and, with `server_url` + a staged
/// crictl, the kubelet loops (T4.2 Scope A) under `node_name`. The kubelet's
/// local log/exec listener (Scope C, Q31) binds/advertises
/// `(node_ip, kubelet_port)` when present.
/// How long the drain waits for CRI to report zero sandboxes after the
/// kubelet's teardown (T4.1 follow-up, Sprint 28). Matches the golden
/// harness's shim grace window so the two bounds agree.
const SANDBOX_GONE_TIMEOUT: Duration = Duration::from_secs(5);

/// T6.3 (Q40): cap the registry's graceful HTTP drain so an interrupted blob upload cannot wedge shutdown.
const REGISTRY_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// S53 / T4.4 (Q64) review P1-⑤: teardown is the NP4 zero-residue drain, but a wedged keeper must not hang agent exit — abort + WARN (mirrors REGISTRY_DRAIN_TIMEOUT).
const NETPOL_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// C8 (long-run bug sweep): flanneld's drain runs the iptables teardown
/// through child processes; a wedged `iptables` (xtables lock) must not hang
/// agent exit. Same posture as the registry/anyhome drains: abort + WARN.
const FLANNEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Batch D (C6), T4.2/Q31: the kubelet's shutdown teardown (stopp+rmp of
/// every sandbox) is bounded work -- a pause child parked in D-state must
/// not hang agent exit. Longer than the others: a full sandbox list is
/// stopped here. Same posture: abort + WARN.
const KUBELET_DRAIN_TIMEOUT: Duration = Duration::from_secs(20);
/// B12 (long-run bug sweep): the NFS data plane (Q36) joins unbounded today --
/// an NFS client holding a handle open past the listener close must not hang
/// server exit. Same budget and posture as the registry drain: abort + WARN.
const ANYHOME_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll the CRI sandbox list until it is empty (bounded by `max`). An empty
/// list means every shim of this run is gone, so stopping containerd cannot
/// orphan one. Two consecutive list errors are tolerated (the daemon may
/// already be gone); persistent failure ends the wait as "not confirmed".
async fn wait_sandboxes_gone(cri: &node_runtime::CriCtl, max: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + max;
    let mut errors = 0u32;
    loop {
        match cri.list_pod_sandboxes().await {
            Ok(list) if list.is_empty() => return true,
            Ok(_) => errors = 0,
            Err(e) => {
                errors += 1;
                if errors > 2 {
                    tracing::debug!(
                        target: "init-pro",
                        error = %e,
                        "CRI list failed during drain; assuming daemon gone"
                    );
                    return false;
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_supervised(
    role: &'static str,
    cfg: Config,
    bind: Option<ServerBind>,
    server_url: Option<String>,
    node_name: Option<String>,
    kubelet_addr: Option<(String, u16)>,
    flannel: Option<crate::flannel::FlannelNet>,
    // T7.1 Phase B (Q37): agent-mode only. `Some(addr)` runs the Agent
    // execution loop with `addr` as the fallback anyhome NFS address for
    // Agents without a workspace annotation.
    agent_exec_addr: Option<String>,
    container_engine: String,
    // T4.13 M2 (Q54): default registry for host-less image refs (builtin
    // engine pulls; inert for the staged containerd path).
    registry_addr: String,
    // T4.13 M5 (Q53): soft builtin image-store quota (0 = off; builtin
    // engine only — the staged containerd path ignores it).
    builtin_image_quota: u64,
    // T4.14 (Q55): rootfs source for the built-in engine (host|image).
    builtin_rootfs: String,
    // T7.2 (Q61): whole-GPU inventory source (parsed by the caller).
    gpu_inventory: kubelet::GpuInventorySource,
    // T1.3a (Q47 P3): static apiserver bearer token (`--token`/env).
    server_token: Option<String>,
    // T4.4 (Q64): NetworkPolicy enforcement (agent mode only — the
    // server passes `false`; it runs no node data plane).
    netpol_enabled: bool,
    // T4.5 (Q65): reverse relay tunnel dialer (agent mode only; the
    // server passes `false` — it dials nodes directly or not at all).
    tunnel_enabled: bool,
) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name(role)
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("init-pro {role}: failed to start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = rt.block_on(async move {
        let shutdown = Shutdown::new();

        if let Err(e) = infra::signal::install(shutdown.clone()).await {
            tracing::warn!(target: "init-pro", role, "signal handler install failed: {e}");
        }

        // T1.2a: build the served schema registry, serve HTTP discovery, and
        // keep a handle so the process waits for full drain on shutdown.
        // T3.1a (decision Q19): the controller manager runs in-process,
        // sharing the apiserver's storage Arc, so controller reads/writes hit
        // the same store the API surface serves (no loopback HTTP hop).
        // T4.1 (Q25): the agent supervises the bundled containerd (stage ->
        // render config -> supervise with backoff). The server keeps the
        // runtime off by default (single-node UX arrives with T4.5/e2e).
        let mut runtime_drain: Option<tokio::task::JoinHandle<()>> = None;
        // T4.1 follow-up (Sprint 28): the supervisor reacts to its OWN token,
        // fired only after the kubelet's sandbox teardown drained CRI. The
        // shared process token must not race the daemon kill against
        // in-flight teardown — that race orphaned shims under load (they
        // outlive containerd holding shm/overlay mounts; datadir rm EBUSY).
        let runtime_shutdown = Shutdown::new();
        // T4.3 Scope A (Q34): embedded flanneld (kube subnet-mgr). Drained
        // after the kubelet, before the containerd runtime.
        let mut flannel_drain: Option<tokio::task::JoinHandle<()>> = None;
        // T4.4 (Q64): node-side NetworkPolicy enforcement keeper. Drained
        // after flanneld — its teardown (zero-residue iptables, NP4) must
        // complete before the process exits.
        let mut netpol_drain: Option<tokio::task::JoinHandle<()>> = None;
        // T4.5 (Q65): the reverse relay tunnel dialer. Drained after the
        // netpol keeper — it holds only one outbound long-poll, so its
        // exit is instant once the shutdown token interrupts the poll.
        let mut tunnel_drain: Option<tokio::task::JoinHandle<()>> = None;
        // T4.12 (Q49/Q51): engine selection for this boot. `builtin` runs
        // the node on the built-in engine once the Q51 boot probe passes;
        // any probe failure falls back to the staged containerd with a
        // Warning. The choice is recorded under run/engine/mode (multicall
        // peers read it to answer "no containerd socket here").
        let builtin = if bind.is_none() && container_engine == "builtin" {
            let picked = matches!(
                crate::engine_probe::probe(&cfg.data_dir, role).await,
                crate::engine_probe::Outcome::Builtin
            );
            crate::engine_probe::write_mode(&cfg.data_dir, picked);
            picked
        } else {
            false
        };
        if bind.is_none() && !builtin {
            // T8.1 (Q38/Q40): the agent's `--registry-addr` becomes the
            // containerd `certs.d/<host>/hosts.toml` plain-HTTP face.
            match node_runtime::start_agent_runtime_with(
                &cfg.data_dir,
                Some(registry_addr.as_str()),
                runtime_shutdown.clone(),
            ) {
                Ok(task) => {
                    runtime_drain = Some(tokio::spawn(async move {
                        let _ = task.await;
                    }))
                }
                Err(e) => tracing::warn!(
                    target: "init-pro",
                    role,
                    "containerd source unavailable; agent degraded (T4.2 will \
                     hard-require the runtime): {e}"
                ),
            }
        }
        // T4.2 Scope A: with a join URL the agent also runs the kubelet
        // equivalent — pod watch + sync over CRI + node/lease registration —
        // against the apiserver HTTP surface (Q21). The kubelet stays off
        // (agent keeps booting) when the staged crictl tree (T4.1) or a
        // v1-compatible http:// URL is missing.
        let mut kubelet_drain: Option<Vec<tokio::task::JoinHandle<()>>> = None;
        // The same CRI handle the kubelet drives, kept for the drain's
        // "sandboxes gone" confirmation (T4.1 follow-up, Sprint 28).
        let mut cri_drain: Option<node_runtime::CriCtl> = None;
        if bind.is_none() {
            if let Some(url) = server_url {
                let node = node_name.unwrap_or_else(kubelet::default_node_name);
                // The kubelet http client is plain-http only in v1 (Q21;
                // TLS lands with T1.3) — reject anything else up front so
                // https join URLs degrade, not fail. Both backends obey.
                if kubelet::http::HttpJson::parse_url(&url).is_err() {
                    tracing::warn!(
                        target: "init-pro",
                        role,
                        "kubelet needs an http:// --server URL in v1 (Q21); \
                         kubelet disabled"
                    );
                } else if builtin {
                    // T4.12 (Q49): the kubelet rides the built-in engine —
                    // no containerd, no staged crictl tree. The Node object
                    // reports engine://<version> so describe tells the truth.
                    // T4.14 (Q55): engine-level rootfs source (host dirs
                    // by default; the pod annotation can flip per pod).
                    let rootfs = kubelet::engine::RootfsMode::parse(&builtin_rootfs)
                        .unwrap_or_else(|e| {
                            tracing::warn!(target: "init-pro", role, error = %e,
                                "bad --builtin-rootfs value; defaulting to host (Q55)");
                            kubelet::engine::RootfsMode::Host
                        });
                    // T7.2 (Q61): the GPU plane's backend inputs — the
                    // builtin engine (probe already passed, hence `builtin`)
                    // under the resolved rootfs mode + the inventory source.
                    let mut kc = kubelet_config(&url, &node, &kubelet_addr, &cfg, &server_token)
                        .with_runtime_version(format!("engine://{}", common::version()));
                    kc.engine_builtin = true;
                    kc.builtin_rootfs = rootfs;
                    kc.gpu_inventory = gpu_inventory.clone();
                    match kubelet::engine::EngineBackend::new(
                        &cfg.data_dir,
                        &kc.sandbox_image,
                        &kc.node_ip,
                        &registry_addr,
                    )
                    .map(|b| b.with_image_quota(builtin_image_quota))
                    .map(|b| b.with_rootfs_mode(rootfs))
                    {
                        Ok(backend) => {
                            // Q49: adopt-or-clean leftover engine sockets.
                            backend.recover().await;
                            let handles =
                                kubelet::spawn(kc, Arc::new(backend), shutdown.clone());
                            tracing::info!(
                                target: "init-pro",
                                role,
                                node = %node,
                                server = %url,
                                "kubelet started (T4.12 built-in engine)"
                            );
                            kubelet_drain = Some(handles);
                        }
                        Err(e) => tracing::warn!(
                            target: "init-pro",
                            role,
                            "built-in engine backend unavailable: {e}; kubelet off"
                        ),
                    }
                } else {
                    let paths = node_runtime::AgentRuntimePaths::for_data_dir(&cfg.data_dir);
                    match node_runtime::CriCtl::for_paths(&paths) {
                        Some(cri) => {
                            let kc = kubelet_config(&url, &node, &kubelet_addr, &cfg, &server_token);
                            let handles = kubelet::spawn(
                                kc,
                                Arc::new(kubelet::CriCtlBackend::new(cri.clone())),
                                shutdown.clone(),
                            );
                            tracing::info!(
                                target: "init-pro",
                                role,
                                node = %node,
                                server = %url,
                                "kubelet started (T4.2)"
                            );
                            kubelet_drain = Some(handles);
                            cri_drain = Some(cri);
                        }
                        None => tracing::warn!(
                            target: "init-pro",
                            role,
                            "no staged crictl; kubelet off (T4.1 tree missing)"
                        ),
                    }
                }
                // T7.1 Phase B (Q37): node-side Agent execution — stage
                // (or discover) the opencoder runner binary, then run the
                // watch + execute loops. Needs ONLY the API surface (no
                // CRI), so it starts even when the kubelet stays off.
                // Degradations: no runner binary / bad data dir => bound
                // Agents report `Ready=False, reason=RunnerMissing` and
                // stay Pending; `agent_exec_addr == None` keeps the loop
                // off entirely (`--disable-agent-exec`). An EMPTY addr
                // keeps the loop on; only workspace mounting degrades to
                // the node-local fallback (Q37).
                if let Some(anyhome_addr) = &agent_exec_addr {
                    let runner_bin =
                        match node_runtime::opencoder::stage_runner(&cfg.data_dir) {
                            Ok(Some(path)) => Some(path),
                            Ok(None) => {
                                tracing::warn!(
                                    target: "init-pro",
                                    role,
                                    "no opencoder runner found; agents will report RunnerMissing (Q37)"
                                );
                                None
                            }
                            Err(e) => {
                                tracing::warn!(
                                    target: "init-pro",
                                    role,
                                    error = %e,
                                    "opencoder runner staging failed; agents will                                      report RunnerMissing (Q37)"
                                );
                                None
                            }
                        };
                    let mut ac = kubelet::AgentRunnerConfig::new(
                        url.clone(),
                        node.clone(),
                        cfg.data_dir.clone(),
                    );
                    ac.runner_bin = runner_bin;
                    // T7.7 D7 (Q63): hardened runner staging (fail-closed).
                    ac.hardened_runner_bin =
                        node_runtime::opencoder::stage_agent_runner(&cfg.data_dir)
                            .unwrap_or_else(|e| {
                                tracing::warn!(target: "init-pro", error = %e, "agent-runner");
                                None
                            });
                    // Empty `--anyhome-addr` = loop on, node-local
                    // workspace fallback (Q37); only a non-empty value is
                    // a mount address.
                    ac.anyhome_addr = (!anyhome_addr.trim().is_empty())
                        .then(|| anyhome_addr.clone());
                    // T7.7 (Q63): node-side sandbox config; absent keeps
                    // hardened Agents Pending (fail-closed).
                    ac.sandbox = kubelet::agent_sandbox::SandboxNodeConfig::from_env();
                    // T1.3a (Q47 P3): the Agent loops authenticate too.
                    ac.server_token = server_token.clone();
                    let handles = kubelet::spawn_agent_runner(ac, shutdown.clone());
                    let mut drain = kubelet_drain.take().unwrap_or_default();
                    drain.extend(handles);
                    kubelet_drain = Some(drain);
                }
                // T4.3 Scope A (Q34): embedded flanneld as the kube subnet
                // manager — same apiserver URL as the kubelet (plain HTTP,
                // no auth in v1). Independent of the kubelet's staged
                // crictl: flanneld only needs the API surface + netlink.
                // `--flannel-backend=none` keeps the agent flannel-free.
                let net = flannel.clone().unwrap_or_default();
                if builtin {
                    // Q52: builtin pods share the host netns — no CNI, no
                    // veth, no subnet.env; flanneld would be a no-op.
                    tracing::info!(
                        target: "init-pro",
                        role,
                        "builtin: pods share host netns, flannel skipped (Q52)"
                    );
                } else if crate::flannel::is_enabled(&net) {
                    match crate::flannel::spawn_flannel(
                        &cfg.data_dir,
                        &url,
                        &node,
                        &net,
                        server_token.as_deref(),
                        shutdown.clone(),
                    ) {
                        Ok(handle) => flannel_drain = Some(handle),
                        Err(e) => tracing::warn!(
                            target: "init-pro",
                            role,
                            "flannel disabled (net-conf unreadable): {e}"
                        ),
                    }
                } else {
                    tracing::info!(
                        target: "init-pro",
                        role,
                        "flannel disabled by --flannel-backend=none"
                    );
                }
                // T4.4 (Q64): node-side NetworkPolicy enforcement —
                // default-on for the standard (containerd + flannel)
                // data plane, opted out by `--disable-network-policy`.
                // Q52 exclusions: builtin nodes never police (their pods
                // share the host netns — skipped above), and no CNI means
                // no pod addresses to police.
                if netpol_enabled && !builtin && crate::flannel::is_enabled(&net) {
                    netpol_drain = crate::netpol_keeper::spawn_netpol_keeper(
                        &url,
                        &node,
                        server_token.as_deref(),
                        shutdown.clone(),
                    );
                    tracing::info!(
                        target: "init-pro",
                        role,
                        node = %node,
                        "network policy enforcement started (T4.4)"
                    );
                } else {
                    tracing::info!(
                        target: "init-pro",
                        role,
                        "network policy enforcement off (--disable-network-policy or no CNI)"
                    );
                }
                // T4.5 (Q65): the agent-dialed reverse relay tunnel —
                // default-on for joined nodes, opted out by
                // `--disable-proxy-tunnel`. It reuses the kubelet's own
                // advertised address (`node_ip:kubelet_port`, T4.2 Scope C)
                // as the local execution target.
                if tunnel_enabled {
                    if let Some((ip, port)) = &kubelet_addr {
                        tunnel_drain = crate::tunnel_dialer::spawn_tunnel_dialer(
                            &url,
                            &node,
                            ip,
                            *port,
                            server_token.as_deref(),
                            shutdown.clone(),
                        );
                        tracing::info!(
                            target: "init-pro",
                            role,
                            node = %node,
                            "reverse relay tunnel started (T4.5, Q65)"
                        );
                    }
                } else {
                    tracing::info!(
                        target: "init-pro",
                        role,
                        "reverse relay tunnel off (--disable-proxy-tunnel, T4.5)"
                    );
                }
            }
        }
        // T1.3a (Q47): the server plane spawns in
        // `runtime_server::spawn_server_plane` (split out of here for the
        // file-size limit) — one storage Arc (Q19), identical spawn order
        // and error paths; the `?` keeps the open_store failure fatal.
        let server_plane = match &bind {
            Some(b) if !b.disable_apiserver => {
                Some(crate::runtime_server::spawn_server_plane(role, &cfg, b, &shutdown).await?)
            }
            Some(_) => {
                tracing::info!(target: "init-pro", role, "apiserver disabled by flag");
                None
            }
            None => None,
        };
        // T1.2a/T3.1a (Q19): controller-manager + scheduler + apiserver
        // drain handles; Sprint 18/S4 (Q28): the Router NodePort service
        // plane — the kube-proxy-equivalent, on by default
        // (`--disable-kube-proxy` turns it off), drained after the agent
        // runtime, before the API surface. T-anyhome (Q36): embedded NFSv3
        // workspace plane (server-only), drained after flanneld; T6.3
        // (Q40): embedded image registry, drained right after anyhome.
        // B10 (long-run bug sweep): the controller manager is SIGNALED at
        // the very start of the drain (below) and only JOINED at the end.
        let (
            server_join,
            controllers_drain,
            scheduler_drain,
            service_plane_drain,
            anyhome_drain,
            registry_drain,
            compaction_drain,
        ) = match server_plane {
            Some(p) => (
                p.server_join,
                p.controllers,
                p.scheduler,
                p.service_plane,
                p.anyhome,
                p.registry,
                p.compaction,
            ),
            None => (None, None, None, None, None, None, None),
        };

        tracing::info!(
            target: "init-pro",
            role,
            version = common::version(),
            data_dir = %cfg.data_dir.display(),
            "init-pro {role} ready",
        );

        shutdown.cancelled().await;
        tracing::info!(target: "init-pro", role, "init-pro {role}: draining");
        // B10: signal the controller manager BEFORE any data plane drains
        // -- no reconciler may start a new registry push, image pull or
        // child create against a listener that is about to close, and the
        // in-flight passes get the whole node-plane drain to finish. Only
        // the signal moves: the task join stays at the end of the sequence
        // (after the API surface) so a wedged reconciler cannot block
        // teardown. A push still cut by the registry's bounded drain
        // surfaces as a transport error, which the Build retries on the
        // next pass (status.pushRetries, B10).
        if let Some((_, cm_stop)) = controllers_drain.as_ref() {
            cm_stop.trigger();
        }
        // T4.2: drain the kubelet loops BEFORE the runtime — this join now
        // WAITS for the sync task's shutdown teardown (T4.1 follow-up,
        // Sprint 28: stopp+rmp of every sandbox, bounded), so CRI work is
        // finished, not merely stopped, before containerd goes away.
        if let Some(handles) = kubelet_drain {
            for mut h in handles {
                // Batch D (C6): bounded like the flannel block below.
                if tokio::time::timeout(KUBELET_DRAIN_TIMEOUT, &mut h).await.is_err() {
                    h.abort();
                    tracing::warn!(
                        target: "init-pro",
                        role,
                        "kubelet drain timed out; loop task aborted"
                    );
                }
            }
            tracing::info!(target: "init-pro", role, "kubelet drained");
            // T4.1 follow-up (Sprint 28): confirm CRI reports no sandboxes
            // before the daemon kill — an empty CRI list means no shim of
            // this run can outlive containerd.
            if let Some(cri) = cri_drain.as_ref() {
                if wait_sandboxes_gone(cri, SANDBOX_GONE_TIMEOUT).await {
                    tracing::info!(target: "init-pro", role, "CRI drained: no sandboxes remain");
                } else {
                    tracing::warn!(
                        target: "init-pro",
                        role,
                        "CRI not confirmed empty; stopping containerd anyway"
                    );
                }
            }
        }
        // T4.1: only now fire the supervisor's token (kill + bounded wait),
        // mirroring k3s stopping the runtime before the control plane.
        if let Some(h) = runtime_drain {
            runtime_shutdown.trigger();
            let _ = h.await;
            tracing::info!(target: "init-pro", role, "containerd runtime drained (supervisor exited)");
        }
        // T4.3 (Q34): flanneld after the runtime — the kubelet's sandbox
        // teardown (CNI DEL) ran while it was still up; its task exits on
        // the shared token, this join only confirms.
        if let Some(mut h) = flannel_drain {
            match tokio::time::timeout(FLANNEL_DRAIN_TIMEOUT, &mut h).await {
                Ok(_) => tracing::info!(target: "init-pro", role, "flannel drained"),
                Err(_) => {
                    h.abort();
                    tracing::warn!(
                        target: "init-pro",
                        role,
                        "flannel drain timed out; daemon task aborted"
                    );
                }
            }
        }
        // T4.4 (Q64): the netpol keeper right after flanneld — its exit
        // is the zero-residue teardown (jump + every INIT-PRO-NPOL-*
        // chain gone, NP4); waiting here keeps shutdown residue-free,
        // bounded so a wedged keeper cannot hang agent exit (S53 /
        // review P1-⑤).
        if let Some(mut h) = netpol_drain {
            match tokio::time::timeout(NETPOL_DRAIN_TIMEOUT, &mut h).await {
                Ok(_) => tracing::info!(target: "init-pro", role, "network policy drained"),
                Err(_) => {
                    h.abort();
                    tracing::warn!(
                        target: "init-pro",
                        role,
                        "network policy drain timed out; keeper aborted"
                    );
                }
            }
        }
        // T4.5 (Q65): the tunnel dialer right after the netpol keeper —
        // confirm its long-poll task exited (no lingering outbound poll).
        if let Some(h) = tunnel_drain {
            let _ = h.await;
            tracing::info!(target: "init-pro", role, "tunnel dialer drained");
        }
        // T-anyhome (Q36): drain the NFS data plane here too (data plane,
        // before the API surface) so no workspace IO survives the drain.
        if let Some(mut h) = anyhome_drain {
            match tokio::time::timeout(ANYHOME_DRAIN_TIMEOUT, &mut h).await {
                Ok(_) => tracing::info!(target: "init-pro", role, "anyhome drained"),
                Err(_) => {
                    h.abort();
                    tracing::warn!(
                        target: "init-pro",
                        role,
                        "anyhome drain timed out; NFS plane aborted"
                    );
                }
            }
        }
        // T6.3 (Q40): drain the image registry alongside the NFS data plane
        // (data plane, before the API surface) so no blob IO survives it.
        if let Some(mut h) = registry_drain {
            match tokio::time::timeout(REGISTRY_DRAIN_TIMEOUT, &mut h).await {
                Ok(_) => tracing::info!(target: "init-pro", role, "registry drained"),
                Err(_) => {
                    h.abort();
                    tracing::warn!(
                        target: "init-pro",
                        role,
                        "registry drain timed out; listener aborted"
                    );
                }
            }
        }
        // Sprint 18 / S4 (Q28): drain the Router service plane AFTER the
        // agent runtime and BEFORE the API surface — nodePort listeners stop
        // serving, then the ClusterIP VIP listeners (T4.3 Scope C, Q44 —
        // their lo aliases are removed with the plane), then the reflectors
        // join on the shared shutdown token.
        if let Some((plane, clusterip, reflectors)) = service_plane_drain {
            plane.drain().await;
            clusterip.drain().await;
            for h in reflectors {
                let _ = h.await;
            }
            tracing::info!(target: "init-pro", role, "service plane drained");
        }
        // B11: the API surface's own outcome rides out of the join -- a
        // bind failure, a died serve loop or a panicked task exits
        // non-zero AFTER the full drain (a supervisor restarts on it); a
        // graceful shutdown answers Ok.
        let api_exit = match server_join {
            Some(jh) => match jh.await {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e),
                Err(join) => Some(std::io::Error::other(join.to_string())),
            },
            None => None,
        };
        // T3.1a: after the API surface has drained, join the controller
        // tasks (leadership elector + supervisor; the supervisor aborts the
        // worker/informer set). The stop itself fired before the data-plane
        // drains (B10). `let _ =` tolerates aborted tasks.
        if let Some((cm_handles, _)) = controllers_drain {
            for h in cm_handles {
                let _ = h.await;
            }
            tracing::info!(target: "init-pro", role, "controller manager drained");
        }
        // T3.2: drain the scheduler after the controllers (it only observes
        // pods/nodes; controllers may still bind ReplicaSet-owned pods).
        if let Some((sched_handles, sched_stop)) = scheduler_drain {
            sched_stop.trigger();
            for h in sched_handles {
                let _ = h.await;
            }
            tracing::info!(target: "init-pro", role, "scheduler drained");
        }
        // B3: the compaction timer shares the shutdown token; join it after
        // the controllers so no pass races the store teardown.
        if let Some(h) = compaction_drain {
            let _ = h.await;
        }
        tracing::info!(target: "init-pro", role, "init-pro {role}: draining complete");
        match api_exit {
            Some(e) => Err(e),
            None => Ok(()),
        }
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("init-pro {role}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Build the kubelet config shared by both runtimes (T4.2/T4.12): the
/// CLI kubelet address overrides the bind/advertise ip:port, else the
/// 127.0.0.1:10250 defaults (non-CLI callers / tests only).
fn kubelet_config(
    url: &str,
    node: &str,
    kubelet_addr: &Option<(String, u16)>,
    cfg: &Config,
    token: &Option<String>,
) -> kubelet::KubeletConfig {
    // T1.3a (Q47 P3): the bearer token rides the config; `None` keeps
    // the v1 default (anonymous plain HTTP).
    match kubelet_addr {
        Some((ip, port)) => kubelet::KubeletConfig::new(url, node, cfg.data_dir.clone())
            .with_node_ip(ip.clone())
            .with_kubelet_port(*port)
            .with_server_token(token.clone()),
        None => kubelet::KubeletConfig::new(url, node, cfg.data_dir.clone())
            .with_server_token(token.clone()),
    }
}
