//! Compact one-row summaries of objects for `list_objects`: what
//! `kubectl get` would show, so a list costs a few tokens per object.

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

/// `5m`, `3h`, `2d`: the age column.
pub fn age(created: Option<&str>, now: DateTime<Utc>) -> Option<String> {
    let t = DateTime::parse_from_rfc3339(created?).ok()?.with_timezone(&Utc);
    let secs = (now - t).num_seconds().max(0);
    Some(match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    })
}

/// One row for `obj`.  Unknown kinds get name / namespace / age / phase.
pub fn summarize(obj: &Value, now: DateTime<Utc>) -> Value {
    let meta = &obj["metadata"];
    let mut row = Map::new();
    row.insert("name".into(), meta["name"].clone());
    if let Some(ns) = meta["namespace"].as_str() {
        row.insert("namespace".into(), ns.into());
    }
    if let Some(a) = age(meta["creationTimestamp"].as_str(), now) {
        row.insert("age".into(), a.into());
    }
    let spec = &obj["spec"];
    let status = &obj["status"];
    match obj["kind"].as_str().unwrap_or_default() {
        "Pod" => {
            let statuses = status["containerStatuses"].as_array().map(Vec::as_slice).unwrap_or_default();
            let ready = statuses.iter().filter(|c| c["ready"] == true).count();
            let restarts: u64 = statuses.iter().filter_map(|c| c["restartCount"].as_u64()).sum();
            let waiting = statuses
                .iter()
                .filter_map(|c| c["state"]["waiting"]["reason"].as_str())
                .chain(statuses.iter().filter_map(|c| c["lastState"]["terminated"]["reason"].as_str()))
                .next();
            row.insert("ready".into(), format!("{ready}/{}", statuses.len()).into());
            row.insert("status".into(), waiting.map_or(status["phase"].clone(), Value::from));
            row.insert("restarts".into(), restarts.into());
            if let Some(node) = spec["nodeName"].as_str() {
                row.insert("node".into(), node.into());
            }
        }
        "Deployment" | "StatefulSet" | "ReplicaSet" => {
            let want = spec["replicas"].as_u64().unwrap_or(1);
            let ready = status["readyReplicas"].as_u64().unwrap_or(0);
            row.insert("ready".into(), format!("{ready}/{want}").into());
            row.insert("images".into(), images(&spec["template"]["spec"]));
        }
        "DaemonSet" => {
            let want = status["desiredNumberScheduled"].as_u64().unwrap_or(0);
            let ready = status["numberReady"].as_u64().unwrap_or(0);
            row.insert("ready".into(), format!("{ready}/{want}").into());
            row.insert("images".into(), images(&spec["template"]["spec"]));
        }
        "Service" => {
            row.insert("type".into(), spec["type"].clone());
            let ports: Vec<String> = spec["ports"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| format!("{}/{}", p["port"], p["protocol"].as_str().unwrap_or("TCP")))
                .collect();
            row.insert("ports".into(), ports.join(",").into());
        }
        "Node" => {
            let ready = status["conditions"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|c| c["type"] == "Ready")
                .map_or("Unknown", |c| if c["status"] == "True" { "Ready" } else { "NotReady" });
            row.insert("status".into(), ready.into());
            row.insert("version".into(), status["nodeInfo"]["kubeletVersion"].clone());
        }
        "Job" => {
            row.insert("succeeded".into(), status["succeeded"].as_u64().unwrap_or(0).into());
            row.insert("failed".into(), status["failed"].as_u64().unwrap_or(0).into());
        }
        "ConfigMap" => {
            let keys: Vec<&String> = obj["data"].as_object().map(|m| m.keys().collect()).unwrap_or_default();
            row.insert("keys".into(), json!(keys));
        }
        _ => {
            if let Some(phase) = status["phase"].as_str() {
                row.insert("status".into(), phase.into());
            }
        }
    }
    Value::Object(row)
}

