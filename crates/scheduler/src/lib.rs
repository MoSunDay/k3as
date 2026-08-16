//! kube-scheduler equivalent (TODO **T3.2**, decision **Q23**).
//!
//! A filter/score plugin framework over the same client-go machinery the
//! controllers crate exports (`Client`/`Informer`/`WorkQueue`/`LeaderElector`
//! — reused, not copied), driven **in-process** against the apiserver's
//! storage `Arc` (decision **Q19**): one queue of pending pods, one
//! snapshot-per-attempt scheduling cycle, a lease-elected single active
//! scheduler (`init-pro-scheduler`, decision **Q18**).
//!
//! Default plugins (upstream parity, raw-JSON pure functions):
//!  - filters: `NodeName`, `NodeUnschedulable`, `TaintToleration`,
//!    `NodeAffinity`, `PodAntiAffinity`, `ResourceFit`, `VolumeBinding`
//!    (v1 passthrough — no PV binder until T6.2, Q22).
//!  - scores: `LeastRequested` (+ preferred `NodeAffinity` /
//!    `PodAntiAffinity` bonuses).
//!  - A logical Node with no `status.capacity/allocatable` is treated as
//!    unbounded (logged once) — v1 test/headless nodes (Q23).
//!
//! Extender seam (**Q3**): upstream-compatible HTTP JSON extenders
//! (`{url}/{filterVerb}`, `/{prioritizeVerb}`, `nodeCacheCapable=false`,
//! timeouts + `ignorable` failure semantics; HTTP-only, **Q10** — no gRPC in
//! v1). Layer 7 (T7.1) AI-agent policies plug in here.

#![forbid(unsafe_code)]

pub mod affinity;
pub mod bind;
pub mod cycle;
pub mod extender;
pub mod filters;
pub mod http;
pub mod plugin;
pub mod resources;
pub mod runner;
pub mod scores;

pub use cycle::{schedule_one, Outcome};
pub use extender::{ExtenderConfig, ExtenderSet};
pub use plugin::{default_filters, default_scores, Filter, NodeInfo, Score, Snapshot, Verdict};
pub use runner::{SchedulerConfig, SchedulerManager};

#[cfg(test)]
mod tests {
    #[test]
    fn plugin_registry_names_are_stable() {
        // Golden G22/G23 depend on this exact registry; guard the order.
        let names: Vec<&str> = super::default_filters().iter().map(|f| f.name()).collect();
        assert_eq!(
            names,
            vec![
                "NodeName",
                "NodeUnschedulable",
                "TaintToleration",
                "NodeAffinity",
                "PodAntiAffinity",
                "ResourceFit",
                "VolumeBinding",
            ]
        );
        let names: Vec<&str> = super::default_scores().iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            vec![
                "LeastRequested",
                "NodeAffinityPreferred",
                "PodAntiAffinityPreferred"
            ]
        );
    }
}
