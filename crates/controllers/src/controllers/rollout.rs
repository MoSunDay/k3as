//! Deployment rollout math (TODO **T3.1b**): strategy parsing,
//! maxSurge/maxUnavailable resolution with upstream rounding (surge ceil,
//! unavailable floor), and the spec-replica-level target computation for
//! RollingUpdate pacing and Recreate gating. JSON-only wire (decision
//! **Q10**): strategies are read off raw `serde_json::Value` Deployment
//! specs and garbage falls back to upstream defaults (25%/25%).
//!
//! This is the v1 spec-replica-level approximation of the upstream
//! availability-window algorithm: without kubelet (T4.2) pods are
//! ready-by-default, so spec replicas track availability and the pacing
//! below operates on ReplicaSet counts, not individual pods. Two documented
//! T3.1b deviations keep the semantics total:
//!
//! 1. **Availability freeze**: old capacity is never retired while the fleet
//!    is below `desired - maxUnavailable` ready replicas -- an explicitly
//!    not-Ready pod stalls the rollout (surfacing later as
//!    ProgressDeadlineExceeded, matching the observable upstream behavior).
//! 2. **Degenerate swap**: `maxSurge=0 AND maxUnavailable=0` (rejected by
//!    upstream validation) swaps old for new directly once availability is
//!    confirmed -- otherwise no sequence of spec moves could ever finish.

use serde_json::Value;

/// An int-or-percent maxSurge/maxUnavailable quantity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Quantity {
    Int(u64),
    Percent(u64),
}

/// Parsed `spec.strategy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Strategy {
    Recreate,
    RollingUpdate {
        max_surge: Quantity,
        max_unavailable: Quantity,
    },
}

/// Read `spec.strategy` (T3.1b). "Recreate" -> [`Strategy::Recreate`];
/// anything else (including absent or garbage) -> RollingUpdate with
/// per-field defaults `Percent(25)` when maxSurge/maxUnavailable are not an
/// int or "NN%" string.
pub(crate) fn parse_strategy(spec: &Value) -> Strategy {
    let strategy = spec.pointer("/strategy");
    if strategy.and_then(|s| s.get("type")).and_then(Value::as_str) == Some("Recreate") {
        return Strategy::Recreate;
    }
    let rolling = strategy.and_then(|s| s.get("rollingUpdate"));
    Strategy::RollingUpdate {
        max_surge: parse_quantity(rolling.and_then(|r| r.get("maxSurge"))),
        max_unavailable: parse_quantity(rolling.and_then(|r| r.get("maxUnavailable"))),
    }
}

/// int-or-"NN%" (upstream IntOrString), defaulting to 25% on garbage.
fn parse_quantity(v: Option<&Value>) -> Quantity {
    match v {
        Some(Value::Number(n)) => n
            .as_u64()
            .map(Quantity::Int)
            .unwrap_or(Quantity::Percent(25)),
        Some(Value::String(s)) => s
            .strip_suffix('%')
            .and_then(|p| p.parse().ok())
            .map(Quantity::Percent)
            .unwrap_or(Quantity::Percent(25)),
        _ => Quantity::Percent(25),
    }
}

/// Resolve a maxSurge quantity against `total` replicas: percents round UP
/// (upstream `GetSurge` ceil).
pub(crate) fn resolve_surge(q: &Quantity, total: u64) -> u64 {
    match *q {
        Quantity::Int(n) => n,
        Quantity::Percent(p) => (total.saturating_mul(p).saturating_add(99)) / 100,
    }
}

/// Resolve a maxUnavailable quantity against `total` replicas: percents
/// round DOWN (upstream `GetUnavailable` floor).
pub(crate) fn resolve_unavailable(q: &Quantity, total: u64) -> u64 {
    match *q {
        Quantity::Int(n) => n,
        Quantity::Percent(p) => total.saturating_mul(p) / 100,
    }
}

/// A ReplicaSet reduced to the fields the rollout math needs: name,
/// `spec.replicas`, `status.readyReplicas` (built by the caller).
#[derive(Clone, Debug)]
pub(crate) struct RsView {
    pub name: String,
    pub spec_replicas: u64,
    pub ready_replicas: u64,
}

