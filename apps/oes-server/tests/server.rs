use std::time::Duration;

use oes_config::{Config, SecretValue};
use tempfile::tempdir;
use tokio::{net::TcpListener, sync::oneshot, time::timeout};

#[tokio::test]
async fn starts_serves_operational_routes_and_shuts_down() {
    let directory = tempdir().expect("temporary directory");
    let mut config = Config::default();
    config.storage.data_directory = directory.path().join("data");
    config.server.shutdown_grace_period_seconds = 2;
    config.auth.root_access_key = Some("test-access".into());
    config.auth.root_secret_key = Some(SecretValue::new("test-secret-at-least-sixteen"));
    config.auth.management_system_token = Some(SecretValue::new(
        "test-system-management-token-32-bytes-long",
    ));
    config.auth.management_auditor_token = Some(SecretValue::new(
        "test-auditor-management-token-32-bytes-long",
    ));

    let runtime = oes_server::initialize(&config)
        .await
        .expect("initialize server");
    let api_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let address = api_listener.local_addr().expect("listener address");
    let s3_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind S3 listener");
    let s3_address = s3_listener.local_addr().expect("S3 listener address");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(runtime.serve(s3_listener, api_listener, async move {
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

    let unauthorized_admin = client
        .get(format!("http://{address}/api/v1/buckets"))
        .send()
        .await
        .expect("unauthorized admin request");
    assert_eq!(
        unauthorized_admin.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );

    let created = client
        .post(format!("http://{address}/api/v1/buckets"))
        .bearer_auth("test-system-management-token-32-bytes-long")
        .json(&serde_json::json!({"name": "native-api-bucket"}))
        .send()
        .await
        .expect("create bucket request");
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);
    let buckets = client
        .get(format!("http://{address}/api/v1/buckets"))
        .bearer_auth("test-auditor-management-token-32-bytes-long")
        .send()
        .await
        .expect("bucket list request")
        .json::<serde_json::Value>()
        .await
        .expect("bucket list JSON");
    assert_eq!(buckets[0]["name"], "native-api-bucket");

    let auditor_write = client
        .post(format!("http://{address}/api/v1/buckets"))
        .bearer_auth("test-auditor-management-token-32-bytes-long")
        .json(&serde_json::json!({"name": "forbidden"}))
        .send()
        .await
        .expect("auditor write request");
    assert_eq!(auditor_write.status(), reqwest::StatusCode::FORBIDDEN);

    let s3_unauthorized = client
        .get(format!("http://{s3_address}/"))
        .send()
        .await
        .expect("unauthorized S3 request");
    assert_eq!(s3_unauthorized.status(), reqwest::StatusCode::FORBIDDEN);
    assert!(
        s3_unauthorized
            .text()
            .await
            .expect("S3 error body")
            .contains("<Code>AccessDenied</Code>")
    );

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

#[tokio::test]
async fn offline_metadata_backup_is_versioned_verified_and_non_overwriting() {
    let directory = tempdir().expect("temporary directory");
    let mut config = Config::default();
    config.storage.data_directory = directory.path().join("source");
    config.auth.root_access_key = Some("test-access".into());
    config.auth.root_secret_key = Some(SecretValue::new("test-secret-at-least-sixteen"));
    config.auth.credential_master_key = Some(SecretValue::new(
        "test-credential-master-key-at-least-32-bytes",
    ));
    let runtime = oes_server::initialize(&config)
        .await
        .expect("initialize source");
    let backup = directory.path().join("backup");
    assert!(oes_server::backup_metadata(&config, &backup).is_err());
    drop(runtime);
    oes_server::backup_metadata(&config, &backup).expect("backup");
    assert!(backup.join("manifest.json").is_file());

    let mut restored = config.clone();
    restored.storage.data_directory = directory.path().join("restored");
    oes_server::restore_metadata(&restored, &backup).expect("restore");
    let restored_runtime = oes_server::initialize(&restored)
        .await
        .expect("initialize restored state");
    drop(restored_runtime);
    assert!(oes_server::restore_metadata(&restored, &backup).is_err());
}
