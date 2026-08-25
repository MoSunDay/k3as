//! Argo Workflows template inputs/outputs & data-source types (TODO
//! **T7.3 Scope A**, decision **Q30**).
//!
//! 1:1 typed port of the `Inputs`/`Outputs`/`Parameter`/`ValueFrom`/
//! `SuppliedValueFrom`/`Arguments`/`Sequence` block of upstream
//! `workflow_types.go` v4 plus all of `data_types.go`. Field-level SSOT is
//! the Go json tags; wire format is JSON-only (decision **Q10**). Data-only
//! port: Go helper methods (`HasValue`, `IsEmpty`, `GetArtifactIfNeeded`)
//! are not carried.

use k8s_openapi::api::core::v1 as apiv1;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use serde::{Deserialize, Serialize};

use super::artifact::{Artifact, Artifacts};
use super::scalar::AnyString;

/// Inputs are the mechanism for passing parameters and artifacts from one
/// template to another (Go: patchStrategy=merge on both lists).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Inputs {
    /// Parameters are a list of parameters passed as inputs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Parameter>,
    /// Artifacts are a list of artifacts passed as inputs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Artifacts,
}

/// Parameter indicates a passed string parameter to a service template with
/// an optional default value.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Parameter {
    /// Name is the parameter name.
    #[serde(default)]
    pub name: String,
    /// Default is the default value to use for an input parameter if a value
    /// was not supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<AnyString>,
    /// Value is the literal value to use for the parameter. In the context of
    /// an input parameter, any passed values take precedence over it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<AnyString>,
    /// ValueFrom is the source for the output parameter's value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_from: Option<ValueFrom>,
    /// GlobalName exports an output parameter to the global scope, making it
    /// available as `workflow.outputs.parameters.XXXX` and in
    /// `workflow.status.outputs.parameters`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub global_name: String,
    /// Enum holds a list of string values to choose from, for the actual
    /// value of the parameter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub r#enum: Vec<AnyString>,
    /// Description is the parameter description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<AnyString>,
}

/// ValueFrom describes a location in which to obtain the value to a
/// parameter.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ValueFrom {
    /// Path in the container to retrieve an output parameter value from in
    /// container templates.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// JSONPath of a resource to retrieve an output parameter value from in
    /// resource templates.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub json_path: String,
    /// JQFilter expression against the resource object in resource templates.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub jq_filter: String,
    /// Selector (https://github.com/expr-lang/expr) that is evaluated against
    /// the event to get the value of the parameter, e.g. `payload.message`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub event: String,
    /// Parameter reference to a step or dag task in which to retrieve an
    /// output parameter value from (e.g. `steps.mystep.outputs.myparam`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parameter: String,
    /// Supplied value to be filled in directly, either through the CLI,
    /// API, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supplied: Option<SuppliedValueFrom>,
    /// ConfigMapKeyRef is configmap selector for input parameter
    /// configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map_key_ref: Option<apiv1::ConfigMapKeySelector>,
    /// Default specifies a value to be used if retrieving the value from the
    /// specified source fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<AnyString>,
    /// Expression, if defined, is evaluated to specify the value for the
    /// parameter.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub expression: String,
}

/// SuppliedValueFrom is a placeholder for a value to be filled in directly,
/// either through the CLI, API, etc. (Go empty struct — serializes as `{}`.)
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SuppliedValueFrom {}

/// Outputs hold outputs of a template: parameters, artifacts, and the
/// result/exit-code of script, container or HTTP templates.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Outputs {
    /// Parameters holds the list of output parameters produced by a step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Parameter>,
    /// Artifacts holds the list of output artifacts produced by a step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Artifacts,
    /// Result holds the result (stdout) of a script or container template, or
    /// the response body of an HTTP template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// ExitCode holds the exit code of a script template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<String>,
}

/// Arguments to a template or workflow: parameters plus artifacts.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Arguments {
    /// Parameters is the list of parameters to pass to the template or
    /// workflow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Parameter>,
    /// Artifacts is the list of artifacts to pass to the template or
    /// workflow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Artifacts,
}

/// Sequence expands a workflow step into multiple parallel steps, one for
/// each element of the sequence.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Sequence {
    /// Count is the number of elements in the sequence (default: 0). Not to
    /// be used with `end`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<IntOrString>,
    /// Number at which to start the sequence (default: 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<IntOrString>,
    /// Number at which to end the sequence (default: 0). Not to be used with
    /// `count`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<IntOrString>,
    /// Format is a printf format string to format the value in the sequence.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub format: String,
}

/// Data is a data template: sources external data in and applies a set of
/// transformations to it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Data {
    /// Source sources external data into a data template.
    #[serde(default)]
    pub source: DataSource,
    /// Transformation applies a set of transformations.
    #[serde(default)]
    pub transformation: Transformation,
}

/// Transformation is a list of transformation steps applied in order
/// (Go named slice type).
pub type Transformation = Vec<TransformationStep>;

/// TransformationStep is a single step of a [`Transformation`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TransformationStep {
    /// Expression defines an expr expression to apply.
    #[serde(default)]
    pub expression: String,
}

