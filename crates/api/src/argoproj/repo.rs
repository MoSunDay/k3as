//! Argo Workflows artifact repository types (TODO **T7.3 Scope A**, Q30).
//!
//! 1:1 typed port of upstream `artifact_repository_types.go` — the controller
//! side configuration for where workflow artifacts are stored (`ArtifactRepository`
//! and its per-backend repository types). Field-level SSOT is the Go json
//! tags; wire format is JSON-only (decision **Q10**). Data-only port: the Go
//! `ArtifactRepositoryType` interface + `IntoArtifactLocation` methods are not
//! carried; the Go `json:",inline"` bucket/auth/config embeds are mirrored
//! with `#[serde(flatten)]`.

use serde::{Deserialize, Serialize};

use super::is_false;
use super::phases::ArtifactPluginName;
use super::stores::{
    ArtifactoryAuth, AzureBlobContainer, GCSBucket, HDFSConfig, OSSBucket, S3Bucket,
};

/// `DefaultArchivePattern` — default key pattern when storing artifacts in an
/// archive repository (Go `DefaultArchivePattern`).
pub const DEFAULT_ARCHIVE_PATTERN: &str = "{{workflow.name}}/{{pod.name}}";

/// `ArtifactRepository` — an artifact repository in which a controller will
/// store its artifacts; exactly one backend is typically configured.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRepository {
    /// Enables log archiving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_logs: Option<bool>,
    /// Stores artifacts in an S3-compliant object store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3ArtifactRepository>,
    /// Stores artifacts to JFrog Artifactory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifactory: Option<ArtifactoryArtifactRepository>,
    /// Stores artifacts in HDFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdfs: Option<HDFSArtifactRepository>,
    /// Stores artifacts in an OSS-compliant object store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oss: Option<OSSArtifactRepository>,
    /// Stores artifacts in a GCS object store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gcs: Option<GCSArtifactRepository>,
    /// Stores artifacts in an Azure Storage account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub azure: Option<AzureArtifactRepository>,
    /// Stores artifacts in a plugin-specific artifact repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginArtifactRepository>,
}

/// `S3ArtifactRepository` — controller configuration for an S3 artifact
/// repository.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct S3ArtifactRepository {
    /// Inline bucket access configuration (Go `json:",inline"`).
    #[serde(flatten)]
    pub s3_bucket: S3Bucket,
    /// Format of how to store keys; can reference workflow variables.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_format: String,
    /// Prefix used as part of the bucket key in which the controller will
    /// store artifacts.
    ///
    /// Deprecated: use `keyFormat` instead.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_prefix: String,
}

/// `OSSArtifactRepository` — controller configuration for an OSS artifact
/// repository.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OSSArtifactRepository {
    /// Inline bucket access configuration (Go `json:",inline"`).
    #[serde(flatten)]
    pub oss_bucket: OSSBucket,
    /// Format of how to store keys; can reference workflow variables.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_format: String,
}

/// `GCSArtifactRepository` — controller configuration for a GCS artifact
/// repository.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GCSArtifactRepository {
    /// Inline bucket access configuration (Go `json:",inline"`).
    #[serde(flatten)]
    pub gcs_bucket: GCSBucket,
    /// Format of how to store keys; can reference workflow variables.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_format: String,
}

/// `ArtifactoryArtifactRepository` — controller configuration for an
/// Artifactory artifact repository.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactoryArtifactRepository {
    /// Inline credentials (Go `json:",inline"`).
    #[serde(flatten)]
    pub artifactory_auth: ArtifactoryAuth,
    /// URL for the artifactory repo.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    #[serde(rename = "repoURL")]
    pub repo_url: String,
    /// Format of how to store keys; can reference workflow variables.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_format: String,
}

/// `AzureArtifactRepository` — controller configuration for an Azure Blob
/// Storage artifact repository.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AzureArtifactRepository {
    /// Inline container access configuration (Go `json:",inline"`).
    #[serde(flatten)]
    pub azure_blob_container: AzureBlobContainer,
    /// Format of how to store blob names; can reference workflow variables.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub blob_name_format: String,
}

/// `HDFSArtifactRepository` — controller configuration for an HDFS artifact
/// repository.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HDFSArtifactRepository {
    /// Inline HDFS configuration (Go `json:",inline"`).
    #[serde(flatten)]
    pub hdfs_config: HDFSConfig,
    /// Format of the path to store a file; can reference workflow variables.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path_format: String,
    /// Copies a file forcibly even if it exists.
    #[serde(default, skip_serializing_if = "is_false")]
    pub force: bool,
}

/// `PluginArtifactRepository` — controller configuration for a plugin artifact
/// repository.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PluginArtifactRepository {
    /// Plugin executable name (always serialized).
    #[serde(default)]
    pub name: ArtifactPluginName,
    /// Format of how to store keys; can reference workflow variables.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_format: String,
    /// Plugin configuration (always serialized).
    #[serde(default)]
    pub configuration: String,
}
