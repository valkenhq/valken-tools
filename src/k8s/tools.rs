use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use valken_core::k8s::events::list_events;
use valken_core::k8s::logs::{open_logs, LogRequest};
use valken_core::k8s::objects::{get_object, list_objects_with, ListParams, ObjectKey};
use valken_core::k8s::rollout::history;
use valken_core::k8s::{LogError, ObjectError};

use super::summary::summarize;
use super::Kube;
use crate::object::ObjectRef;
use crate::tool::{Access, Cx, Tier, Tool, ToolError, ToolOutput, ToolSpec};

/// Longest `get_object` body; bigger objects are cut with a marker.
const MAX_OBJECT_CHARS: usize = 24_000;
const MAX_LOG_LINES: usize = 500;
const DEFAULT_LOG_LINES: usize = 100;
const DEFAULT_LIST: usize = 50;
const MAX_LIST: usize = 300;

fn object_error(e: ObjectError, what: &str) -> ToolError {
    match e {
        ObjectError::NotFound => ToolError::NotFound(what.to_owned()),
        ObjectError::Forbidden(m) => ToolError::Forbidden(m),
        e => ToolError::failed(e),
    }
}

fn opt(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|s| !s.is_empty())
}

// ── list_clusters ──────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoInput {}

pub struct ListClusters(pub Arc<Kube>);

#[async_trait]
impl Tool for ListClusters {
    type Input = NoInput;

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_clusters",
            description: "List the clusters open in Valken (kubeconfig context names, environment, which one is on screen). \
                Call this when the question involves more than one cluster or you need a cluster name for another tool.",
            access: Access::Read,
            tier: Tier::Free,
        }
    }

    fn target(&self, _: &NoInput) -> String {
        "clusters".into()
    }

    async fn run(&self, _cx: Cx<'_>, _: NoInput) -> Result<ToolOutput, ToolError> {
        let list = self.0.clusters().list();
        Ok(ToolOutput::json(&list).note(format!("{} clusters", list.len())))
    }
}

// ── get_object ─────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObjectInput {
    /// Kubeconfig context name of the cluster; omit for the cluster on screen.
    #[serde(default)]
    pub cluster: Option<String>,
    /// Kind, plural or short name as kubectl accepts it: `deployment`, `po`, `ingresses`, `certificate`.
    pub kind: String,
    /// Namespace; omit for cluster-scoped kinds.
    #[serde(default)]
    pub namespace: Option<String>,
    pub name: String,
}

pub struct GetObject(pub Arc<Kube>);

#[async_trait]
impl Tool for GetObject {
    type Input = ObjectInput;

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_object",
            description: "Get one Kubernetes object's live manifest (spec and status) as JSON. Env var values and noisy fields are masked; \
                Secrets can't be read. Call this when you need exact spec fields: resources, probes, images, selectors, conditions.",
            access: Access::Read,
            tier: Tier::Free,
        }
    }

    fn target(&self, i: &ObjectInput) -> String {
        short(opt(&i.namespace), &i.name)
    }

    async fn run(&self, cx: Cx<'_>, i: ObjectInput) -> Result<ToolOutput, ToolError> {
        let (cluster, client) = self.0.client(opt(&i.cluster))?;
        let info = self.0.resolve(&cluster, &client, &i.kind).await?;
        let key = ObjectKey {
            api_version: info.api_version(),
            kind: info.kind.clone(),
            namespace: opt(&i.namespace).map(str::to_owned),
            name: i.name.clone(),
        };
        let mut obj = get_object(&client, &cluster, &key).await.map_err(|e| object_error(e, &key.name))?;
        let masked = cx.redactor.object(&mut obj);
        let mut body = serde_json::to_string(&obj).map_err(ToolError::failed)?;
        let mut note = format!("{} chars", body.len());
        if body.len() > MAX_OBJECT_CHARS {
            let cut = (0..=MAX_OBJECT_CHARS).rev().find(|&i| body.is_char_boundary(i)).unwrap_or(0);
            body.truncate(cut);
            body.push_str("…[truncated]");
            note = "truncated".into();
        }
        if masked > 0 {
            note.push_str(&format!(" · {masked} env masked"));
        }
        let cite = ObjectRef::new(cluster, &info.kind, opt(&i.namespace).filter(|_| info.namespaced), &i.name);
        Ok(ToolOutput::text(body).cite(cite).note(note))
    }
}

