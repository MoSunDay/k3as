//! Argo Workflows artifact store/location types (TODO **T7.3 Scope A**, Q30).
//!
//! 1:1 typed port of the artifact-backing types in upstream
//! `workflow_types.go` (S3/Git/Artifactory/Azure/HDFS/Raw/GCS/OSS/plugin
//! buckets and artifacts). Field-level SSOT is the Go json tags; wire format
//! is JSON-only (decision **Q10**). Data-only port: Go embeds the bucket
//! structs with `json:",inline"`, mirrored here with `#[serde(flatten)]`; the
//! Go `Artifact` interface methods (`GetKey`/`SetKey`/`HasLocation`) are not
//! carried.

use k8s_openapi::api::core::v1 as apiv1;
use serde::{Deserialize, Serialize};

use super::is_false;
use super::is_zero_i32;
use super::phases::ArtifactPluginName;

/// `S3Bucket` — access information for an S3 (or S3-compatible) bucket.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct S3Bucket {
    /// Hostname of the bucket endpoint.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
    /// Name of the bucket.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bucket: String,
    /// Optional bucket region.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub region: String,
    /// Connect to the service without TLS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insecure: Option<bool>,
    /// Secret selector for the bucket's access key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_secret: Option<apiv1::SecretKeySelector>,
    /// Secret selector for the bucket's secret key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key_secret: Option<apiv1::SecretKeySelector>,
    /// Ephemeral credentials (IAM assume role / S3 access grant) token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token_secret: Option<apiv1::SecretKeySelector>,
    /// Amazon Resource Name (ARN) of the role to assume.
    #[serde(default, skip_serializing_if = "String::is_empty", rename = "roleARN")]
    pub role_arn: String,
    /// Derive credentials from the SDK defaults.
    #[serde(default, skip_serializing_if = "is_false", rename = "useSDKCreds")]
    pub use_sdk_creds: bool,
    /// Attempt bucket creation for output artifacts when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_bucket_if_not_present: Option<CreateS3BucketOptions>,
    /// Server-side encryption settings applied to the bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption_options: Option<S3EncryptionOptions>,
    /// Secret containing the CA used to verify the endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_secret: Option<apiv1::SecretKeySelector>,
    /// S3 addressing style (`virtual` / `path`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub addressing_style: String,
}

/// `S3EncryptionOptions` — server-side encryption for an S3 bucket.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct S3EncryptionOptions {
    /// KMS key ID (SSE-KMS when set, else SSE-S3).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kms_key_id: String,
    /// JSON-serialized KMS encryption context.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kms_encryption_context: String,
    /// Enable server-side encryption.
    #[serde(default, skip_serializing_if = "is_false")]
    pub enable_encryption: bool,
    /// Secret selector for the SSE-C customer key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_side_customer_key_secret: Option<apiv1::SecretKeySelector>,
}

/// `CreateS3BucketOptions` — options when auto-creating an S3 bucket.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CreateS3BucketOptions {
    /// Enable object locking on the created bucket.
    #[serde(default, skip_serializing_if = "is_false")]
    pub object_locking: bool,
}

/// `S3Artifact` — the location of an S3 artifact.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct S3Artifact {
    /// Inline bucket access configuration (Go `json:",inline"`).
    #[serde(flatten)]
    pub s3_bucket: S3Bucket,
    /// Key (path) of the artifact in the bucket.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
}

/// `GitArtifact` — a git repository placed as an input artifact.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GitArtifact {
    /// Repository URL (always serialized).
    #[serde(default)]
    pub repo: String,
    /// Revision to check out.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub revision: String,
    /// Shallow-clone depth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u64>,
    /// Additional refs to fetch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fetch: Vec<String>,
    /// Secret selector for the repo username.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username_secret: Option<apiv1::SecretKeySelector>,
    /// Secret selector for the repo password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_secret: Option<apiv1::SecretKeySelector>,
    /// Secret selector for the SSH private key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_private_key_secret: Option<apiv1::SecretKeySelector>,
    /// Skip host-key verification.
    #[serde(default, skip_serializing_if = "is_false")]
    pub insecure_ignore_host_key: bool,
    /// Disable git submodules.
    #[serde(default, skip_serializing_if = "is_false")]
    pub disable_submodules: bool,
    /// Clone a single branch only.
    #[serde(default, skip_serializing_if = "is_false")]
    pub single_branch: bool,
    /// Branch to clone (with `singleBranch`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub branch: String,
    /// Skip TLS verification.
    #[serde(default, skip_serializing_if = "is_false")]
    #[serde(rename = "insecureSkipTLS")]
    pub insecure_skip_tls: bool,
}

/// `ArtifactoryAuth` — secret selectors for Artifactory authentication.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactoryAuth {
    /// Secret selector for the repository username.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username_secret: Option<apiv1::SecretKeySelector>,
    /// Secret selector for the repository password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_secret: Option<apiv1::SecretKeySelector>,
}

/// `ArtifactoryArtifact` — the location of an Artifactory artifact.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactoryArtifact {
    /// URL of the artifact (always serialized).
    #[serde(default)]
    pub url: String,
    /// Inline credentials (Go `json:",inline"`).
    #[serde(flatten)]
    pub artifactory_auth: ArtifactoryAuth,
}

