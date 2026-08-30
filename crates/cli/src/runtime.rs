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

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use ::runtime as node_runtime;
use common::embed::EmbeddedManifest;
use infra::{Config, Shutdown};

/// Server-only wiring: where to bind the discovery API + which in-process
/// control-plane components are enabled (Q19).
pub struct ServerBind {
    pub addr: SocketAddr,
    pub disable_apiserver: bool,
    pub disable_controllers: bool,
    pub disable_scheduler: bool,
    /// `--disable-kube-proxy` turns the NodePort service plane OFF
    /// (Sprint 18 / **S4**, Q28). By default the built-in Router carries
    /// Service traffic — one reverse-proxy listener per allocated nodePort,
    /// peers resolved live from Endpoints (the kube-proxy-equivalent).
    pub disable_kube_proxy: bool,
    /// `--kube-scheduler-arg KEY=VALUE` passthrough (`config=<path>` wires
    /// HTTP extenders, T3.2/Q3).
    pub scheduler_args: Vec<String>,
    /// DSN selecting the storage backend (**T2.3**, decisions **Q29/Q33**):
    /// `None`/empty = the embedded default (**Q17**); `rdb://<path>` = the
    /// RocksDB `RdbStorage` backend; other schemes are fatal (etcd-gRPC is
    /// deferred to T3.4).
    pub datastore_endpoint: Option<String>,
    /// `--cluster-cidr`: the node-cidr controller carves Node `/24`
    /// podCIDRs from it (T4.3 Scope A, Q34).
    pub cluster_cidr: String,
    /// Embedded anyhome NFSv3 workspace plane (T-anyhome, Q36):
    /// `Some(addr)` = serve on that address; `None` = disabled
    /// (`--disable-anyhome`).
    pub anyhome: Option<String>,
}

pub fn run_server(cfg: Config, bind: ServerBind) -> ExitCode {
    run_supervised("server", cfg, Some(bind), None, None, None, None)
}