fn short(ns: Option<&str>, name: &str) -> String {
    match ns {
        Some(ns) => format!("{ns}/{name}"),
        None => name.to_owned(),
    }
}

// ── list_objects ───────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListInput {
    /// Kubeconfig context name of the cluster; omit for the cluster on screen.
    #[serde(default)]
    pub cluster: Option<String>,
    /// Kind, plural or short name: `pods`, `deploy`, `nodes`, `certificates`.
    pub kind: String,
    /// Namespace; omit for all namespaces (or cluster-scoped kinds).
    #[serde(default)]
    pub namespace: Option<String>,
    /// Label selector, e.g. `app=checkout,tier!=canary`.
    #[serde(default)]
    pub label_selector: Option<String>,
    /// Maximum rows (default 50, at most 300).
    #[serde(default)]
    pub limit: Option<usize>,
}

pub struct ListObjects(pub Arc<Kube>);

#[async_trait]
impl Tool for ListObjects {
    type Input = ListInput;

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_objects",
            description: "List objects of one kind as compact rows (like `kubectl get`: ready, status, restarts, images, age). \
                Call this to find which objects exist or which are unhealthy before reading any one in full.",
            access: Access::Read,
            tier: Tier::Free,
        }
    }

    fn target(&self, i: &ListInput) -> String {
        match opt(&i.namespace) {
            Some(ns) => format!("{} in {ns}", i.kind),
            None => i.kind.clone(),
        }
    }

    async fn run(&self, _cx: Cx<'_>, i: ListInput) -> Result<ToolOutput, ToolError> {
        let (cluster, client) = self.0.client(opt(&i.cluster))?;
        let info = self.0.resolve(&cluster, &client, &i.kind).await?;
        let limit = i.limit.unwrap_or(DEFAULT_LIST).clamp(1, MAX_LIST);
        let mut params = ListParams::default().limit(limit as u32);
        if let Some(sel) = opt(&i.label_selector) {
            params = params.labels(sel);
        }
        let items = list_objects_with(&client, &cluster, &info.api_version(), &info.kind, opt(&i.namespace), &params)
            .await
            .map_err(|e| object_error(e, &i.kind))?;
        let now = chrono::Utc::now();
        let more = items.len() >= limit;
        // LIST items carry no `kind`; the summary needs it.
        let rows: Vec<Value> = items
            .into_iter()
            .map(|mut o| {
                o["kind"] = info.kind.clone().into();
                let mut row = summarize(&o, now);
                row["kind"] = info.kind.clone().into();
                row
            })
            .collect();
        Ok(ToolOutput::json(&json!({"cluster": cluster, "kind": info.kind, "rows": rows, "more": more}))
            .note(format!("{} rows{}", rows.len(), if more { "+" } else { "" })))
    }
}

// ── get_events ─────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventsInput {
    /// Kubeconfig context name of the cluster; omit for the cluster on screen.
    #[serde(default)]
    pub cluster: Option<String>,
    pub namespace: String,
    /// Only events about this object: its kind (`Pod`, `Deployment`) …
    #[serde(default)]
    pub kind: Option<String>,
    /// … and name.
    #[serde(default)]
    pub name: Option<String>,
    /// Maximum events, newest first (default 30).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Only `Warning` events.
    #[serde(default)]
    pub warnings_only: bool,
}

pub struct GetEvents(pub Arc<Kube>);

