//! Leader election (T3.1a, decision **Q18**): coordination.k8s.io `Lease`
//! objects with resourceVersion CAS -- no etcd leases, no third-party lock.
//!
//! Pure decision logic ([`lease_action`]) is split from I/O so the state
//! machine is unit-testable with a manual clock (see `tests/leaderelection.rs`).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use storage::Key;

use crate::client::Client;
use crate::error::ControllerError;
use crate::object::resource_version;
use crate::time::{now_rfc3339, parse_rfc3339};

/// Wall clock (unix seconds); injectable for tests.
pub type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Lease identity + timing knobs.
pub struct LeaseConfig {
    pub namespace: String,
    pub name: String,
    /// Unique holder identity of this elector (e.g. host + random suffix).
    pub identity: String,
    /// Lease validity in seconds; takeover is legal `>= lease_duration`
    /// after the holder's last renew.
    pub lease_duration: u64,
    /// Retry/acquire tick in milliseconds.
    pub retry_period: u64,
    pub now: NowFn,
}

/// What the elector should do given the current lease state. Pure.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum LeaseAction {
    /// No lease object yet: create it.
    CreateLease,
    /// We already hold it: refresh `renewTime`.
    Renew,
    /// Held by someone else but expired: CAS-take it.
    TakeOver,
    /// Held by someone else and still fresh.
    NotAcquired,
}

/// The decision for one acquire/renew attempt. Unparseable timestamps
/// resolve to 0 (i.e. expired) so a corrupt lease can always be taken over.
pub fn lease_action(
    lease: Option<&Value>,
    holder: &str,
    now: u64,
    lease_duration: u64,
) -> LeaseAction {
    let Some(lease) = lease else {
        return LeaseAction::CreateLease;
    };
    let current = lease
        .pointer("/spec/holderIdentity")
        .and_then(Value::as_str)
        .unwrap_or("");
    if current == holder {
        return LeaseAction::Renew;
    }
    let last = lease_last_renewed(lease).unwrap_or(0);
    if now.saturating_sub(last) >= lease_duration {
        LeaseAction::TakeOver
    } else {
        LeaseAction::NotAcquired
    }
}

fn lease_last_renewed(lease: &Value) -> Option<u64> {
    lease
        .pointer("/spec/renewTime")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .or_else(|| {
            lease
                .pointer("/spec/acquireTime")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339)
        })
}

/// A fresh lease claimed by `cfg.identity`.
pub fn new_lease(cfg: &LeaseConfig, now: u64) -> Value {
    let t = now_rfc3339(now);
    json!({
        "apiVersion": "coordination.k8s.io/v1",
        "kind": "Lease",
        "metadata": {"name": cfg.name, "namespace": cfg.namespace},
        "spec": {
            "holderIdentity": cfg.identity,
            "leaseDurationSeconds": cfg.lease_duration,
            "acquireTime": t,
            "renewTime": t,
            "leaseTransitions": 0,
        },
    })
}

/// Refresh `renewTime` only (we are the holder).
pub fn renew_lease(lease: &Value, now: u64) -> Value {
    let mut next = lease.clone();
    if let Some(spec) = next.pointer_mut("/spec/renewTime") {
        *spec = Value::String(now_rfc3339(now));
    }
    next
}

/// Steal an expired lease: new holder, fresh timestamps, transitions + 1.
pub fn takeover_lease(lease: &Value, cfg: &LeaseConfig, now: u64) -> Value {
    let mut next = lease.clone();
    let t = now_rfc3339(now);
    if let Some(spec) = next.get_mut("spec").and_then(Value::as_object_mut) {
        spec.insert("holderIdentity".into(), json!(cfg.identity));
        spec.insert("acquireTime".into(), json!(t));
        spec.insert("renewTime".into(), json!(t));
        let transitions = spec
            .get("leaseTransitions")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        spec.insert("leaseTransitions".into(), json!(transitions + 1));
    }
    next
}

/// Drives one Lease through acquire / renew / takeover attempts.
pub struct LeaderElector {
    client: Arc<dyn Client>,
    cfg: LeaseConfig,
}

impl LeaderElector {
    pub fn new(client: Arc<dyn Client>, cfg: LeaseConfig) -> Self {
        Self { client, cfg }
    }

    fn lease_key(&self) -> Key {
        Key::new(
            "coordination.k8s.io",
            "leases",
            self.cfg.namespace.clone(),
            self.cfg.name.clone(),
        )
    }

