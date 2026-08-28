//! `networking.k8s.io/v1` `Ingress` (workstream A, decision **Q32**).
//!
//! 1:1 typed port of upstream `k8s.io/api/networking/v1` `types.go`
//! (`Ingress`, `IngressSpec`, `IngressRule`, `HTTPIngressPath`,
//! `IngressBackend`, `IngressTLS`, `IngressStatus`). Field SSOT is the Go
//! json tags; the openrusty Ingress gateway ([`crate::networking`]) folds
//! these objects into its route table.

use kube_core::metadata::{ObjectMeta, TypeMeta};
use serde::{Deserialize, Serialize};

/// Ingress is a collection of rules that allow inbound connections to reach
/// the endpoints defined by a backend.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Ingress {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    /// Spec is the desired state of the Ingress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<IngressSpec>,
    /// Status is the current state of the Ingress (written by the gateway).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<IngressStatus>,
}

/// IngressList is a collection of Ingress.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IngressList {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: kube_core::metadata::ListMeta,
    #[serde(default)]
    pub items: Vec<Ingress>,
}

/// IngressSpec describes the Ingress the user wishes to exist.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IngressSpec {
    /// IngressClassName is the name of an IngressClass cluster resource that
    /// defines which controller should implement the resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_class_name: Option<String>,
    /// DefaultBackend is the backend that should handle requests that do not
    /// match any rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_backend: Option<IngressBackend>,
    /// TLS configuration (parsed for parity; Q32 serves h2c only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<Vec<IngressTLS>>,
    /// A list of host rules used to configure the Ingress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<IngressRule>>,
}

/// IngressRule represents the rules mapping the paths under a specified host
/// to the related backend services.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IngressRule {
    /// Host is the fully qualified domain name of a network host
    /// (empty = match all hosts).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// HTTP represents a rule to apply against incoming requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HTTPIngressRuleValue>,
}

/// HTTPIngressRuleValue is a list of http selectors pointing to backends.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HTTPIngressRuleValue {
    /// A collection of paths that map requests to backends.
    #[serde(default)]
    pub paths: Vec<HTTPIngressPath>,
}

/// HTTPIngressPath associates a path with a backend.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HTTPIngressPath {
    /// Path is matched against the path of an incoming request
    /// (empty = "/"; interpretation depends on `pathType`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// PathType determines the interpretation of the Path matching
    /// (`Exact` / `Prefix` / `ImplementationSpecific`).
    #[serde(default)]
    pub path_type: String,
    /// Backend defines the referenced service endpoint to which the traffic
    /// will be forwarded to.
    #[serde(default)]
    pub backend: IngressBackend,
}

/// IngressBackend describes all endpoints for a given service and port.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IngressBackend {
    /// Service references a Service as a Backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<IngressServiceBackend>,
    /// Resource is an ObjectRef to another Kubernetes resource in the
    /// namespace of the Ingress object (not routed by the gateway).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<TypedLocalObjectReference>,
}

/// IngressServiceBackend references a Kubernetes Service as a Backend.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IngressServiceBackend {
    /// Name is the referenced service.
    #[serde(default)]
    pub name: String,
    /// Port of the referenced service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<ServiceBackendPort>,
}

/// ServiceBackendPort is the service port being referenced.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServiceBackendPort {
    /// Name is the name of the port on the Service.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Number is the numerical service port (gRPC) number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<i32>,
}

/// IngressTLS describes the transport layer security associated with an
/// Ingress (parity-only under Q32: h2c, no termination).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IngressTLS {
    /// Hosts are a list of hosts included in the TLS certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<String>>,
    /// SecretName is the name of the secret used to terminate TLS traffic.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub secret_name: String,
}

/// IngressStatus describe the current state of the Ingress.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IngressStatus {
    /// LoadBalancer contains the current status of the load-balancer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_balancer: Option<IngressLoadBalancerStatus>,
}

/// IngressLoadBalancerStatus represents the status of a load-balancer.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IngressLoadBalancerStatus {
    /// Ingress is a list containing ingress points for the load-balancer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<Vec<IngressLoadBalancerIngress>>,
}

/// IngressLoadBalancerIngress represents the status of a load-balancer
/// ingress point.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IngressLoadBalancerIngress {
    /// IP is set for load-balancer ingress points that are IP based.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ip: String,
    /// Hostname is set for load-balancer ingress points that are DNS based.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hostname: String,
}

/// TypedLocalObjectReference is an upstream
/// `k8s.io/api/core/v1.TypedLocalObjectReference` (re-declared here so the
/// data plane no longer depends on k8s-openapi; Q32).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TypedLocalObjectReference {
    /// APIGroup is the group for the resource being referenced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_group: Option<String>,
    /// Kind is the type of resource being referenced.
    #[serde(default)]
    pub kind: String,
    /// Name is the name of resource being referenced.
    #[serde(default)]
    pub name: String,
}
