//! Server-plane wiring, split out of `runtime.rs` (file-size limit, the
//! `storage_dsn.rs` precedent): the server-subcommand bind structs + entry,
//! plus the in-process server-plane spawn block `run_supervised` drives
//! (**T1.3a** pre-split, Sprint 50, decision **Q47**). Pure mechanical move
//! — behavior, flags, signatures, and the drain order are unchanged.
//!
//! - [`ServerBind`]/[`RegistryBind`] — server-only wiring (re-exported from
//!   `runtime` so `runtime::ServerBind` paths keep working).
//! - [`run_server`] — the `server` subcommand entry.
//! - [`spawn_server_plane`] — T1.2a/T1.2b apiserver + T3.1a controllers
//!   (Q19) + T3.2 scheduler (Q23) + Sprint 18/S4 Router service plane (Q28)
//!   + anyhome (Q36) + registry (Q40), all against one storage Arc.

use std::net::SocketAddr;
use std::process::ExitCode;

use infra::{Config, Shutdown};

use crate::runtime::run_supervised;
use crate::storage_dsn::open_store;

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
    /// `--service-cidr`: the apiserver allocates Service `spec.clusterIP`s
    /// from it and the Router VIP plane aliases them onto `lo` (T4.3
    /// Scope C, Q44). Must parse as an IPv4 CIDR or startup is fatal.
    pub service_cidr: String,
    /// Embedded anyhome NFSv3 workspace plane (T-anyhome, Q36):
    /// `Some(addr)` = serve on that address; `None` = disabled
    /// (`--disable-anyhome`).
    pub anyhome: Option<String>,
    /// `--disable servicelb` (T4.3 Scope B / T5.6, Q41): skips the
    /// ServiceLB announce controller and its worker (external LB then owns
    /// `LoadBalancer`-type Services).
    pub disable_servicelb: bool,
    /// Embedded image registry (T6.3, Q40): `Some` = serve the Docker
    /// Registry HTTP API v2; `None` = disabled (`--disable-registry`).
    pub registry: Option<RegistryBind>,
    /// Static-token authn CSV (T1.3a, **Q47**): `Some` arms the
    /// authn + RBAC authz middleware pair and registers the
    /// `rbac.authorization.k8s.io/v1` group + ServiceAccount (conditional
    /// schema — the no-auth default stays byte-identical, golden G02/G03).
    pub token_file: Option<std::path::PathBuf>,
    /// Serving-cert SAN entries (T1.3a, **Q47**): non-empty opts the API
    /// listener into TLS (self-signed CA + serving cert minted at startup).
    pub tls_san: Vec<String>,
    /// Embedded CI/CD pipeline controller plane (**T8.3**, **Q56**):
    /// `Some` = the controller manager runs the `ServicePipeline`/`Build`
    /// workers (docker build -> registry push -> Deployment image patch);
    /// `None` = `--disable-pipeline` (no workers, no enqueues).
    pub pipeline: Option<controllers::PipelineConfig>,
}

/// Embedded image-registry wiring (T6.3, Q40): the listen address plus the
/// retention/keep-forever policy the push path enforces.
#[derive(Clone, Debug)]
pub struct RegistryBind {
    pub addr: String,
    pub cfg: registry::RegistryConfig,
}

pub fn run_server(cfg: Config, bind: ServerBind) -> ExitCode {
    // Servers never run the node runtime (the `bind.is_none()` blocks are
    // agent-only), so the engine choice and registry addr are inert here;
    // pass containerd + the default registry to keep the signature honest
    // (T4.12/Q51, T4.13; Q55's default rootfs mode is host anyway).
    run_supervised(
        "server",
        cfg,
        Some(bind),
        None,
        None,
        None,
        None,
        None,
        "containerd".to_string(),
        registry::DEFAULT_ADDR.to_string(),
        0,
        "host".to_string(),
        // T7.2 (Q61): servers never run the node runtime — auto inventory.
        kubelet::GpuInventorySource::Auto,
        // T1.3a (Q47 P3): servers run no node loops — no bearer token.
        None,
        // T4.4 (Q64): servers run no node data plane — no enforcement.
        false,
        // T4.5 (Q65): servers never dial tunnels — they ARE the relay
        // endpoint; node-side dialing is agent-only.
        false,
    )
}