    /// One acquire-or-renew attempt. `Ok(true)` = we hold the lease after
    /// the call. CAS races surface as `Ok(false)` (retry next tick).
    pub async fn try_acquire_or_renew(&self) -> Result<bool, ControllerError> {
        let key = self.lease_key();
        let existing = self.client.get(&key).await?;
        let now = (self.cfg.now)();
        match lease_action(
            existing.as_ref(),
            &self.cfg.identity,
            now,
            self.cfg.lease_duration,
        ) {
            LeaseAction::NotAcquired => Ok(false),
            LeaseAction::CreateLease => {
                match self.client.create(&key, new_lease(&self.cfg, now)).await {
                    Ok(_) => Ok(true),
                    // Raced another candidate: retry next tick.
                    Err(e) if e.is_already_exists() || e.is_conflict() => Ok(false),
                    Err(e) => Err(e),
                }
            }
            LeaseAction::Renew => {
                // Renew at half-lease cadence (upstream RenewDeadline): skip
                // the write while the lease is still fresh so an idle leader
                // does not bump the cluster revision every retry tick.
                let lease = existing.as_ref().expect("renew implies existing");
                let last = lease_last_renewed(lease).unwrap_or(0);
                if now.saturating_sub(last) < self.cfg.lease_duration / 2 {
                    return Ok(true);
                }
                let rv = resource_version(lease).unwrap_or(0);
                match self
                    .client
                    .update(&key, renew_lease(lease, now), Some(rv))
                    .await
                {
                    Ok(_) => Ok(true),
                    Err(e) if e.is_conflict() => Ok(false),
                    Err(e) => Err(e),
                }
            }
            LeaseAction::TakeOver => {
                let lease = existing.as_ref().expect("takeover implies existing");
                let rv = resource_version(lease).unwrap_or(0);
                match self
                    .client
                    .update(&key, takeover_lease(lease, &self.cfg, now), Some(rv))
                    .await
                {
                    Ok(_) => Ok(true),
                    Err(e) if e.is_conflict() => Ok(false),
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// Spawn the retry loop; reports leadership on the returned watch
    /// channel. Client errors never panic the loop (log + not-leader).
    pub fn spawn(
        self,
        stop: crate::Stop,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio::sync::watch::Receiver<bool>,
    ) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let retry = Duration::from_millis(self.cfg.retry_period);
        let handle = tokio::spawn(async move {
            loop {
                let leader = self.try_acquire_or_renew().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "leader-election attempt failed");
                    false
                });
                let _ = tx.send(leader);
                tokio::select! {
                    _ = stop.cancelled() => {
                        let _ = tx.send(false);
                        break;
                    }
                    _ = tokio::time::sleep(retry) => {}
                }
            }
        });
        (handle, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(identity: &str) -> LeaseConfig {
        LeaseConfig {
            namespace: "kube-system".into(),
            name: "cm".into(),
            identity: identity.into(),
            lease_duration: 30,
            retry_period: 10,
            now: Arc::new(|| 1000),
        }
    }

    fn lease(holder: &str, renew: &str) -> Value {
        json!({"spec": {"holderIdentity": holder, "renewTime": renew, "leaseTransitions": 2}})
    }

    #[test]
    fn lease_action_decisions() {
        let mine = lease("a", "2001-09-09T01:46:40Z"); // t=1000000000 -> far past
        assert_eq!(lease_action(None, "a", 1000, 30), LeaseAction::CreateLease);
        // Mine: always renew, even if the timestamp says expired.
        assert_eq!(
            lease_action(Some(&mine), "a", 9_999_999_999, 30),
            LeaseAction::Renew
        );
        // Fresh foreign lease: not acquired.
        assert_eq!(
            lease_action(Some(&lease("b", "1970-01-01T00:16:40Z")), "a", 1000, 30),
            LeaseAction::NotAcquired
        );
        // Expired foreign lease (renewed at 1000, now >= 1030): takeover.
        assert_eq!(
            lease_action(Some(&lease("b", "1970-01-01T00:16:40Z")), "a", 1030, 30),
            LeaseAction::TakeOver
        );
        // Unparseable time -> 0 -> expired once now >= lease_duration.
        assert_eq!(
            lease_action(Some(&lease("b", "bogus")), "a", 29, 30),
            LeaseAction::NotAcquired
        );
        assert_eq!(
            lease_action(Some(&lease("b", "bogus")), "a", 30, 30),
            LeaseAction::TakeOver
        );
    }

    #[test]
    fn lease_builders_shape() {
        let l = new_lease(&cfg("a"), 0);
        assert_eq!(l["metadata"]["name"], "cm");
        assert_eq!(l["spec"]["holderIdentity"], "a");
        assert_eq!(l["spec"]["leaseDurationSeconds"], 30);
        assert_eq!(l["spec"]["leaseTransitions"], 0);
        assert_eq!(l["spec"]["acquireTime"], "1970-01-01T00:00:00Z");

        let renewed = renew_lease(&l, 60);
        assert_eq!(renewed["spec"]["renewTime"], "1970-01-01T00:01:00Z");
        assert_eq!(renewed["spec"]["acquireTime"], l["spec"]["acquireTime"]);

        let took = takeover_lease(&l, &cfg("b"), 60);
        assert_eq!(took["spec"]["holderIdentity"], "b");
        assert_eq!(took["spec"]["leaseTransitions"], 1);
        assert_eq!(took["spec"]["acquireTime"], "1970-01-01T00:01:00Z");
    }
}
