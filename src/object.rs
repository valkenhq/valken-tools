//! Object references: what citations, context chips and audit targets point at.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One Kubernetes object in one cluster.
///
/// `kind` is the lowercase singular (`deployment`, `pod`); `namespace` is
/// `None` for cluster-scoped kinds.  `cluster` is the kubeconfig context name,
/// the same id the app uses for its cluster sessions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct ObjectRef {
    /// Kubeconfig context name of the cluster.
    pub cluster: String,
    /// Lowercase singular kind, e.g. `deployment`.
    pub kind: String,
    /// Namespace; omit for cluster-scoped kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub name: String,
}

impl ObjectRef {
    pub fn new(
        cluster: impl Into<String>,
        kind: impl Into<String>,
        namespace: Option<&str>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            cluster: cluster.into(),
            kind: kind.into().to_ascii_lowercase(),
            namespace: namespace.filter(|ns| !ns.is_empty()).map(str::to_owned),
            name: name.into(),
        }
    }

    /// `storefront/checkout`, or just the name when cluster-scoped: the chip text.
    pub fn short(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{ns}/{}", self.name),
            None => self.name.clone(),
        }
    }

    /// The Secret kind is never readable by any tool.
    pub fn is_secret(&self) -> bool {
        self.kind == "secret" || self.kind == "secrets"
    }
}

/// `deployment storefront/checkout @prod-eu`: the audit/trail form.
impl fmt::Display for ObjectRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} @{}", self.kind, self.short(), self.cluster)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_kind_and_empty_namespace() {
        let r = ObjectRef::new("kind-a", "Deployment", Some(""), "web");
        assert_eq!(r.kind, "deployment");
        assert_eq!(r.namespace, None);
        assert_eq!(r.short(), "web");
    }

    #[test]
    fn display_and_short() {
        let r = ObjectRef::new("prod-eu", "deployment", Some("storefront"), "checkout");
        assert_eq!(r.short(), "storefront/checkout");
        assert_eq!(r.to_string(), "deployment storefront/checkout @prod-eu");
    }

    #[test]
    fn secret_detection() {
        assert!(ObjectRef::new("c", "Secret", Some("a"), "b").is_secret());
        assert!(!ObjectRef::new("c", "configmap", Some("a"), "b").is_secret());
    }

    #[test]
    fn serde_omits_missing_namespace() {
        let r = ObjectRef::new("c", "node", None, "n1");
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("namespace"));
        assert_eq!(serde_json::from_str::<ObjectRef>(&json).unwrap(), r);
    }
}