pub fn run_agent(
    cfg: Config,
    server_url: Option<String>,
    node_name: Option<String>,
    node_ip: String,
    kubelet_port: u16,
    flannel: crate::flannel::FlannelNet,
) -> ExitCode {
    run_supervised(
        "agent",
        cfg,
        None,
        server_url,
        node_name,
        Some((node_ip, kubelet_port)),
        Some(flannel),
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

fn run_supervised(
    role: &'static str,
    cfg: Config,
    bind: Option<ServerBind>,
    server_url: Option<String>,
    node_name: Option<String>,
    kubelet_addr: Option<(String, u16)>,
    flannel: Option<crate::flannel::FlannelNet>,
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
        let mut controllers_drain: Option<(
            Vec<tokio::task::JoinHandle<()>>,
            controllers::Stop,
        )> = None;
        let mut scheduler_drain: Option<(
            Vec<tokio::task::JoinHandle<()>>,
            controllers::Stop,
        )> = None;
        // Sprint 18 / S4 (Q28): the Router NodePort service plane — the
        // kube-proxy-equivalent, on by default; `--disable-kube-proxy` turns
        // it off. Drained after the agent runtime, before the API surface.
        let mut service_plane_drain: Option<(
            router::NodePortPlane,
            Vec<tokio::task::JoinHandle<()>>,
        )> = None;
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
        // T-anyhome (Q36): embedded anyhome NFSv3 workspace plane
        // (server-only). Drained after flanneld, before the API surface.
        let mut anyhome_drain: Option<tokio::task::JoinHandle<()>> = None;
        if bind.is_none() {
            match node_runtime::start_agent_runtime(&cfg.data_dir, runtime_shutdown.clone()) {
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
                let paths = node_runtime::AgentRuntimePaths::for_data_dir(&cfg.data_dir);
                match node_runtime::CriCtl::for_paths(&paths) {
                    Some(cri) => {
                        // The kubelet http client is plain-http only in v1
                        // (Q21; TLS lands with T1.3) — reject anything else
                        // up front so https join URLs degrade, not fail.
                        if kubelet::http::HttpJson::parse_url(&url).is_err() {
                            tracing::warn!(
                                target: "init-pro",
                                role,
                                "kubelet needs an http:// --server URL in v1 (Q21); \
                                 kubelet disabled"
                            );
                        } else {
                            let kc = match &kubelet_addr {
                                Some((ip, port)) => kubelet::KubeletConfig::new(
                                    url.clone(),
                                    node.clone(),
                                    cfg.data_dir.clone(),
                                )
                                .with_node_ip(ip.clone())
                                .with_kubelet_port(*port),
                                None => {
                                    // Defaults (127.0.0.1:10250) — used by
                                    // non-CLI callers / tests only.
                                    kubelet::KubeletConfig::new(
                                        url.clone(),
                                        node.clone(),
                                        cfg.data_dir.clone(),
                                    )
                                }
                            };
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
                    }
                    None => tracing::warn!(
                        target: "init-pro",
                        role,
                        "no staged crictl; kubelet off (T4.1 tree missing)"
                    ),
                }
                // T4.3 Scope A (Q34): embedded flanneld as the kube subnet
                // manager — same apiserver URL as the kubelet (plain HTTP,
                // no auth in v1). Independent of the kubelet's staged
                // crictl: flanneld only needs the API surface + netlink.
                // `--flannel-backend=none` keeps the agent flannel-free.
                let net = flannel.clone().unwrap_or_default();
                if crate::flannel::is_enabled(&net) {
                    match crate::flannel::spawn_flannel(
                        &cfg.data_dir,
                        &url,
                        &node,
                        &net,
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
            }
        }
        let server_join = match &bind {
            Some(b) if !b.disable_apiserver => {
                let reg = crate::discovery::served_schema();
                let advertised = b.addr.to_string();
                let summary = crate::discovery::served_groups_summary(&reg, &advertised);
                tracing::info!(target: "init-pro", role, "T1.1 {summary}");
                // T1.2b: default backend = the zero-dependency embedded store
                // (Q17); `rdb://` opts into RdbStorage (T2.3, Q33) —
                // etcd-gRPC remains deferred to T3.4.
                let store = match open_store(b.datastore_endpoint.as_deref()).await {
                    Ok(store) => store,
                    Err(e) => {
                        tracing::error!(target: "init-pro", role, "{e}");
                        return Err(e);
                    }
                };
                // T3.1a: spawn the controller manager against the same
                // storage Arc (Q19); leader-elected via a Lease + CAS (Q18).
                // Only the full apiserver path runs controllers —
                // `--disable-apiserver` keeps single-process server semantics.
                if !b.disable_controllers {
                    let cm_stop = controllers::Stop::new();
                    let cm_handles = controllers::ControllerManager::spawn_with(
                        store.clone(),
                        cm_stop.clone(),
                        controllers::ManagerConfig {
                            // T4.3 Scope A (Q34): allocate Node podCIDRs
                            // from --cluster-cidr.
                            cluster_cidr: Some(b.cluster_cidr.clone()),
                            // T-anyhome (Q36): Workflow step pods + Agent
                            // children get the /workspace contract from the
                            // anyhome endpoint (None = --disable-anyhome).
                            workspace_nfs: b.anyhome.clone(),
                        },
                    );
                    tracing::info!(target: "init-pro", role, "controller manager started (leader-elected via Lease, Q18)");
                    controllers_drain = Some((cm_handles, cm_stop));
                }
                // T-anyhome (Q36): the NFSv3 workspace plane rides the SAME
                // storage Arc (Q19) right after the controllers spawn.
                // Degraded-but-booting (flannel precedent): a bind/fs failure
                // is a warn, the API surface still comes up.
                if let Some(addr) = &b.anyhome {
                    match crate::anyhome::spawn_anyhome(
                        &cfg.data_dir,
                        addr,
                        store.clone(),
                        shutdown.clone(),
                    ) {
                        Ok(handle) => anyhome_drain = Some(handle),
                        Err(e) => tracing::warn!(
                            target: "init-pro",
                            role,
                            "anyhome disabled (fs root unreadable): {e}"
                        ),
                    }
                }
                // T3.2 (Q23): the scheduler reuses the controllers framework
                // (informer/workqueue/leader-election) over the same storage
                // Arc — one more in-process component, no loopback HTTP.
                if !b.disable_scheduler {
                    let mut sched_cfg = scheduler::SchedulerConfig::new();
                    for arg in &b.scheduler_args {
                        match arg.split_once('=') {
                            Some(("config", value)) => {
                                match scheduler::SchedulerConfig::load_extenders(
                                    std::path::Path::new(value),
                                ) {
                                    Ok(loaded) => {
                                        let count = loaded.extenders.len();
                                        sched_cfg.extenders = loaded.extenders;
                                        tracing::info!(target: "init-pro", role,
                                            count,
                                            "scheduler extenders loaded from {value}");
                                    }
                                    Err(e) => {
                                        return Err(std::io::Error::other(format!(
                                            "--kube-scheduler-arg config={value}: {e}"
                                        )));
                                    }
                                }
                            }
                            _ => tracing::warn!(target: "init-pro", role,
                                arg = %arg,
                                "--kube-scheduler-arg accepted but not implemented for this key; no-op"),
                        }
                    }
                    let sched_stop = controllers::Stop::new();
                    let sched_handles = scheduler::SchedulerManager::spawn(
                        store.clone(),
                        sched_cfg,
                        sched_stop.clone(),
                    );
                    tracing::info!(target: "init-pro", role, "scheduler started (leader-elected via Lease, Q18)");
                    scheduler_drain = Some((sched_handles, sched_stop));
                }
                // Sprint 18 / S4 (Q28): the NodePort service plane runs by
                // default — LIST->WATCH reflectors over the SAME storage Arc
                // plus one reverse-proxy listener per allocated nodePort
                // (the kube-proxy-equivalent; `--disable-kube-proxy` is the
                // k3s-parity escape hatch that turns it off).
                if !b.disable_kube_proxy {
                    let (resolver, reflectors) =
                        router::supervise(store.clone(), shutdown.clone());
                    service_plane_drain = Some((
                        router::spawn_nodeport_plane(
                            resolver,
                            router::NodePortConfig::default(),
                            shutdown.clone(),
                        ),
                        reflectors,
                    ));
                    tracing::info!(target: "init-pro", role,
                        "service plane started: Router carries nodePort traffic (Sprint 18 / S4, Q28)");
                }
                let server_shutdown = shutdown.clone();
                let addr = b.addr;
                Some(tokio::spawn(async move {
                    if let Err(e) = apiserver::serve(reg, store, addr, advertised, async move {
                        server_shutdown.cancelled().await;
                    })
                    .await
                    {
                        tracing::error!(target: "init-pro", role, "apiserver exited: {e}");
                    }
                }))
            }
            Some(_) => {
                tracing::info!(target: "init-pro", role, "apiserver disabled by flag");
                None
            }
            None => None,
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
        // T4.2: drain the kubelet loops BEFORE the runtime — this join now
        // WAITS for the sync task's shutdown teardown (T4.1 follow-up,
        // Sprint 28: stopp+rmp of every sandbox, bounded), so CRI work is
        // finished, not merely stopped, before containerd goes away.
        if let Some(handles) = kubelet_drain {
            for h in handles {
                let _ = h.await;
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
        if let Some(h) = flannel_drain {
            let _ = h.await;
            tracing::info!(target: "init-pro", role, "flannel drained");
        }
        // T-anyhome (Q36): drain the NFS data plane here too (data plane,
        // before the API surface) so no workspace IO survives the drain.
        if let Some(h) = anyhome_drain {
            let _ = h.await;
            tracing::info!(target: "init-pro", role, "anyhome drained");
        }
        // Sprint 18 / S4 (Q28): drain the Router service plane AFTER the
        // agent runtime and BEFORE the API surface — nodePort listeners stop
        // serving, then the reflectors join on the shared shutdown token.
        if let Some((plane, reflectors)) = service_plane_drain {
            plane.drain().await;
            for h in reflectors {
                let _ = h.await;
            }
            tracing::info!(target: "init-pro", role, "service plane drained");
        }
        if let Some(jh) = server_join {
            let _ = jh.await;
        }
        // T3.1a: after the API surface has drained, stop the controllers and
        // join their tasks (leadership elector + supervisor; the supervisor
        // aborts the worker/informer set). `let _ =` tolerates aborted tasks.
        if let Some((cm_handles, cm_stop)) = controllers_drain {
            cm_stop.trigger();
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
        tracing::info!(target: "init-pro", role, "init-pro {role}: draining complete");
        Ok::<(), std::io::Error>(())
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("init-pro {role}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `--datastore-endpoint` classification (**T2.3**, decision **Q29**): the
/// pure DSN-to-backend selector [`open_store`] switches on.
#[derive(Debug, PartialEq, Eq)]
enum Dsn {
    /// Unset or empty: the zero-dependency embedded default (**Q17**).
    Embedded,
    /// `rdb://<path>`: the RocksDB (rdb `store` engine) backend
    /// (`RdbStorage`, **Q33**).
    Rdb(PathBuf),
    /// Anything else (including bare `sqlite://`): fatal, unsupported.
    Unsupported(String),
}

/// Classify a `--datastore-endpoint` DSN (**T2.3/Q29**). Pure: no I/O, no
/// side effects — the openable cases stay distinguishable for tests.
fn classify_dsn(dsn: Option<&str>) -> Dsn {
    match dsn {
        None | Some("") => Dsn::Embedded,
        Some(s) if s.starts_with("rdb://") => match &s["rdb://".len()..] {
            "" => Dsn::Unsupported(s.to_string()),
            path => Dsn::Rdb(PathBuf::from(path)),
        },
        Some(s) => Dsn::Unsupported(s.to_string()),
    }
}

/// Open the storage backend selected by `--datastore-endpoint` (**T2.3**,
/// decision **Q29**). Unset/empty keeps the embedded default (**Q17**) —
/// rdb is an explicit opt-in, never a silent behavior change;
/// `rdb://<path>` opens the RocksDB backend (**Q33**); other schemes are
/// fatal (etcd-gRPC is deferred to T3.4).
async fn open_store(dsn: Option<&str>) -> std::io::Result<Arc<dyn storage::StorageBackend>> {
    match classify_dsn(dsn) {
        Dsn::Embedded => Ok(Arc::new(storage::EmbeddedStorage::new())),
        Dsn::Rdb(path) => {
            let store = storage::RdbStorage::open(&path)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            tracing::info!(target: "init-pro", path = %path.display(),
                "rdb datastore (rocksdb) — T2.3/Q33");
            Ok(Arc::new(store))
        }
        Dsn::Unsupported(s) => {
            let msg = if s == "rdb://" {
                "--datastore-endpoint rdb:// requires a file path".to_string()
            } else {
                format!(
                    "unsupported --datastore-endpoint scheme (got {s:?}); \
                     supported: rdb://<path> (T2.3); empty = embedded default"
                )
            };
            Err(std::io::Error::other(msg))
        }
    }
}

/// `stage --dry-run` lists the embedded manifest + hashes without writing.
/// `stage` (no dry-run) performs the live atomic staging (B5).
pub fn run_stage(cfg: Config, dry_run: bool, manifest: &EmbeddedManifest) -> ExitCode {
    if dry_run {
        print_dry_run(&cfg, manifest);
        return ExitCode::SUCCESS;
    }

    match crate::stage::stage(&cfg, manifest.assets, manifest.sha256_sums) {
        Ok(result) => {
            if result.staged {
                println!(
                    "init-pro stage: staged {} asset(s) -> data/{}",
                    manifest.assets.len(),
                    result.hash
                );
            } else {
                println!(
                    "init-pro stage: up-to-date (data/current -> {})",
                    result.hash
                );
            }
            println!("data/current: {}", result.current.display());
            for entry in &result.path_entries {
                println!("PATH+: {}", entry.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("init-pro stage: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Print the dry-run manifest listing.
fn print_dry_run(cfg: &Config, manifest: &EmbeddedManifest) {
    let assets = manifest.assets;
    println!("init-pro stage --dry-run");
    println!("data-dir: {}", cfg.data_dir.display());
    println!(
        "manifest ({} embedded asset{}):",
        assets.len(),
        if assets.len() == 1 { "" } else { "s" }
    );
    if assets.is_empty() {
        println!("  (none — build with INIT_PRO_EMBED=1 to bake vendor artifacts)");
    } else {
        let total = assets.iter().map(|a| a.size).sum::<u64>();
        for a in assets {
            println!("  {:<28} {} bytes  sha256={}", a.path, a.size, a.sha256);
        }
        println!("total uncompressed: {} bytes", total);
    }
    let sums_lines = manifest.sha256_sums.lines().count();
    let links_lines = manifest.data_links.lines().count();
    println!(".sha256sums: {} entries", sums_lines);
    println!(".links: {} entries", links_lines);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T2.3/Q33: unset or empty DSN must keep the embedded default (Q17) -
    /// rdb is explicit opt-in only.
    #[test]
    fn classify_dsn_empty_is_embedded() {
        assert_eq!(classify_dsn(None), Dsn::Embedded);
        assert_eq!(classify_dsn(Some("")), Dsn::Embedded);
    }

    #[test]
    fn classify_dsn_rdb_paths() {
        assert_eq!(
            classify_dsn(Some("rdb:///tmp/a.rdb")),
            Dsn::Rdb(PathBuf::from("/tmp/a.rdb"))
        );
        assert_eq!(
            classify_dsn(Some("rdb://rel.rdb")),
            Dsn::Rdb(PathBuf::from("rel.rdb"))
        );
    }

    #[test]
    fn classify_dsn_rejects_bare_rdb_and_foreign_schemes() {
        assert_eq!(
            classify_dsn(Some("rdb://")),
            Dsn::Unsupported("rdb://".to_string())
        );
        assert_eq!(
            classify_dsn(Some("sqlite:///tmp/a.db")),
            Dsn::Unsupported("sqlite:///tmp/a.db".to_string())
        );
        assert_eq!(
            classify_dsn(Some("mysql://x")),
            Dsn::Unsupported("mysql://x".to_string())
        );
        assert_eq!(
            classify_dsn(Some("postgres://y")),
            Dsn::Unsupported("postgres://y".to_string())
        );
    }
}