#[async_trait]
impl Tool for GetEvents {
    type Input = EventsInput;

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_events",
            description: "Get Kubernetes events in a namespace, newest first, optionally only those about one object. \
                Call this first when something is crashing, pending, restarting or failing to roll out: OOMKilled, BackOff, \
                FailedScheduling and probe failures show up here.",
            access: Access::Read,
            tier: Tier::Free,
        }
    }

    fn target(&self, i: &EventsInput) -> String {
        short(Some(&i.namespace), opt(&i.name).unwrap_or("*"))
    }

    async fn run(&self, _cx: Cx<'_>, i: EventsInput) -> Result<ToolOutput, ToolError> {
        let (cluster, client) = self.0.client(opt(&i.cluster))?;
        let involved = match (opt(&i.kind), opt(&i.name)) {
            (Some(kind), Some(name)) => Some((canonical_kind(kind), name)),
            _ => None,
        };
        let rows = list_events(&client, &i.namespace, involved.as_ref().map(|(k, n)| (k.as_str(), *n)))
            .await
            .map_err(|e| object_error(e, &i.namespace))?;
        let rows: Vec<Value> = rows
            .into_iter()
            .filter(|r| !i.warnings_only || r.is_warning())
            .take(i.limit.unwrap_or(30).clamp(1, 200))
            .map(|r| {
                json!({"time": r.time, "type": r.kind, "reason": r.reason, "object": format!("{}/{}", r.object_kind, r.object_name),
                    "count": r.count, "message": r.message})
            })
            .collect();
        let cites = involved
            .map(|(kind, name)| ObjectRef::new(cluster.clone(), kind, Some(&i.namespace), name))
            .into_iter();
        Ok(ToolOutput::json(&rows).cites(cites).note(format!("{} events", rows.len())))
    }
}

/// `deployment` → `Deployment`, the casing `involvedObject.kind` uses.
fn canonical_kind(kind: &str) -> String {
    match kind.to_ascii_lowercase().as_str() {
        "po" | "pod" | "pods" => "Pod".into(),
        "deploy" | "deployment" | "deployments" => "Deployment".into(),
        "sts" | "statefulset" | "statefulsets" => "StatefulSet".into(),
        "ds" | "daemonset" | "daemonsets" => "DaemonSet".into(),
        "rs" | "replicaset" | "replicasets" => "ReplicaSet".into(),
        "svc" | "service" | "services" => "Service".into(),
        "no" | "node" | "nodes" => "Node".into(),
        "job" | "jobs" => "Job".into(),
        "cj" | "cronjob" | "cronjobs" => "CronJob".into(),
        "ing" | "ingress" | "ingresses" => "Ingress".into(),
        "pvc" | "persistentvolumeclaim" => "PersistentVolumeClaim".into(),
        _ => {
            let mut c = kind.chars();
            c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
        }
    }
}

// ── get_logs ───────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogsInput {
    /// Kubeconfig context name of the cluster; omit for the cluster on screen.
    #[serde(default)]
    pub cluster: Option<String>,
    pub namespace: String,
    pub pod: String,
    /// Container name; omit for the pod's first container.
    #[serde(default)]
    pub container: Option<String>,
    /// Logs of the previous (crashed) container instead of the running one.
    #[serde(default)]
    pub previous: bool,
    /// Last N lines (default 100, at most 500).
    #[serde(default)]
    pub tail: Option<usize>,
    /// Only the last N seconds.
    #[serde(default)]
    pub since_seconds: Option<i64>,
    /// Only lines containing this text (case-insensitive).
    #[serde(default)]
    pub contains: Option<String>,
}

pub struct GetLogs(pub Arc<Kube>);