/// `AzureBlobContainer` — access information for an Azure Blob container.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AzureBlobContainer {
    /// Service URL, e.g. `https://<ACCOUNT>.blob.core.windows.net`.
    #[serde(default)]
    pub endpoint: String,
    /// Container where resources are stored.
    #[serde(default)]
    pub container: String,
    /// Secret selector for the account access key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_key_secret: Option<apiv1::SecretKeySelector>,
    /// Derive credentials from the SDK defaults.
    #[serde(default, skip_serializing_if = "is_false", rename = "useSDKCreds")]
    pub use_sdk_creds: bool,
}

/// `AzureArtifact` — the location of an Azure Storage artifact.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AzureArtifact {
    /// Inline container access configuration (Go `json:",inline"`).
    #[serde(flatten)]
    pub azure_blob_container: AzureBlobContainer,
    /// Blob name (path) of the artifact in the container.
    #[serde(default)]
    pub blob: String,
}

/// `HDFSArtifact` — the location of an HDFS artifact.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HDFSArtifact {
    /// Inline HDFS configuration (Go `json:",inline"`).
    #[serde(flatten)]
    pub hdfs_config: HDFSConfig,
    /// Path of the artifact in HDFS.
    #[serde(default)]
    pub path: String,
    /// Overwrite existing artifacts.
    #[serde(default, skip_serializing_if = "is_false")]
    pub force: bool,
}

/// `HDFSConfig` — HDFS connection configuration (Kerberos + addresses).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HDFSConfig {
    /// Inline Kerberos configuration (Go `json:",inline"`).
    #[serde(flatten)]
    pub hdfs_krb_config: HDFSKrbConfig,
    /// Namenode addresses (`host:port`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    /// HDFS user to impersonate.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hdfs_user: String,
    /// Data transfer protection level.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub data_transfer_protection: String,
}

/// `HDFSKrbConfig` — Kerberos configuration for HDFS access.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HDFSKrbConfig {
    /// Secret selector for the Kerberos credentials cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "krbCCacheSecret")]
    pub krb_ccache_secret: Option<apiv1::SecretKeySelector>,
    /// Secret selector for the Kerberos keytab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub krb_keytab_secret: Option<apiv1::SecretKeySelector>,
    /// Kerberos username.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub krb_username: String,
    /// Kerberos realm.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub krb_realm: String,
    /// ConfigMap selector for the krb5.conf.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub krb_config_config_map: Option<apiv1::ConfigMapKeySelector>,
    /// Kerberos service principal name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub krb_service_principal_name: String,
}

/// `RawArtifact` — literal artifact data placed inline.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RawArtifact {
    /// Raw artifact data (always serialized).
    #[serde(default)]
    pub data: String,
}

/// `GCSBucket` — access information for a GCS bucket.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GCSBucket {
    /// Name of the bucket.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bucket: String,
    /// Secret selector for the service account key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_account_key_secret: Option<apiv1::SecretKeySelector>,
}

/// `GCSArtifact` — the location of a GCS artifact.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GCSArtifact {
    /// Inline bucket access configuration (Go `json:",inline"`).
    #[serde(flatten)]
    pub gcs_bucket: GCSBucket,
    /// Key (path) of the artifact in the bucket (always serialized).
    #[serde(default)]
    pub key: String,
}

/// `OSSBucket` — access information for an Aliyun OSS bucket.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OSSBucket {
    /// Hostname of the bucket endpoint.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
    /// Name of the bucket.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bucket: String,
    /// Secret selector for the bucket's access key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_secret: Option<apiv1::SecretKeySelector>,
    /// Secret selector for the bucket's secret key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key_secret: Option<apiv1::SecretKeySelector>,
    /// Attempt bucket creation for output artifacts when absent.
    #[serde(default, skip_serializing_if = "is_false")]
    pub create_bucket_if_not_present: bool,
    /// User's temporary security token.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub security_token: String,
    /// Bucket lifecycle management rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_rule: Option<OSSLifecycleRule>,
    /// Derive credentials from the SDK defaults.
    #[serde(default, skip_serializing_if = "is_false", rename = "useSDKCreds")]
    pub use_sdk_creds: bool,
}

/// `OSSArtifact` — the location of an OSS artifact.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OSSArtifact {
    /// Inline bucket access configuration (Go `json:",inline"`).
    #[serde(flatten)]
    pub oss_bucket: OSSBucket,
    /// Key (path) of the artifact in the bucket (always serialized).
    #[serde(default)]
    pub key: String,
}

/// `OSSLifecycleRule` — lifecycle management for an OSS bucket.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OSSLifecycleRule {
    /// Days after which objects become infrequent-access storage.
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub mark_infrequent_access_after_days: i32,
    /// Days after which objects are deleted.
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub mark_deletion_after_days: i32,
}

/// `PluginArtifact` — an artifact handled by an out-of-tree artifact plugin.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PluginArtifact {
    /// Plugin executable name (see [`super::phases::ArtifactPluginName`]).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: ArtifactPluginName,
    /// Plugin configuration (free-form).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub configuration: String,
    /// Plugin connection timeout in seconds (default 5 in Go).
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub connection_timeout_seconds: i32,
    /// Key (path) of the artifact (always serialized).
    #[serde(default)]
    pub key: String,
}
