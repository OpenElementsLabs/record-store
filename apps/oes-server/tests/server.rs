use std::time::Duration;

use oes_config::{Config, DeploymentMode, SecretValue};
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
    assert_eq!(info["mode"], "standalone");
    assert!(info.get("cluster_id").is_none());
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

#[tokio::test]
async fn cluster_mode_bootstraps_persistent_identity_and_exposes_status() {
    let directory = tempdir().expect("temporary directory");
    let mut config = Config::default();
    config.server.mode = DeploymentMode::Cluster;
    let rpc_probe = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve RPC address");
    config.server.rpc_bind = rpc_probe.local_addr().expect("RPC address");
    config.server.rpc_advertise = Some(config.server.rpc_bind.to_string());
    drop(rpc_probe);
    config.server.shutdown_grace_period_seconds = 2;
    config.cluster.replication_factor = 1;
    config.storage.data_directory = directory.path().join("cluster-data");
    config.auth.root_access_key = Some("test-access".into());
    config.auth.root_secret_key = Some(SecretValue::new("test-secret-at-least-sixteen"));
    config.auth.management_system_token = Some(SecretValue::new(
        "test-system-management-token-32-bytes-long",
    ));

    let runtime = oes_server::initialize(&config)
        .await
        .expect("initialize clustered server");
    let identity_before = std::fs::read(config.storage.data_directory.join("node-identity.json"))
        .expect("persisted node identity");
    assert!(
        config
            .storage
            .data_directory
            .join("node-credential.json")
            .is_file()
    );

    let api_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind API listener");
    let address = api_listener.local_addr().expect("API address");
    let s3_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind S3 listener");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(runtime.serve(s3_listener, api_listener, async move {
        let _ = shutdown_rx.await;
    }));

    let status = reqwest::Client::new()
        .get(format!("http://{address}/api/v1/cluster"))
        .bearer_auth("test-system-management-token-32-bytes-long")
        .send()
        .await
        .expect("cluster status request");
    assert_eq!(status.status(), reqwest::StatusCode::OK);
    let status = status
        .json::<serde_json::Value>()
        .await
        .expect("cluster status JSON");
    assert_eq!(status["replication"]["replication_factor"], 1);
    assert_eq!(status["nodes"].as_array().map(Vec::len), Some(1));
    assert!(status["cluster_id"].is_string());

    let info = reqwest::Client::new()
        .get(format!("http://{address}/api/v1/system/info"))
        .send()
        .await
        .expect("cluster system info request")
        .json::<serde_json::Value>()
        .await
        .expect("cluster system info JSON");
    assert_eq!(info["mode"], "cluster");
    assert_eq!(info["cluster_id"], status["cluster_id"]);

    shutdown_tx.send(()).expect("request shutdown");
    timeout(Duration::from_secs(3), server)
        .await
        .expect("bounded cluster shutdown")
        .expect("server task")
        .expect("clean cluster shutdown");

    let restarted = oes_server::initialize(&config)
        .await
        .expect("restart clustered server");
    let identity_after = std::fs::read(config.storage.data_directory.join("node-identity.json"))
        .expect("reloaded node identity");
    assert_eq!(identity_before, identity_after);
    let api_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind restart API listener");
    let s3_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind restart S3 listener");
    restarted
        .serve(s3_listener, api_listener, std::future::ready(()))
        .await
        .expect("shut restarted cluster down");
}

#[tokio::test]
async fn a_token_joined_node_enters_the_consensus_group() {
    let directory = tempdir().expect("temporary directory");
    let first_rpc = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve first RPC");
    let first_rpc_address = first_rpc.local_addr().expect("first RPC address");
    drop(first_rpc);
    let mut first = Config::default();
    first.server.mode = DeploymentMode::Cluster;
    first.server.rpc_bind = first_rpc_address;
    first.server.rpc_advertise = Some(first_rpc_address.to_string());
    first.server.shutdown_grace_period_seconds = 2;
    first.cluster.replication_factor = 2;
    first.storage.data_directory = directory.path().join("first");
    first.auth.root_access_key = Some("test-access".into());
    first.auth.root_secret_key = Some(SecretValue::new("test-secret-at-least-sixteen"));
    first.auth.management_system_token = Some(SecretValue::new(
        "test-system-management-token-32-bytes-long",
    ));
    let first_runtime = oes_server::initialize(&first)
        .await
        .expect("initialize first node");
    let first_api = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind first API");
    let first_api_address = first_api.local_addr().expect("first API address");
    let first_s3 = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind first S3");
    let (first_shutdown_tx, first_shutdown_rx) = oneshot::channel();
    let first_server = tokio::spawn(first_runtime.serve(first_s3, first_api, async move {
        let _ = first_shutdown_rx.await;
    }));

    let client = reqwest::Client::new();
    let issued = client
        .post(format!(
            "http://{first_api_address}/api/v1/cluster/join-tokens"
        ))
        .bearer_auth("test-system-management-token-32-bytes-long")
        .json(&serde_json::json!({
            "lifetime_seconds": 300,
            "description": "integration join",
        }))
        .send()
        .await
        .expect("issue join token")
        .json::<serde_json::Value>()
        .await
        .expect("join token JSON");
    let token = issued["token"].as_str().expect("join token");

    let second_rpc = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve second RPC");
    let second_rpc_address = second_rpc.local_addr().expect("second RPC address");
    drop(second_rpc);
    let mut second = first.clone();
    second.server.rpc_bind = second_rpc_address;
    second.server.rpc_advertise = Some(second_rpc_address.to_string());
    second.cluster.seeds = vec![first_rpc_address.to_string()];
    second.cluster.join_token = Some(SecretValue::new(token));
    second.storage.data_directory = directory.path().join("second");
    let second_runtime = oes_server::initialize(&second)
        .await
        .expect("join second node");
    let second_api = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind second API");
    let second_s3 = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind second S3");
    let (second_shutdown_tx, second_shutdown_rx) = oneshot::channel();
    let second_server = tokio::spawn(second_runtime.serve(second_s3, second_api, async move {
        let _ = second_shutdown_rx.await;
    }));

    let status = client
        .get(format!("http://{first_api_address}/api/v1/cluster"))
        .bearer_auth("test-system-management-token-32-bytes-long")
        .send()
        .await
        .expect("cluster status")
        .json::<serde_json::Value>()
        .await
        .expect("cluster status JSON");
    assert_eq!(status["nodes"].as_array().map(Vec::len), Some(2));
    assert_eq!(status["metadata"]["status"]["members"], 2);

    second_shutdown_tx.send(()).expect("stop second node");
    timeout(Duration::from_secs(3), second_server)
        .await
        .expect("second node bounded shutdown")
        .expect("second server task")
        .expect("second node clean shutdown");
    first_shutdown_tx.send(()).expect("stop first node");
    timeout(Duration::from_secs(3), first_server)
        .await
        .expect("first node bounded shutdown")
        .expect("first server task")
        .expect("first node clean shutdown");
}