/// RollingUpdate targets: the new RS's target `spec.replicas` and each old
/// RS's target, in input order (see module docs for the approximation).
///
/// * Scale-up (upstream `NewRSNewReplicas`): the new RS grows into the
///   surge headroom, never past `desired`.
/// * Scale-down (upstream `reconcileOldReplicaSets` maxScaledDown): old
///   capacity is retired by `total - (desired - maxUnavailable) -
///   unavailable-new-pods`, so a not-yet-ready new RS holds old pods in
///   place. Olds are never scaled up.
pub(crate) fn rolling_targets(
    desired: u64,
    max_surge: u64,
    max_unavailable: u64,
    new: &RsView,
    old: &[RsView],
) -> (u64, Vec<(String, u64)>) {
    let old_sum: u64 = old.iter().map(|r| r.spec_replicas).sum();
    let ready_total = new.ready_replicas + old.iter().map(|r| r.ready_replicas).sum::<u64>();

    // Scale-up: room under the `desired + maxSurge` total-pod ceiling.
    let headroom = desired
        .saturating_add(max_surge)
        .saturating_sub(new.spec_replicas + old_sum);
    let new_target = desired.min(new.spec_replicas + headroom);

    // Scale-down budget over the post-scale-up totals.
    let min_available = desired.saturating_sub(max_unavailable);
    let new_unavailable = new_target.saturating_sub(new.ready_replicas);
    let mut budget = new_target
        .saturating_add(old_sum)
        .saturating_sub(min_available)
        .saturating_sub(new_unavailable);

    // v1 availability freeze: below the ready floor nothing retires.
    if !old.is_empty() && ready_total < min_available {
        budget = 0;
    }
    // Degenerate maxSurge=0 + maxUnavailable=0: swap directly once the
    // fleet confirms availability (upstream validation rejects the config).
    if max_surge == 0 && max_unavailable == 0 && ready_total >= desired {
        budget = old_sum;
    }

    let old_target_total = old_sum.saturating_sub(budget);
    // Distribute: the first old RS keeps replicas first (later olds drain
    // first), never scaling any old up.
    let mut remaining = old_target_total;
    let targets = old
        .iter()
        .map(|r| {
            let target = r.spec_replicas.min(remaining);
            remaining -= target;
            (r.name.clone(), target)
        })
        .collect();
    (new_target, targets)
}

