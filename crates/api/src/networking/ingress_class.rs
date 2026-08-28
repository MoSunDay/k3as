//! `networking.k8s.io/v1` `IngressClass` (workstream A, decision **Q32**).
//!
//! Typed port of upstream `k8s.io/api/networking/v1` `IngressClass` +
//! `IngressClassSpec` + the shared `TypedLocalObjectReference` shape.
//! `parameters` is not carried (documented reduction in [`super`]: the
//! built-in gateway is selected by `spec.controller`, never by an external
//! parameters object).

use kube_core::metadata::{ObjectMeta, TypeMeta};
use serde::{Deserialize, Serialize};

/// IngressClass represents the class of the Ingress.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IngressClass {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    /// Spec is the desired state of the IngressClass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<IngressClassSpec>,
}

/// IngressClassList is a collection of IngressClasses.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IngressClassList {
    #[serde(flatten)]
    pub types: TypeMeta,
    #[serde(default)]
    pub metadata: kube_core::metadata::ListMeta,
    #[serde(default)]
    pub items: Vec<IngressClass>,
}

/// IngressClassSpec provides information about the class of an Ingress.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IngressClassSpec {
    /// Controller refers to the name of the controller that should handle
    /// this class (e.g. `init-pro.io/ingress`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<String>,
}