/// [`summarize`] plus what an investigation usually needs first: per
/// container image / resources / probes, and non-true conditions.  The
/// context-chip form of an object.
pub fn brief(obj: &Value, now: DateTime<Utc>) -> Value {
    let mut row = summarize(obj, now);
    row["kind"] = obj["kind"].clone();
    let pod = if obj["kind"] == "Pod" { &obj["spec"] } else { &obj["spec"]["template"]["spec"] };
    let containers: Vec<Value> = pod["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let probes: Vec<&str> = ["livenessProbe", "readinessProbe"].into_iter().filter(|p| !c[*p].is_null()).collect();
            json!({"name": c["name"], "image": c["image"], "resources": c["resources"], "probes": probes})
        })
        .collect();
    if !containers.is_empty() {
        row["containers"] = Value::Array(containers);
    }
    let conditions: Vec<Value> = obj["status"]["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["status"] != "True")
        .map(|c| json!({"type": c["type"], "reason": c["reason"], "message": c["message"]}))
        .collect();
    if !conditions.is_empty() {
        row["conditions"] = Value::Array(conditions);
    }
    row
}

fn images(pod_spec: &Value) -> Value {
    pod_spec["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["image"].as_str())
        .collect::<Vec<_>>()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 22, 12, 0, 0).unwrap()
    }

    #[test]
    fn ages() {
        assert_eq!(age(Some("2026-10-22T11:59:30Z"), now()).as_deref(), Some("30s"));
        assert_eq!(age(Some("2026-10-22T11:00:00Z"), now()).as_deref(), Some("60m"));
        assert_eq!(age(Some("2026-10-22T07:00:00Z"), now()).as_deref(), Some("5h"));
        assert_eq!(age(Some("2026-10-12T12:00:00Z"), now()).as_deref(), Some("10d"));
        assert_eq!(age(Some("garbage"), now()), None);
    }

    #[test]
    fn crashlooping_pod() {
        let pod = json!({"kind": "Pod", "metadata": {"name": "x2kq", "namespace": "shop", "creationTimestamp": "2026-10-22T11:00:00Z"},
            "spec": {"nodeName": "n1"},
            "status": {"phase": "Running", "containerStatuses": [
                {"ready": false, "restartCount": 3, "state": {"waiting": {"reason": "CrashLoopBackOff"}}},
                {"ready": true, "restartCount": 0, "state": {"running": {}}}
            ]}});
        let row = summarize(&pod, now());
        assert_eq!(row, json!({"name": "x2kq", "namespace": "shop", "age": "60m", "ready": "1/2",
            "status": "CrashLoopBackOff", "restarts": 3, "node": "n1"}));
    }

    #[test]
    fn deployment_service_node_configmap() {
        let d = json!({"kind": "Deployment", "metadata": {"name": "web"}, "spec": {"replicas": 3,
            "template": {"spec": {"containers": [{"image": "web:1"}]}}}, "status": {"readyReplicas": 2}});
        assert_eq!(summarize(&d, now())["ready"], "2/3");
        assert_eq!(summarize(&d, now())["images"], json!(["web:1"]));
        let s = json!({"kind": "Service", "metadata": {"name": "web"}, "spec": {"type": "ClusterIP", "ports": [{"port": 80, "protocol": "TCP"}]}});
        assert_eq!(summarize(&s, now())["ports"], "80/TCP");
        let n = json!({"kind": "Node", "metadata": {"name": "n"}, "status": {"conditions": [{"type": "Ready", "status": "False"}], "nodeInfo": {"kubeletVersion": "v1.32"}}});
        assert_eq!(summarize(&n, now())["status"], "NotReady");
        let cm = json!({"kind": "ConfigMap", "metadata": {"name": "c"}, "data": {"A": "1", "B": "2"}});
        assert_eq!(summarize(&cm, now())["keys"], json!(["A", "B"]));
    }

    #[test]
    fn brief_adds_containers_and_failing_conditions() {
        let d = json!({"kind": "Deployment", "metadata": {"name": "web"}, "spec": {"replicas": 1, "template": {"spec": {
            "containers": [{"name": "app", "image": "web:1", "resources": {"limits": {"memory": "512Mi"}}, "readinessProbe": {}}]}}},
            "status": {"conditions": [{"type": "Available", "status": "False", "reason": "MinimumReplicasUnavailable"},
                                      {"type": "Progressing", "status": "True"}]}});
        let b = brief(&d, now());
        assert_eq!(b["kind"], "Deployment");
        assert_eq!(b["containers"][0]["resources"]["limits"]["memory"], "512Mi");
        assert_eq!(b["containers"][0]["probes"], json!(["readinessProbe"]));
        assert_eq!(b["conditions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn unknown_kind_keeps_phase() {
        let x = json!({"kind": "Certificate", "metadata": {"name": "c"}, "status": {"phase": "Ready"}});
        assert_eq!(summarize(&x, now()), json!({"name": "c", "status": "Ready"}));
    }
}
