//! Argo Workflows metrics types (TODO **T7.3 Scope A**, decision **Q30**).
//!
//! 1:1 typed port of upstream `workflow_types.go` v4 lines ~3995-4167
//! (`Metrics`, `Prometheus`, `MetricLabel`, `Gauge`, `Histogram`, `Counter`)
//! — the field-level SSOT is the Go json tags; wire format is JSON-only
//! (Q10). Data-only port: the Go `GetMetricType`/`GetKey` helpers are out of
//! scope. `GaugeOperation` comes from [`super::phases`], `Amount` (raw JSON
//! number passthrough) from [`super::scalar`].

use serde::{Deserialize, Serialize};

use super::phases::GaugeOperation;
use super::scalar::Amount;

/// Metrics are a list of metrics emitted from a Workflow/Template.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    /// Prometheus is a list of prometheus metrics to be emitted
    /// (upstream caps it at 100 items via kubebuilder validation).
    #[serde(default)]
    pub prometheus: Vec<Prometheus>,
}

/// Prometheus is a prometheus metric to be emitted.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Prometheus {
    /// Name is the name of the metric
    /// (`^[a-zA-Z_][a-zA-Z0-9_]*$` upstream).
    #[serde(default)]
    pub name: String,
    /// Labels is a list of metric labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<MetricLabel>,
    /// Help is a string that describes the metric (min length 1 upstream).
    #[serde(default)]
    pub help: String,
    /// When is a conditional statement that decides when to emit the metric.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub when: String,
    /// Gauge is a gauge metric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gauge: Option<Gauge>,
    /// Histogram is a histogram metric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub histogram: Option<Histogram>,
    /// Counter is a counter metric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counter: Option<Counter>,
}

/// MetricLabel is a single label for a prometheus metric.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MetricLabel {
    /// Label key (`^[a-zA-Z_][a-zA-Z0-9_]*$` upstream).
    #[serde(default)]
    pub key: String,
    /// Label value.
    #[serde(default)]
    pub value: String,
}

/// Gauge is a Gauge prometheus metric.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Gauge {
    /// Value is the value to be used in the operation with the metric's
    /// current value. If no operation is set, value is the value of the
    /// metric.
    #[serde(default)]
    pub value: String,
    /// Realtime emits this metric in real time if applicable.
    ///
    /// Go pointer without `omitempty`: always serialized (`null` when unset),
    /// mirroring `encoding/json` behaviour for nil pointers.
    #[serde(default)]
    pub realtime: Option<bool>,
    /// Operation defines the operation to apply with value and the metrics'
    /// current value (enum `Set`/`Add`/`Sub`, see [`GaugeOperation`]).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub operation: GaugeOperation,
}

/// Histogram is a Histogram prometheus metric.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Histogram {
    /// Value is the value of the metric.
    #[serde(default)]
    pub value: String,
    /// Buckets is a list of bucket divisors for the histogram
    /// (raw JSON numbers, see [`Amount`]).
    #[serde(default)]
    pub buckets: Vec<Amount>,
}

/// Counter is a Counter prometheus metric.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Counter {
    /// Value is the value of the metric.
    #[serde(default)]
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_counter_roundtrips_camel_case() {
        let raw = serde_json::json!({
            "name": "result_count",
            "help": "counts results",
            "counter": {}
        });
        let p: Prometheus = serde_json::from_value(raw).expect("parse counter metric");
        assert_eq!(p.name, "result_count");
        assert_eq!(p.help, "counts results");
        let counter = p.counter.as_ref().expect("counter present");
        assert_eq!(counter.value, "");

        // Re-serialize: camelCase keys, omitempty fields dropped,
        // required fields always present.
        let out = serde_json::to_value(&p).expect("serialize");
        assert_eq!(out["name"], serde_json::json!("result_count"));
        assert_eq!(out["help"], serde_json::json!("counts results"));
        assert_eq!(out["counter"], serde_json::json!({"value": ""}));
        assert!(out.get("gauge").is_none());
        assert!(out.get("labels").is_none());

        // Full round-trip identity.
        let p2: Prometheus = serde_json::from_value(out).expect("re-parse");
        assert_eq!(p, p2);
    }

    #[test]
    fn gauge_with_string_value_parses() {
        let g: Gauge =
            serde_json::from_value(serde_json::json!({"value": "1"})).expect("parse gauge");
        assert_eq!(g.value, "1");
        assert_eq!(g.realtime, None);
        assert_eq!(g.operation, "");
    }
}
