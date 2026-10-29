//! Read tools over valken-core: objects, lists, events, logs, rollout
//! history.  The application supplies cluster access through [`Clusters`].

pub mod summary;
mod tools;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use valken_core::k8s::table::{discover, ApiCatalog, ApiResourceInfo};

pub use tools::{GetEvents, GetLogs, GetObject, GetRolloutHistory, ListClusters, ListObjects};

use crate::tool::{Registry, ToolError};

/// One cluster open in Valken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClusterInfo {
    /// Kubeconfig context name: what tools take as `cluster`.
    pub name: String,
    /// `prod`, `staging`, `dev`, … (Valken's environment tag).
    pub env: String,
    /// The cluster on screen.
    pub focused: bool,
    pub connected: bool,
}

/// Cluster access the application provides.  Called from tool tasks, so it
/// must not touch UI state: keep a thread-safe copy.
pub trait Clusters: Send + Sync + 'static {
    fn list(&self) -> Vec<ClusterInfo>;
    fn client(&self, cluster: &str) -> Option<kube::Client>;

    fn focused(&self) -> Option<String> {
        self.list().into_iter().find(|c| c.focused).map(|c| c.name)
    }
}

/// Shared state for the read tools: cluster access plus a per-cluster API
/// discovery cache (so `deploy`, `po`, `certificate` resolve like kubectl).
pub struct Kube {
    clusters: Arc<dyn Clusters>,
    catalogs: Mutex<HashMap<String, (Instant, Arc<ApiCatalog>)>>,
}

/// How long a cluster's discovery is trusted, so a CRD installed
/// mid-session shows up without a reconnect.
const CATALOG_TTL: Duration = Duration::from_secs(10 * 60);

impl Kube {
    pub fn new(clusters: Arc<dyn Clusters>) -> Arc<Self> {
        Arc::new(Self {
            clusters,
            catalogs: Mutex::default(),
        })
    }

    pub fn clusters(&self) -> &Arc<dyn Clusters> {
        &self.clusters
    }

    /// `(name, client)` for `cluster`, defaulting to the focused cluster.
    pub fn client(&self, cluster: Option<&str>) -> Result<(String, kube::Client), ToolError> {
        let name = match cluster.filter(|c| !c.is_empty()) {
            Some(c) => c.to_owned(),
            None => self
                .clusters
                .focused()
                .ok_or_else(|| ToolError::NoCluster("(none focused)".into()))?,
        };
        let client = self
            .clusters
            .client(&name)
            .ok_or_else(|| ToolError::NoCluster(name.clone()))?;
        Ok((name, client))
    }

    /// Resolves `kind` (`deploy`, `pods`, `Certificate`) on `cluster`.
    pub async fn resolve(
        &self,
        cluster: &str,
        client: &kube::Client,
        kind: &str,
    ) -> Result<ApiResourceInfo, ToolError> {
        let cached = self.catalogs.lock().ok().and_then(|c| {
            c.get(cluster)
                .filter(|(at, _)| at.elapsed() < CATALOG_TTL)
                .map(|(_, c)| c.clone())
        });
        let catalog = match cached {
            Some(c) => c,
            None => {
                let c = Arc::new(discover(client).await.map_err(ToolError::failed)?);
                if let Ok(mut map) = self.catalogs.lock() {
                    map.insert(cluster.to_owned(), (Instant::now(), c.clone()));
                }
                c
            }
        };
        let info = catalog
            .resolve(&kind.to_ascii_lowercase())
            .cloned()
            .ok_or_else(|| ToolError::NotFound(format!("no kind `{kind}` on {cluster}")))?;
        if info.kind == "Secret" {
            return Err(ToolError::Forbidden(
                "Secrets are never readable by the agent".into(),
            ));
        }
        Ok(info)
    }

    /// Forgets a cluster's discovery (it disconnected or CRDs changed).
    pub fn forget(&self, cluster: &str) {
        if let Ok(mut map) = self.catalogs.lock() {
            map.remove(cluster);
        }
    }
}

/// Registers every read tool in this module.
pub fn register(registry: &mut Registry, kube: &Arc<Kube>) {
    registry
        .register(ListClusters(kube.clone()))
        .register(GetObject(kube.clone()))
        .register(ListObjects(kube.clone()))
        .register(GetEvents(kube.clone()))
        .register(GetLogs(kube.clone()))
        .register(GetRolloutHistory(kube.clone()));
}
