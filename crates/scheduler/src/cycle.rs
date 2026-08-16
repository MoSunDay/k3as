//! The scheduling cycle (T3.2): one attempt for one pod —
//! local filters -> extender filters -> weighted scores (+ extender
//! prioritize) -> highest node wins (ties: snapshot order, deterministic for
//! golden). Pure aside from the optional extender HTTP calls; all input
//! state arrives as an immutable [`Snapshot`].

use serde_json::Value;

use crate::extender::ExtenderSet;
use crate::plugin::{Filter, Score, Snapshot, Verdict};

/// One cycle outcome: the chosen node, or why none was feasible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub node: Option<String>,
    pub reason: String,
}

impl Outcome {
    fn placed(node: String) -> Self {
        Outcome {
            node: Some(node),
            reason: String::new(),
        }
    }

    fn unschedulable(reason: String) -> Self {
        Outcome { node: None, reason }
    }
}

/// Run one scheduling attempt for `pod` against `snap`.
///
/// `extenders = None` (or empty) is the default plugin-only path — pure and
/// unit-testable. Extender failures of a non-ignorable extender surface as
/// an unschedulable outcome with the extender error as the reason (the
/// worker's write-if-changed guard keeps this from hot-looping).
pub async fn schedule_one(
    pod: &Value,
    snap: &Snapshot,
    filters: &[Box<dyn Filter>],
    scores: &[Box<dyn Score>],
    extenders: Option<&ExtenderSet>,
) -> Outcome {
    // 1. Local filters.
    let mut feasible: Vec<&crate::plugin::NodeInfo> = Vec::new();
    let mut rejects: Vec<String> = Vec::new();
    for info in &snap.nodes {
        let mut pass = true;
        for f in filters {
            match f.filter(pod, info, snap) {
                Verdict::Pass => {}
                Verdict::Reject { plugin, reason } => {
                    rejects.push(format!("{plugin}: {reason}"));
                    pass = false;
                    break;
                }
            }
        }
        if pass {
            feasible.push(info);
        }
    }
    if feasible.is_empty() {
        let mut seen = rejects;
        seen.sort();
        seen.dedup();
        return Outcome::unschedulable(seen.join("; "));
    }

    // 2. Extender filter (Q3 seam).
    let feasible_names: Vec<String> = match extenders {
        Some(set) if !set.is_empty() => match set.filter(pod, &feasible).await {
            Ok(names) => names,
            Err(e) => return Outcome::unschedulable(format!("Extender: {e}")),
        },
        _ => feasible
            .iter()
            .filter_map(|i| {
                i.node
                    .pointer("/metadata/name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect(),
    };
    if feasible_names.is_empty() {
        return Outcome::unschedulable("Extender: rejected every node".into());
    }
    let ranked: Vec<&crate::plugin::NodeInfo> = feasible
        .iter()
        .copied()
        .filter(|i| {
            i.node
                .pointer("/metadata/name")
                .and_then(|v| v.as_str())
                .map(|n| feasible_names.iter().any(|f| f == n))
                .unwrap_or(false)
        })
        .collect();
    if ranked.is_empty() {
        return Outcome::unschedulable("Extender: unknown node names".into());
    }

    // 3. Weighted local scores.
    let mut totals: Vec<(&crate::plugin::NodeInfo, i64)> = ranked
        .iter()
        .map(|info| {
            let total: i64 = scores
                .iter()
                .map(|s| s.score(pod, info, snap) * s.weight())
                .sum();
            (*info, total)
        })
        .collect();

    // 4. Extender prioritize (weighted add).
    if let Some(set) = extenders {
        if !set.is_empty() {
            match set.prioritize(pod, &ranked).await {
                Ok(deltas) => {
                    for (info, total) in totals.iter_mut() {
                        let name = info
                            .node
                            .pointer("/metadata/name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        if let Some(delta) = deltas.get(name) {
                            *total += delta;
                        }
                    }
                }
                Err(e) => return Outcome::unschedulable(format!("Extender: {e}")),
            }
        }
    }

    // 5. Highest total wins; ties keep snapshot order (stable for golden).
    let best = totals
        .iter()
        .max_by_key(|(_, total)| *total)
        .expect("ranked is non-empty");
    let node = best
        .0
        .node
        .pointer("/metadata/name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Outcome::placed(node)
}
