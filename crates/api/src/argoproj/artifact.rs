//! Argo Workflows artifact types (TODO **T7.3 Scope A**, Q30).
//!
//! 1:1 typed port of the artifact block in upstream `workflow_types.go`
//! (`Artifact`, `Artifacts`, `ArtifactLocation`, `ArtifactRepositoryRef(+Status)`,
//! `ArchiveStrategy` + tar/zip/none strategies). Field-level SSOT is the Go
//! json tags; wire format is JSON-only (decision **Q10**). Data-only port:
//! Go embeds `ArtifactLocation` with `json:",inline"`, mirrored here with
//! `#[serde(flatten)]`; the Go `ArtifactLocationType` interface methods
//! (`HasLocation`/`GetKey`/`SetKey`) and path-cleaning helpers are not carried.

use serde::{Deserialize, Serialize};

use super::http::HTTPArtifact;
use super::is_false;
use super::lifecycle::ArtifactGC;
use super::repo::ArtifactRepository;
use super::stores::{
    ArtifactoryArtifact, AzureArtifact, GCSArtifact, GitArtifact, HDFSArtifact, OSSArtifact,
    PluginArtifact, RawArtifact, S3Artifact,
};

/// `Artifact` — indicates an artifact to place at a specified path.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    /// Name of the artifact; must be unique within a template's inputs/outputs
    /// (always serialized).
    #[serde(default)]
    pub name: String,
    /// Container path to the artifact.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// Mode bits to use on this file, a value between 0 and 0777 (set when
    /// loading input artifacts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<i32>,
    /// Allows an artifact to reference an artifact from a previous step.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    /// Location of the artifact (Go `json:",inline"` embed).
    #[serde(flatten)]
    pub artifact_location: ArtifactLocation,
    /// Exports an output artifact to the global scope, making it available as
    /// `workflow.outputs.artifacts.XXXX` and in `workflow.status.outputs`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub global_name: String,
    /// Controls how the artifact will be saved to the artifact repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<ArchiveStrategy>,
    /// Make artifacts optional, if artifacts don't generate or exist.
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
    /// Allows an artifact to be sourced from a subpath within the specified
    /// source.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sub_path: String,
    /// If mode is set, apply the permission recursively into the artifact if
    /// it is a folder.
    #[serde(default, skip_serializing_if = "is_false")]
    pub recurse_mode: bool,
    /// If defined, is evaluated to specify the value for the artifact.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from_expression: String,
    /// Strategy to use when deleting an artifact from completed or deleted
    /// workflows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "artifactGC")]
    pub artifact_gc: Option<ArtifactGC>,
    /// Has this been deleted?
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
}

/// `Artifacts` — an ordered list of [`Artifact`] (Go `type Artifacts []Artifact`).
pub type Artifacts = Vec<Artifact>;

/// `ArtifactLocation` — describes a location for a single or multiple
/// artifacts; exactly one of the driver-specific pointers is typically set.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactLocation {
    /// Indicates if the container logs should be archived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_logs: Option<bool>,
    /// S3 artifact location details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Artifact>,
    /// Git artifact location details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitArtifact>,
    /// HTTP artifact location details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HTTPArtifact>,
    /// Artifactory artifact location details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifactory: Option<ArtifactoryArtifact>,
    /// HDFS artifact location details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdfs: Option<HDFSArtifact>,
    /// Raw artifact location details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<RawArtifact>,
    /// OSS artifact location details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oss: Option<OSSArtifact>,
    /// GCS artifact location details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gcs: Option<GCSArtifact>,
    /// Azure Storage artifact location details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub azure: Option<AzureArtifact>,
    /// Plugin artifact location details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginArtifact>,
}

/// `ArchiveStrategy` — describes how to archive files/directories when saving
/// artifacts.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveStrategy {
    /// Tar and gzip the file or directory when saving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tar: Option<TarStrategy>,
    /// Skip the tar process and upload the files or directory tree as
    /// independent files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub none: Option<NoneStrategy>,
    /// Unzip zipped input artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zip: Option<ZipStrategy>,
}

/// `TarStrategy` — tar and gzip the file or directory when saving.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TarStrategy {
    /// Gzip compression level for the artifact (defaults to
    /// `gzip.DefaultCompression`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression_level: Option<i32>,
}

/// `ZipStrategy` — unzip zipped input artifacts.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ZipStrategy {}

/// `NoneStrategy` — skip the tar process and upload the files or directory
/// tree as independent files. If the artifact is a directory, the artifact
/// driver must support saving/loading the directory appropriately.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NoneStrategy {}

/// `ArtifactRepositoryRef` — a reference to a ConfigMap holding the artifact
/// repository configuration.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRepositoryRef {
    /// The name of the config map. Defaults to "artifact-repositories".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub config_map: String,
    /// The config map key. Defaults to the value of the
    /// "workflows.argoproj.io/default-artifact-repository" annotation.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
}

/// `ArtifactRepositoryRefStatus` — the resolved artifact repository reference
/// with namespace info.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRepositoryRefStatus {
    /// The referenced config map + key (Go `json:",inline"` embed).
    #[serde(flatten)]
    pub artifact_repository_ref: ArtifactRepositoryRef,
    /// The namespace of the config map. Defaults to the workflow's namespace,
    /// or the controller's namespace (if found).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    /// If this ref represents the default artifact repository, rather than a
    /// config map.
    #[serde(default, skip_serializing_if = "is_false")]
    pub default: bool,
    /// The repository the workflow will use. This may be empty before v3.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_repository: Option<ArtifactRepository>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn artifact_serializes_camel_case_and_drops_absent_optionals() {
        let mut artifact = Artifact {
            name: "my-artifact".to_string(),
            ..Default::default()
        };
        artifact.artifact_location.s3 = Some(S3Artifact {
            key: "my/key.tar.gz".to_string(),
            ..Default::default()
        });
        let value = serde_json::to_value(&artifact).expect("serialize Artifact");
        assert_eq!(
            value,
            json!({
                "name": "my-artifact",
                "s3": { "key": "my/key.tar.gz" },
            })
        );
    }

    #[test]
    fn artifact_location_archive_logs_only_parses() {
        let loc: ArtifactLocation =
            serde_json::from_str(r#"{"archiveLogs":true}"#).expect("parse ArtifactLocation");
        assert_eq!(loc.archive_logs, Some(true));
        assert!(loc.s3.is_none());
        assert!(loc.git.is_none());
        assert!(loc.http.is_none());
        assert!(loc.plugin.is_none());
    }
}
