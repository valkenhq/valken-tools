//! The read tools against a real cluster, through the registry (so
//! redaction and the Secret block apply exactly as in the app).
//!
//! ```sh
//! VALKEN_TEST_KUBECONFIG=/path/to/kubeconfig cargo test --test k8s_live -- --ignored
//! ```

use std::sync::Arc;

use serde_json::{json, Value};
use valken_tools::k8s::{self, ClusterInfo, Clusters, Kube};
use valken_tools::redact::Redactor;
use valken_tools::tool::{Actor, Registry, ToolError};

struct One(kube::Client);

impl Clusters for One {
    fn list(&self) -> Vec<ClusterInfo> {
        vec![ClusterInfo {
            name: "live".into(),
            env: "dev".into(),
            focused: true,
            connected: true,
        }]
    }
    fn client(&self, cluster: &str) -> Option<kube::Client> {
        (cluster == "live").then(|| self.0.clone())
    }
}

async fn registry() -> Option<Registry> {
    let path = std::env::var_os("VALKEN_TEST_KUBECONFIG")?;
    let kubeconfig = kube::config::Kubeconfig::read_from(path).ok()?;
    let config = kube::Config::from_custom_kubeconfig(kubeconfig, &Default::default())
        .await
        .ok()?;
    let client = kube::Client::try_from(config).ok()?;
    let mut r = Registry::new(Redactor::default(), None);
    k8s::register(&mut r, &Kube::new(Arc::new(One(client))));
    Some(r)
}

async fn call(r: &Registry, tool: &str, input: Value) -> Result<String, ToolError> {
    r.call(&Actor::Agent, false, tool, input)
        .await
        .output
        .map(|o| o.body)
}

#[tokio::test]
#[ignore = "needs VALKEN_TEST_KUBECONFIG"]
async fn read_tools_against_a_cluster() {
    let Some(r) = registry().await else {
        eprintln!("skipped: VALKEN_TEST_KUBECONFIG unset or unreadable");
        return;
    };
    let clusters: Value =
        serde_json::from_str(&call(&r, "list_clusters", json!({})).await.unwrap()).unwrap();
    assert_eq!(clusters[0]["name"], "live");

    // Short names resolve like kubectl; rows are compact.
    let pods: Value = serde_json::from_str(
        &call(
            &r,
            "list_objects",
            json!({"kind": "po", "namespace": "kube-system"}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let first = &pods["rows"][0];
    assert!(
        first["ready"].is_string() && first["restarts"].is_number(),
        "{first}"
    );
    let pod = first["name"].as_str().unwrap().to_owned();

    let obj: Value = serde_json::from_str(
        &call(
            &r,
            "get_object",
            json!({"kind": "deploy", "namespace": "kube-system", "name": "coredns"}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(obj["kind"], "Deployment");
    assert!(
        obj["metadata"].get("managedFields").is_none(),
        "noise stripped"
    );

    let logs = call(
        &r,
        "get_logs",
        json!({"namespace": "kube-system", "pod": pod, "tail": 5}),
    )
    .await
    .unwrap();
    assert!(logs.starts_with(&format!("pod {pod} container ")));
    assert!(logs.lines().count() <= 6);

    let events = call(
        &r,
        "get_events",
        json!({"namespace": "kube-system", "limit": 3}),
    )
    .await
    .unwrap();
    assert!(serde_json::from_str::<Vec<Value>>(&events).unwrap().len() <= 3);

    let revs: Value = serde_json::from_str(
        &call(
            &r,
            "get_rollout_history",
            json!({"namespace": "kube-system", "deployment": "coredns"}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(revs[0]["current"], true);

    // Secrets are refused outright, by any name.
    for kind in ["secret", "secrets", "Secret"] {
        let err = call(
            &r,
            "get_object",
            json!({"kind": kind, "namespace": "kube-system", "name": "x"}),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Forbidden(_)), "{kind}: {err:?}");
        let err = call(&r, "list_objects", json!({"kind": kind}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Forbidden(_)));
    }
    let missing = call(
        &r,
        "get_object",
        json!({"kind": "pod", "namespace": "kube-system", "name": "nope-404"}),
    )
    .await
    .unwrap_err();
    assert!(matches!(missing, ToolError::NotFound(_)));
    let previous = call(
        &r,
        "get_logs",
        json!({"namespace": "kube-system", "pod": pod, "previous": true}),
    )
    .await;
    assert!(previous.is_ok() || matches!(previous, Err(ToolError::NotFound(_))));
}