/// The drain handles [`spawn_server_plane`] returns (**T1.3a**): one field
/// per local `run_supervised` used to assign in place — the drain sequence
/// there consumes them in the exact same order.
pub(crate) struct ServerPlaneDrains {
    pub(crate) server_join: Option<tokio::task::JoinHandle<()>>,
    pub(crate) controllers: Option<(Vec<tokio::task::JoinHandle<()>>, controllers::Stop)>,
    pub(crate) scheduler: Option<(Vec<tokio::task::JoinHandle<()>>, controllers::Stop)>,
    pub(crate) service_plane: Option<(
        router::NodePortPlane,
        router::ClusterIpPlane,
        Vec<tokio::task::JoinHandle<()>>,
    )>,
    pub(crate) anyhome: Option<tokio::task::JoinHandle<()>>,
    pub(crate) registry: Option<tokio::task::JoinHandle<()>>,
}

/// Spawn the server plane (**T1.3a** pre-split, decision **Q47**): discovery
/// apiserver over the selected store (T1.2a/T1.2b), the in-process
/// controller manager (T3.1a, Q19), scheduler (T3.2, Q23), Router NodePort
/// service plane (Sprint 18/S4, Q28), anyhome (Q36), and the image registry
/// (T6.3, Q40) — all sharing one storage Arc. Moved verbatim out of
/// `run_supervised`; the caller owns the drain order.
pub(crate) async fn spawn_server_plane(
    role: &'static str,
    cfg: &Config,
    b: &ServerBind,
    shutdown: &Shutdown,
) -> std::io::Result<ServerPlaneDrains> {
    let mut drains = ServerPlaneDrains {
        server_join: None,
        controllers: None,
        scheduler: None,
        service_plane: None,
        anyhome: None,
        registry: None,
    };
    let reg = crate::discovery::served_schema(b.token_file.is_some());
    // T4.3 Scope C (Q44): parse --service-cidr once, up front — a
    // malformed CIDR is fatal BEFORE any listener binds (VIP plane or
    // apiserver); k3s parity is a hard refusal, not a silent default.
    let service_cidr = match apiserver::ServiceCidr::parse(&b.service_cidr) {
        Some(c) => c,
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("--service-cidr {}: malformed IPv4 CIDR", b.service_cidr),
            ));
        }
    };
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
                // T4.3 Scope B (Q41): --disable servicelb.
                disable_servicelb: b.disable_servicelb,
                // T8.3 (Q56): --disable-pipeline leaves the plane
                // fully off (no workers, no enqueues).
                pipeline: b.pipeline.clone(),
            },
        );
        tracing::info!(target: "init-pro", role, "controller manager started (leader-elected via Lease, Q18)");
        drains.controllers = Some((cm_handles, cm_stop));
    }
    // T-anyhome (Q36): the NFSv3 workspace plane rides the SAME
    // storage Arc (Q19) right after the controllers spawn.
    // Degraded-but-booting (flannel precedent): a bind/fs failure
    // is a warn, the API surface still comes up.
    if let Some(addr) = &b.anyhome {
        match crate::anyhome::spawn_anyhome(&cfg.data_dir, addr, store.clone(), shutdown.clone()) {
            Ok(handle) => drains.anyhome = Some(handle),
            Err(e) => tracing::warn!(
                target: "init-pro",
                role,
                "anyhome disabled (fs root unreadable): {e}"
            ),
        }
    }
    // T6.3 (Q40): the image registry is a standalone data plane
    // (blob CAS + tag retention under `<data_dir>/registry/`).
    // Degraded-but-booting, same as anyhome: a bind/fs failure
    // is a warn, the API surface still comes up.
    if let Some(rb) = &b.registry {
        match registry::spawn_registry(&cfg.data_dir, &rb.addr, rb.cfg.clone(), shutdown.clone()) {
            Ok(handle) => drains.registry = Some(handle),
            Err(e) => tracing::warn!(
                target: "init-pro",
                error = %e,
                "registry spawn failed (image plane degraded)"
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
                    match scheduler::SchedulerConfig::load_extenders(std::path::Path::new(value)) {
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
        let sched_handles =
            scheduler::SchedulerManager::spawn(store.clone(), sched_cfg, sched_stop.clone());
        tracing::info!(target: "init-pro", role, "scheduler started (leader-elected via Lease, Q18)");
        drains.scheduler = Some((sched_handles, sched_stop));
    }
    // Sprint 18 / S4 (Q28): the NodePort service plane runs by
    // default — LIST->WATCH reflectors over the SAME storage Arc
    // plus one reverse-proxy listener per allocated nodePort
    // (the kube-proxy-equivalent; `--disable-kube-proxy` is the
    // k3s-parity escape hatch that turns it off).
    if !b.disable_kube_proxy {
        let (resolver, reflectors) = router::supervise(store.clone(), shutdown.clone());
        // T4.3 Scope C (Q44): the SAME resolver feeds the ClusterIP VIP
        // plane — one lo-aliased listener per (VIP, servicePort).
        let clusterip = router::spawn_clusterip_plane(resolver.clone(), shutdown.clone());
        drains.service_plane = Some((
            router::spawn_nodeport_plane(
                resolver,
                router::NodePortConfig::default(),
                shutdown.clone(),
            ),
            clusterip,
            reflectors,
        ));
        tracing::info!(target: "init-pro", role,
            "service plane started: Router carries nodePort traffic (Sprint 18 / S4, Q28)");
        tracing::info!(target: "init-pro", role,
            "clusterIP plane started: Router carries ClusterIP VIP traffic (T4.3 Scope C, Q44)");
    }
    let server_shutdown = shutdown.clone();
    let addr = b.addr;
    // T7.5 (Q43): thread the anyhome endpoint into the
    // apiserver so the per-Pod anyhome defaulting is armed iff
    // the NFS plane is on (None = --disable-anyhome: inert).
    let workspace_nfs = b.anyhome.clone();
    // T1.3a (Q47): --token-file arms the authn/authz pair (loaded once,
    // fail-closed on a malformed row); --tls-san opts the listener into
    // TLS (self-signed CA + serving cert; k3s writes the CA next to the
    // data dir for kubeconfig use).
    let auth = match &b.token_file {
        Some(path) => match apiserver::TokenAuth::load(path) {
            Ok(a) => {
                tracing::info!(target: "init-pro", role,
                    tokens = a.len(), path = %path.display(),
                    "static-token authn armed (T1.3a, Q47)");
                Some(std::sync::Arc::new(a))
            }
            Err(e) => {
                tracing::error!(target: "init-pro", role,
                    "token file unreadable/malformed, refusing to serve unauthenticated: {e}");
                return Err(e);
            }
        },
        None => None,
    };
    let tls = if b.tls_san.is_empty() {
        None
    } else {
        match apiserver::generate_self_signed(&b.tls_san).and_then(|m| {
            let ca_path = cfg
                .data_dir
                .join("server")
                .join("tls")
                .join("server-ca.crt");
            if let Some(dir) = ca_path.parent() {
                std::fs::create_dir_all(dir)?;
                std::fs::write(&ca_path, &m.ca_pem)?;
            }
            apiserver::acceptor(&m)
        }) {
            Ok(a) => {
                tracing::info!(target: "init-pro", role,
                    sans = ?b.tls_san, "serving TLS armed (T1.3a, Q47)");
                Some(a)
            }
            Err(e) => {
                tracing::error!(target: "init-pro", role, "tls material failed: {e}");
                return Err(e);
            }
        }
    };
    drains.server_join = Some(tokio::spawn(async move {
        if let Err(e) = apiserver::serve(
            reg,
            store,
            addr,
            advertised,
            apiserver::ServeOptions {
                workspace_nfs,
                auth,
                tls,
                // T4.3 Scope C (Q44): drive the apiserver's clusterIP
                // allocator with the parsed --service-cidr.
                service_cidr: Some(service_cidr),
            },
            async move {
                server_shutdown.cancelled().await;
            },
        )
        .await
        {
            tracing::error!(target: "init-pro", role, "apiserver exited: {e}");
        }
    }));
    Ok(drains)
}