/// Recreate targets (T3.1b): all old capacity drains to 0 before the new RS
/// scales up -- while any old RS holds replicas the new RS stays at its
/// current spec (0 when absent).
pub(crate) fn recreate_targets(
    desired: u64,
    new: &RsView,
    old: &[RsView],
) -> (u64, Vec<(String, u64)>) {
    let olds_blocked = old.iter().any(|r| r.spec_replicas > 0);
    let new_target = if olds_blocked {
        new.spec_replicas
    } else {
        desired
    };
    let targets = old.iter().map(|r| (r.name.clone(), 0)).collect::<Vec<_>>();
    (new_target, targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_strategy_defaults_and_forms() {
        // Absent strategy / rollingUpdate -> 25% defaults.
        assert_eq!(
            parse_strategy(&json!({})),
            Strategy::RollingUpdate {
                max_surge: Quantity::Percent(25),
                max_unavailable: Quantity::Percent(25),
            }
        );
        assert_eq!(
            parse_strategy(&json!({"strategy": {"type": "RollingUpdate"}})),
            parse_strategy(&json!({}))
        );
        // Ints and percent strings.
        assert_eq!(
            parse_strategy(&json!({"strategy": {"type": "RollingUpdate",
                "rollingUpdate": {"maxSurge": 2, "maxUnavailable": "10%"}}})),
            Strategy::RollingUpdate {
                max_surge: Quantity::Int(2),
                max_unavailable: Quantity::Percent(10),
            }
        );
        // Recreate.
        assert_eq!(
            parse_strategy(&json!({"strategy": {"type": "Recreate"}})),
            Strategy::Recreate
        );
        // Garbage tolerated: unknown type, garbage quantities, negatives.
        assert_eq!(
            parse_strategy(&json!({"strategy": {"type": "Bogus",
                "rollingUpdate": {"maxSurge": "lots", "maxUnavailable": -1}}})),
            parse_strategy(&json!({}))
        );
    }

    #[test]
    fn quantity_rounding_ceil_surge_floor_unavailable() {
        // 25% of 3: surge ceil(0.75)=1, unavailable floor(0.75)=0.
        let q = Quantity::Percent(25);
        assert_eq!(resolve_surge(&q, 3), 1);
        assert_eq!(resolve_unavailable(&q, 3), 0);
        // 50% of 5: ceil(2.5)=3, floor(2.5)=2.
        let q = Quantity::Percent(50);
        assert_eq!(resolve_surge(&q, 5), 3);
        assert_eq!(resolve_unavailable(&q, 5), 2);
        // Exact, zero, full, and ints.
        assert_eq!(resolve_surge(&Quantity::Percent(0), 7), 0);
        assert_eq!(resolve_unavailable(&Quantity::Percent(100), 7), 7);
        assert_eq!(resolve_surge(&Quantity::Int(4), 7), 4);
        assert_eq!(resolve_unavailable(&Quantity::Int(4), 7), 4);
    }

    fn view(name: &str, spec: u64, ready: u64) -> RsView {
        RsView {
            name: name.into(),
            spec_replicas: spec,
            ready_replicas: ready,
        }
    }

    #[test]
    fn fresh_deployment_scales_straight_to_desired() {
        let (new_target, olds) = rolling_targets(3, 1, 0, &view("n", 0, 0), &[]);
        assert_eq!(new_target, 3);
        assert!(olds.is_empty());
    }

    #[test]
    fn surge_cap_respected_and_paced_by_new_readiness() {
        // desired=3, surge=1, unavailable=0. Start: old 3/3 ready, new 0.
        let old = [view("o1", 3, 3)];
        let (new_target, olds) = rolling_targets(3, 1, 0, &view("n", 0, 0), &old);
        assert_eq!(new_target, 1, "new grows only into the surge headroom");
        assert_eq!(
            olds[0],
            ("o1".into(), 3),
            "not-ready new pod holds old pods"
        );

        // New pod up+ready: exactly one old replica retires (1-for-1).
        let (_, olds) = rolling_targets(3, 1, 0, &view("n", 1, 1), &old);
        assert_eq!(olds[0], ("o1".into(), 2));

        // New fully rolled out: the last old replica retires.
        let old = [view("o1", 1, 1)];
        let (new_target, olds) = rolling_targets(3, 1, 0, &view("n", 3, 3), &old);
        assert_eq!(new_target, 3);
        assert_eq!(olds[0], ("o1".into(), 0));
    }

    #[test]
    fn old_rs_cap_distribution_first_old_keeps_replicas() {
        // desired=4, surge=1: budget retires exactly 1; the FIRST old keeps
        // replicas and later olds drain first; no old scales up.
        let olds = [view("o1", 3, 3), view("o2", 1, 1)];
        let (_, targets) = rolling_targets(4, 1, 0, &view("n", 1, 1), &olds);
        assert_eq!(targets, vec![("o1".into(), 3), ("o2".into(), 0)]);
        // Full drain spreads 0 to everyone.
        let olds = [view("o1", 2, 2), view("o2", 2, 2)];
        let (_, targets) = rolling_targets(4, 1, 0, &view("n", 4, 4), &olds);
        assert_eq!(targets, vec![("o1".into(), 0), ("o2".into(), 0)]);
    }

    #[test]
    fn not_ready_fleet_freezes_the_rollout() {
        // desired=1, surge=0, unavailable=0, the only old pod not ready:
        // nothing moves (the ProgressDeadline vector).
        let old = [view("o1", 1, 0)];
        let (new_target, olds) = rolling_targets(1, 0, 0, &view("n", 0, 0), &old);
        assert_eq!(new_target, 0);
        assert_eq!(olds[0], ("o1".into(), 1));
    }

    #[test]
    fn degenerate_zero_config_swaps_when_available() {
        // maxSurge=0 + maxUnavailable=0 with the old pod ready: swap.
        let old = [view("o1", 1, 1)];
        let (new_target, olds) = rolling_targets(1, 0, 0, &view("n", 0, 0), &old);
        assert_eq!(new_target, 0);
        assert_eq!(
            olds[0],
            ("o1".into(), 0),
            "old retires so new can take over"
        );
    }

    #[test]
    fn recreate_gates_on_old_capacity() {
        // Old RS still holding replicas -> new stays put, olds drain.
        let old = [view("o1", 2, 2)];
        let (new_target, olds) = recreate_targets(2, &view("n", 0, 0), &old);
        assert_eq!(new_target, 0);
        assert_eq!(olds[0], ("o1".into(), 0));
        // Olds drained -> new scales to desired.
        let (new_target, olds) = recreate_targets(2, &view("n", 0, 0), &[]);
        assert_eq!(new_target, 2);
        assert!(olds.is_empty());
    }
}
