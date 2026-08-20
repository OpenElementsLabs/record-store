use std::time::Duration;

use oes_config::Config;
use tempfile::tempdir;
use tokio::{net::TcpListener, sync::oneshot, time::timeout};

#[tokio::test]
async fn starts_serves_operational_routes_and_shuts_down() {
    let directory = tempdir().expect("temporary directory");
    let mut config = Config::default();
    config.storage.data_directory = directory.path().join("data");
    config.server.shutdown_grace_period_seconds = 2;

    let runtime = oes_server::initialize(&config)
        .await
        .expect("initialize server");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let address = listener.local_addr().expect("listener address");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(runtime.serve(listener, async move {
        let _ = shutdown_rx.await;
    }));

    let client = reqwest::Client::new();
    let health = client
        .get(format!("http://{address}/health"))
        .header("x-request-id", "integration-request")
        .send()
        .await
        .expect("health request");
    assert_eq!(health.status(), reqwest::StatusCode::OK);
    assert_eq!(
        health.headers().get("x-request-id").expect("request ID"),
        "integration-request"
    );
    assert_eq!(
        health
            .json::<serde_json::Value>()
            .await
            .expect("health JSON"),
        serde_json::json!({"status": "ok"})
    );

    let ready = client
        .get(format!("http://{address}/ready"))
        .send()
        .await
        .expect("readiness request");
    assert_eq!(ready.status(), reqwest::StatusCode::OK);

    let info = client
        .get(format!("http://{address}/api/v1/system/info"))
        .send()
        .await
        .expect("system info request")
        .json::<serde_json::Value>()
        .await
        .expect("system info JSON");
    assert_eq!(info["name"], "oes");
    assert_eq!(info["status"], "ready");
    assert!(info["version"].is_string());

    let missing = client
        .get(format!("http://{address}/not-found"))
        .send()
        .await
        .expect("missing route request");
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    let missing = missing
        .json::<serde_json::Value>()
        .await
        .expect("error JSON");
    assert_eq!(missing["error"]["code"], "ROUTE_NOT_FOUND");
    assert!(missing["error"]["request_id"].is_string());

    shutdown_tx.send(()).expect("request shutdown");
    timeout(Duration::from_secs(3), server)
        .await
        .expect("bounded shutdown")
        .expect("server task")
        .expect("clean shutdown");
}