/// DataSource sources external data into a data template.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DataSource {
    /// ArtifactPaths is a data transformation that collects a list of
    /// artifact paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_paths: Option<ArtifactPaths>,
}

/// ArtifactPaths expands a step from a collection of artifacts. The embedded
/// Go `Artifact` (`json:",inline"`) is flattened onto the wire via
/// `#[serde(flatten)]`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactPaths {
    /// Artifact is the artifact location from which to source the artifacts;
    /// it can be a directory.
    #[serde(flatten)]
    pub artifact: Artifact,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_default_from_json_number() {
        let p: Parameter = serde_json::from_str(r#"{"name":"p","default":42}"#).unwrap();
        assert_eq!(p.name, "p");
        assert_eq!(p.default, Some(AnyString("42".to_string())));
        // Enum round-trips through AnyString numbers as strings.
        let p: Parameter = serde_json::from_str(r#"{"name":"p","enum":["a",7]}"#).unwrap();
        assert_eq!(p.r#enum.len(), 2);
        assert_eq!(p.r#enum[1].0, "7");
    }

    #[test]
    fn parameter_minimal_serializes_name_only() {
        let p = Parameter {
            name: "p".to_string(),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            serde_json::json!({"name": "p"})
        );
    }

    #[test]
    fn inputs_round_trip_with_parameters_and_artifacts() {
        let raw = r#"{
            "parameters": [
                {"name": "message", "value": "hello"},
                {"name": "n", "default": 42}
            ],
            "artifacts": [{"name": "src", "path": "/tmp/src"}]
        }"#;
        let inputs: Inputs = serde_json::from_str(raw).unwrap();
        assert_eq!(inputs.parameters.len(), 2);
        assert_eq!(inputs.parameters[0].value.as_ref().unwrap().0, "hello");
        assert_eq!(
            inputs.parameters[1].default.as_ref().unwrap().0,
            "42",
            "JSON number default deserializes as AnyString(\"42\")"
        );
        assert_eq!(inputs.artifacts.len(), 1);

        let v = serde_json::to_value(&inputs).unwrap();
        // The flattened Artifact on ArtifactPaths is not in play here; check
        // the Inputs payload itself round-trips losslessly.
        assert_eq!(v["parameters"][0]["name"], "message");
        assert_eq!(v["parameters"][0]["value"], "hello");
        assert_eq!(v["parameters"][1]["default"], "42");
        assert_eq!(v["artifacts"][0]["name"], "src");
        let back: Inputs = serde_json::from_value(v).unwrap();
        assert_eq!(back, inputs);
    }

    #[test]
    fn value_from_full_field_set_round_trip() {
        let raw = r#"{
            "path": "/tmp/x",
            "jsonPath": "{.status.phase}",
            "jqFilter": ".foo",
            "event": "payload.message",
            "parameter": "steps.step.outputs.p",
            "supplied": {},
            "default": "fallback"
        }"#;
        let vf: ValueFrom = serde_json::from_str(raw).unwrap();
        assert_eq!(vf.json_path, "{.status.phase}");
        assert!(vf.supplied.is_some());
        let v = serde_json::to_value(&vf).unwrap();
        assert_eq!(v["supplied"], serde_json::json!({}));
        assert_eq!(v["default"], "fallback");
        assert_eq!(serde_json::from_value::<ValueFrom>(v).unwrap(), vf);
    }

    #[test]
    fn data_source_flattens_artifact_paths() {
        let raw = r#"{
            "source": {"artifactPaths": {"name": "dir", "path": "/data"}},
            "transformation": [{"expression": "filter(.size > 0)"}]
        }"#;
        let d: Data = serde_json::from_str(raw).unwrap();
        assert!(d.source.artifact_paths.is_some());
        assert_eq!(d.transformation.len(), 1);
        let v = serde_json::to_value(&d).unwrap();
        // `source` and `transformation` carry no omitempty in Go -> always
        // serialized, even when default.
        assert_eq!(
            serde_json::to_value(Data::default()).unwrap(),
            serde_json::json!({"source": {}, "transformation": []})
        );
        assert_eq!(
            v["source"]["artifactPaths"]["name"],
            serde_json::json!("dir")
        );
    }

    #[test]
    fn outputs_result_exit_code_are_optional_strings() {
        let o: Outputs = serde_json::from_str(r#"{"result":"out","exitCode":"0"}"#).unwrap();
        assert_eq!(o.result.as_deref(), Some("out"));
        assert_eq!(o.exit_code.as_deref(), Some("0"));
        assert_eq!(
            serde_json::to_value(Outputs::default()).unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn sequence_int_or_string_fields() {
        let s: Sequence =
            serde_json::from_str(r#"{"count":10,"start":"1","end":20,"format":"item-%d"}"#)
                .unwrap();
        assert!(s.count.is_some());
        assert_eq!(s.format, "item-%d");
        assert_eq!(
            serde_json::from_value::<Sequence>(serde_json::to_value(&s).unwrap()).unwrap(),
            s
        );
    }
}
