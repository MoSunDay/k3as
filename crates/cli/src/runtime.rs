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
use std::process::ExitCode;
use std::sync::Arc;

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
    /// `--kube-scheduler-arg KEY=VALUE` passthrough (`config=<path>` wires
    /// HTTP extenders, T3.2/Q3).
    pub scheduler_args: Vec<String>,
}

pub fn run_server(cfg: Config, bind: ServerBind) -> ExitCode {
    run_supervised("server", cfg, Some(bind), None, None)
}

pub fn run_agent(cfg: Config, server_url: Option<String>, node_name: Option<String>) -> ExitCode {
    run_supervised("agent", cfg, None, server_url, node_name)
}

/// `server` serves discovery when given a [`ServerBind`]; `agent` supervises
/// the bundled containerd runtime (T4.1) and, with `server_url` + a staged
/// crictl, the kubelet loops (T4.2 Scope A) under `node_name`.
fn run_supervised(
    role: &'static str,
    cfg: Config,
    bind: Option<ServerBind>,
    server_url: Option<String>,
    node_name: Option<String>,
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
        // T4.1 (Q25): the agent supervises the bundled containerd (stage ->
        // render config -> supervise with backoff). The server keeps the
        // runtime off by default (single-node UX arrives with T4.5/e2e).
        let mut runtime_drain: Option<tokio::task::JoinHandle<()>> = None;
        if bind.is_none() {
            match node_runtime::start_agent_runtime(&cfg.data_dir, shutdown.clone()) {
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
                            let kc = kubelet::KubeletConfig::new(
                                url.clone(),
                                node.clone(),
                                cfg.data_dir.clone(),
                            );
                            let handles = kubelet::spawn(
                                kc,
                                Arc::new(kubelet::CriCtlBackend::new(cri)),
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
                        }
                    }
                    None => tracing::warn!(
                        target: "init-pro",
                        role,
                        "no staged crictl; kubelet off (T4.1 tree missing)"
                    ),
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
                // (ADR Q17). Real etcd-gRPC (T2.2) / SQLite-KINE (T2.3) slot in
                // as alternative StorageBackend impls behind --datastore-endpoint.
                let store: Arc<dyn storage::StorageBackend> =
                    Arc::new(storage::EmbeddedStorage::new());
                // T3.1a: spawn the controller manager against the same
                // storage Arc (Q19); leader-elected via a Lease + CAS (Q18).
                // Only the full apiserver path runs controllers —
                // `--disable-apiserver` keeps single-process server semantics.
                if !b.disable_controllers {
                    let cm_stop = controllers::Stop::new();
                    let cm_handles =
                        controllers::ControllerManager::spawn(store.clone(), cm_stop.clone());
                    tracing::info!(target: "init-pro", role, "controller manager started (leader-elected via Lease, Q18)");
                    controllers_drain = Some((cm_handles, cm_stop));
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
        // T4.2: drain the kubelet loops BEFORE the runtime — the kubelet
        // must stop driving CRI before containerd dies (the shared Shutdown
        // token has already stopped both; this only orders the joins).
        if let Some(handles) = kubelet_drain {
            for h in handles {
                let _ = h.await;
            }
            tracing::info!(target: "init-pro", role, "kubelet drained");
        }
        // T4.1: then drain the containerd supervisor (kill + bounded wait),
        // mirroring k3s stopping the runtime before the control plane.
        if let Some(h) = runtime_drain {
            let _ = h.await;
            tracing::info!(target: "init-pro", role, "containerd runtime drained (supervisor exited)");
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