#[async_trait]
impl Tool for GetLogs {
    type Input = LogsInput;

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_logs",
            description: "Get a pod container's recent log lines (tokens and keys masked). Use previous=true for a crashlooping \
                container: the crash reason is in the previous instance's last lines. Find pod names with list_objects first.",
            access: Access::Read,
            tier: Tier::Free,
        }
    }

    fn target(&self, i: &LogsInput) -> String {
        let prev = if i.previous { " --previous" } else { "" };
        format!("{}{prev}", short(Some(&i.namespace), &i.pod))
    }

    async fn run(&self, cx: Cx<'_>, i: LogsInput) -> Result<ToolOutput, ToolError> {
        if !cx.redactor.rules().allow_logs {
            return Err(ToolError::Forbidden("log lines are turned off in Settings → Agent → Privacy".into()));
        }
        let (cluster, client) = self.0.client(opt(&i.cluster))?;
        let mut others = Vec::new();
        let container = match opt(&i.container) {
            Some(c) => c.to_owned(),
            None => {
                let key = ObjectKey { api_version: "v1".into(), kind: "Pod".into(), namespace: Some(i.namespace.clone()), name: i.pod.clone() };
                let pod = get_object(&client, &cluster, &key).await.map_err(|e| object_error(e, &i.pod))?;
                let mut names = pod["spec"]["containers"].as_array().into_iter().flatten().filter_map(|c| c["name"].as_str());
                let first = names.next().ok_or_else(|| ToolError::NotFound("pod has no containers".into()))?.to_owned();
                others = names.map(str::to_owned).collect();
                first
            }
        };
        let tail = i.tail.unwrap_or(DEFAULT_LOG_LINES).clamp(1, MAX_LOG_LINES);
        let req = LogRequest {
            namespace: i.namespace.clone(),
            pod: i.pod.clone(),
            container: container.clone(),
            since_seconds: i.since_seconds,
            previous: i.previous,
            follow: false,
            // Filtering happens client-side: read more so the filter has something to keep.
            tail_lines: Some(if i.contains.is_some() { (tail * 10).min(5000) } else { tail } as i64),
        };
        let mut stream = open_logs(&client, &req).await.map_err(|e| match e {
            LogError::NoPrevious => ToolError::NotFound("no previous container: it hasn't restarted".into()),
            LogError::NotFound => ToolError::NotFound(i.pod.clone()),
            LogError::Forbidden(m) => ToolError::Forbidden(m),
            e => ToolError::failed(e),
        })?;
        let needle = opt(&i.contains).map(str::to_lowercase);
        let mut lines = Vec::new();
        while let Some(line) = stream.next().await {
            let Ok(line) = line else { break };
            if needle.as_ref().is_none_or(|n| line.text.to_lowercase().contains(n)) {
                lines.push(line.text);
            }
        }
        let lines = &lines[lines.len().saturating_sub(tail)..];
        let mut head = format!("pod {} container {container}", i.pod);
        if i.previous {
            head.push_str(" (previous instance)");
        }
        if !others.is_empty() {
            head.push_str(&format!("; other containers: {}", others.join(", ")));
        }
        let body = format!("{head}\n{}", lines.join("\n"));
        Ok(ToolOutput::text(body)
            .cite(ObjectRef::new(cluster, "pod", Some(&i.namespace), &i.pod))
            .note(format!("{} lines", lines.len())))
    }
}

// ── get_rollout_history ────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RolloutInput {
    /// Kubeconfig context name of the cluster; omit for the cluster on screen.
    #[serde(default)]
    pub cluster: Option<String>,
    pub namespace: String,
    pub deployment: String,
}

pub struct GetRolloutHistory(pub Arc<Kube>);

#[async_trait]
impl Tool for GetRolloutHistory {
    type Input = RolloutInput;

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_rollout_history",
            description: "Get a Deployment's rollout revisions, newest first: images, when each was created, replicas, change cause. \
                Call this when something started failing recently, to see what changed and when.",
            access: Access::Read,
            tier: Tier::Free,
        }
    }

    fn target(&self, i: &RolloutInput) -> String {
        short(Some(&i.namespace), &i.deployment)
    }

    async fn run(&self, _cx: Cx<'_>, i: RolloutInput) -> Result<ToolOutput, ToolError> {
        let (cluster, client) = self.0.client(opt(&i.cluster))?;
        let revs = history(&client, &i.namespace, &i.deployment).await.map_err(|e| object_error(e, &i.deployment))?;
        let rows: Vec<Value> = revs
            .iter()
            .take(10)
            .map(|r| {
                json!({"revision": r.revision, "current": r.current, "replica_set": r.replica_set, "images": r.images,
                    "created": r.created, "replicas": r.replicas, "ready": r.ready, "change_cause": r.change_cause})
            })
            .collect();
        Ok(ToolOutput::json(&rows)
            .cite(ObjectRef::new(cluster, "deployment", Some(&i.namespace), &i.deployment))
            .note(format!("{} revisions", rows.len())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_kinds() {
        assert_eq!(canonical_kind("po"), "Pod");
        assert_eq!(canonical_kind("deployment"), "Deployment");
        assert_eq!(canonical_kind("certificate"), "Certificate");
        assert_eq!(canonical_kind(""), "");
    }

    #[test]
    fn inputs_reject_unknown_fields() {
        let ok: Result<ObjectInput, _> = serde_json::from_value(json!({"kind": "pod", "name": "x"}));
        assert!(ok.is_ok());
        let bad: Result<ObjectInput, _> = serde_json::from_value(json!({"kind": "pod", "name": "x", "verb": "delete"}));
        assert!(bad.is_err());
    }
}
