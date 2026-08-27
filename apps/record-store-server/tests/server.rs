use std::time::Duration;

use record_store_config::{Config, DeploymentMode, SecretValue};
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
    config.auth.metrics_scrape_token = Some(SecretValue::new(
        "test-dedicated-metrics-scrape-token-32-bytes-long",
    ));

    let runtime = record_store_server::initialize(&config)
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

    let anonymous_info = client
        .get(format!("http://{address}/api/v1/system/info"))
        .send()
        .await
        .expect("anonymous system info request");
    assert_eq!(anonymous_info.status(), reqwest::StatusCode::UNAUTHORIZED);

    let info = client
        .get(format!("http://{address}/api/v1/system/info"))
        .bearer_auth("test-auditor-management-token-32-bytes-long")
        .send()
        .await
        .expect("authenticated system info request")
        .json::<serde_json::Value>()
        .await
        .expect("system info JSON");
    assert_eq!(info["name"], "record-store");
    assert_eq!(info["status"], "ready");
    assert_eq!(info["mode"], "standalone");
    assert!(info.get("cluster_id").is_none());
    assert!(info["version"].is_string());

    let missing_metrics_token = client
        .get(format!("http://{address}/metrics"))
        .send()
        .await
        .expect("metrics request without token");
    assert_eq!(
        missing_metrics_token.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let invalid_metrics_token = client
        .get(format!("http://{address}/metrics"))
        .bearer_auth("test-system-management-token-32-bytes-long")
        .send()
        .await
        .expect("metrics request with management token");
    assert_eq!(
        invalid_metrics_token.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let metrics = client
        .get(format!("http://{address}/metrics"))
        .bearer_auth("test-dedicated-metrics-scrape-token-32-bytes-long")
        .send()
        .await
        .expect("metrics request with scrape token");
    assert_eq!(metrics.status(), reqwest::StatusCode::OK);
    assert!(
        metrics
            .text()
            .await
            .expect("metrics body")
            .contains("record_store_requests_total")
    );

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
    let runtime = record_store_server::initialize(&config)
        .await
        .expect("initialize source");
    let backup = directory.path().join("backup");
    assert!(record_store_server::backup_metadata(&config, &backup).is_err());
    drop(runtime);
    record_store_server::backup_metadata(&config, &backup).expect("backup");
    assert!(backup.join("manifest.json").is_file());

    let mut restored = config.clone();
    restored.storage.data_directory = directory.path().join("restored");
    record_store_server::restore_metadata(&restored, &backup).expect("restore");
    let restored_runtime = record_store_server::initialize(&restored)
        .await
        .expect("initialize restored state");
    drop(restored_runtime);
    assert!(record_store_server::restore_metadata(&restored, &backup).is_err());
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

    let runtime = record_store_server::initialize(&config)
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
        .bearer_auth("test-system-management-token-32-bytes-long")
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

    let restarted = record_store_server::initialize(&config)
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
    let first_runtime = record_store_server::initialize(&first)
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
    let second_runtime = record_store_server::initialize(&second)
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

/// The console-facing surface: session, capabilities, bucket accounting, and the
/// object browser including streaming transfer in both directions.
#[tokio::test]
async fn management_api_serves_the_console_surface() {
    let directory = tempdir().expect("temporary directory");
    let mut config = Config::default();
    config.storage.data_directory = directory.path().join("data");
    config.server.shutdown_grace_period_seconds = 2;
    config.auth.root_access_key = Some("test-access".into());
    config.auth.root_secret_key = Some(SecretValue::new("test-secret-at-least-sixteen"));
    config.auth.credential_master_key = Some(SecretValue::new(
        "test-credential-master-key-at-least-32-bytes",
    ));
    config.auth.management_system_token = Some(SecretValue::new(
        "test-system-management-token-32-bytes-long",
    ));
    config.auth.management_auditor_token = Some(SecretValue::new(
        "test-auditor-management-token-32-bytes-long",
    ));

    let runtime = record_store_server::initialize(&config)
        .await
        .expect("initialize server");
    let api_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let address = api_listener.local_addr().expect("listener address");
    let s3_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind S3 listener");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(runtime.serve(s3_listener, api_listener, async move {
        let _ = shutdown_rx.await;
    }));

    let client = reqwest::Client::new();
    let admin = "test-system-management-token-32-bytes-long";
    let auditor = "test-auditor-management-token-32-bytes-long";
    let base = format!("http://{address}");

    // Deployment mode and capabilities are discovered from the backend.
    let info = client
        .get(format!("{base}/api/v1/system/info"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("system info")
        .json::<serde_json::Value>()
        .await
        .expect("system info JSON");
    assert_eq!(info["mode"], "standalone");
    assert_eq!(info["capabilities"]["cluster"], false);
    assert_eq!(info["capabilities"]["versioning"], true);
    assert_eq!(info["capabilities"]["object_browser"], true);
    assert_eq!(info["capabilities"]["erasure_coding"], false);

    // A session tells a client which role it holds and what to offer.
    let unauthenticated = client
        .get(format!("{base}/api/v1/auth/session"))
        .send()
        .await
        .expect("anonymous session request");
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);

    let session = client
        .get(format!("{base}/api/v1/auth/session"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("session request")
        .json::<serde_json::Value>()
        .await
        .expect("session JSON");
    assert_eq!(session["role"], "system_administrator");
    assert_eq!(session["permissions"]["manage_service_accounts"], true);

    let auditor_session = client
        .get(format!("{base}/api/v1/auth/session"))
        .bearer_auth(auditor)
        .send()
        .await
        .expect("auditor session")
        .json::<serde_json::Value>()
        .await
        .expect("auditor session JSON");
    assert_eq!(auditor_session["role"], "auditor");
    assert_eq!(auditor_session["permissions"]["manage_objects"], false);
    assert_eq!(auditor_session["permissions"]["read_audit"], true);

    client
        .post(format!("{base}/api/v1/buckets"))
        .bearer_auth(admin)
        .json(&serde_json::json!({"name": "console-bucket"}))
        .send()
        .await
        .expect("create bucket");

    // Lifecycle rules have an owned console consumer and explicit delete behavior.
    let lifecycle = client
        .post(format!("{base}/api/v1/buckets/console-bucket/lifecycle"))
        .bearer_auth(admin)
        .json(&serde_json::json!({
            "prefix": "reports/",
            "enabled": true,
            "expiration": 30,
            "noncurrent_version_expiration": 7
        }))
        .send()
        .await
        .expect("create lifecycle rule");
    assert_eq!(lifecycle.status(), reqwest::StatusCode::CREATED);
    let lifecycle = lifecycle
        .json::<serde_json::Value>()
        .await
        .expect("lifecycle rule JSON");
    let lifecycle_id = lifecycle["id"].as_str().expect("lifecycle rule ID");
    let lifecycle_rules = client
        .get(format!("{base}/api/v1/buckets/console-bucket/lifecycle"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("list lifecycle rules")
        .json::<serde_json::Value>()
        .await
        .expect("lifecycle list JSON");
    assert_eq!(lifecycle_rules[0]["id"], lifecycle_id);
    let deleted_lifecycle = client
        .delete(format!("{base}/api/v1/lifecycle-rules/{lifecycle_id}"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("delete lifecycle rule");
    assert_eq!(deleted_lifecycle.status(), reqwest::StatusCode::NO_CONTENT);
    let missing_lifecycle = client
        .delete(format!("{base}/api/v1/lifecycle-rules/{lifecycle_id}"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("delete missing lifecycle rule");
    assert_eq!(missing_lifecycle.status(), reqwest::StatusCode::BAD_REQUEST);

    // Streaming upload through the management API.
    let payload = b"console upload payload".to_vec();
    let uploaded = client
        .put(format!(
            "{base}/api/v1/buckets/console-bucket/object/reports/2026/q1.txt"
        ))
        .bearer_auth(admin)
        .header("content-type", "text/plain")
        .body(payload.clone())
        .send()
        .await
        .expect("upload object");
    assert_eq!(uploaded.status(), reqwest::StatusCode::CREATED);
    let uploaded = uploaded
        .json::<serde_json::Value>()
        .await
        .expect("upload JSON");
    assert_eq!(uploaded["key"], "reports/2026/q1.txt");
    assert_eq!(uploaded["size"], payload.len());
    // Internal identifiers must never reach a management client.
    assert!(uploaded.get("id").is_none());
    assert!(uploaded.get("bucket_id").is_none());
    assert!(uploaded.get("payload_format").is_none());

    // Bucket accounting arrives with the bucket list, not per bucket.
    let buckets = client
        .get(format!("{base}/api/v1/buckets"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("bucket list")
        .json::<serde_json::Value>()
        .await
        .expect("bucket list JSON");
    let bucket = buckets
        .as_array()
        .expect("array")
        .iter()
        .find(|entry| entry["name"] == "console-bucket")
        .expect("bucket present");
    assert_eq!(bucket["object_count"], 1);
    assert_eq!(bucket["logical_bytes"], payload.len());

    // Prefix navigation groups keys without inventing directories.
    let listing = client
        .get(format!(
            "{base}/api/v1/buckets/console-bucket/objects?delimiter=/&limit=10"
        ))
        .bearer_auth(admin)
        .send()
        .await
        .expect("object listing")
        .json::<serde_json::Value>()
        .await
        .expect("listing JSON");
    assert_eq!(listing["prefixes"][0], "reports/");
    assert_eq!(listing["objects"].as_array().expect("array").len(), 0);
    assert_eq!(listing["is_truncated"], false);

    let nested = client
        .get(format!(
            "{base}/api/v1/buckets/console-bucket/objects?prefix=reports/2026/&delimiter=/&limit=10"
        ))
        .bearer_auth(admin)
        .send()
        .await
        .expect("nested listing")
        .json::<serde_json::Value>()
        .await
        .expect("nested JSON");
    assert_eq!(nested["objects"][0]["key"], "reports/2026/q1.txt");

    let detail = client
        .get(format!(
            "{base}/api/v1/buckets/console-bucket/object/reports/2026/q1.txt"
        ))
        .bearer_auth(admin)
        .send()
        .await
        .expect("object detail")
        .json::<serde_json::Value>()
        .await
        .expect("detail JSON");
    assert_eq!(detail["content_type"], "text/plain");
    assert!(
        detail["checksum"]
            .as_str()
            .expect("checksum")
            .starts_with("sha256:")
    );

    // Streaming download returns the exact bytes with a safe disposition.
    let download = client
        .get(format!(
            "{base}/api/v1/buckets/console-bucket/object-content/reports/2026/q1.txt"
        ))
        .bearer_auth(admin)
        .send()
        .await
        .expect("download object");
    assert_eq!(download.status(), reqwest::StatusCode::OK);
    assert_eq!(
        download
            .headers()
            .get("content-disposition")
            .expect("disposition")
            .to_str()
            .expect("ascii"),
        "attachment; filename=\"q1.txt\""
    );
    assert_eq!(download.bytes().await.expect("body").as_ref(), payload);

    // Storage events are a separate feed from the audit trail.
    let events = client
        .get(format!("{base}/api/v1/events?limit=10"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("event list")
        .json::<serde_json::Value>()
        .await
        .expect("event JSON");
    let names: Vec<&str> = events["events"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect();
    assert!(names.contains(&"object.created"), "events: {names:?}");
    assert!(names.contains(&"bucket.created"), "events: {names:?}");

    // Version history is exposed once versioning is enabled.
    let versioning = client
        .put(format!("{base}/api/v1/buckets/console-bucket/versioning"))
        .bearer_auth(admin)
        .json(&serde_json::json!({"versioning": "enabled"}))
        .send()
        .await
        .expect("enable versioning");
    assert_eq!(versioning.status(), reqwest::StatusCode::OK);
    assert_eq!(
        versioning
            .json::<serde_json::Value>()
            .await
            .expect("versioning JSON")["versioning"],
        "enabled"
    );
    client
        .put(format!(
            "{base}/api/v1/buckets/console-bucket/object/reports/2026/q1.txt"
        ))
        .bearer_auth(admin)
        .body(b"second revision".to_vec())
        .send()
        .await
        .expect("second upload");
    let versions = client
        .get(format!(
            "{base}/api/v1/buckets/console-bucket/object-versions?prefix=reports/&limit=10"
        ))
        .bearer_auth(admin)
        .send()
        .await
        .expect("version listing")
        .json::<serde_json::Value>()
        .await
        .expect("version JSON");
    let entries = versions["versions"].as_array().expect("array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries.iter().filter(|v| v["is_latest"] == true).count(), 1);

    // An auditor may read the audit trail but must not browse object bytes.
    let auditor_objects = client
        .get(format!(
            "{base}/api/v1/buckets/console-bucket/objects?limit=10"
        ))
        .bearer_auth(auditor)
        .send()
        .await
        .expect("auditor object listing");
    assert_eq!(auditor_objects.status(), reqwest::StatusCode::FORBIDDEN);

    // Deleting a missing object is reported as a not-found, not a silent success.
    let missing = client
        .delete(format!(
            "{base}/api/v1/buckets/console-bucket/object/absent.txt"
        ))
        .bearer_auth(admin)
        .send()
        .await
        .expect("delete missing object");
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    let body = missing
        .json::<serde_json::Value>()
        .await
        .expect("error JSON");
    assert_eq!(body["error"]["code"], "OBJECT_NOT_FOUND");
    assert!(body["error"]["request_id"].is_string());

    let deleted = client
        .delete(format!(
            "{base}/api/v1/buckets/console-bucket/object/reports/2026/q1.txt"
        ))
        .bearer_auth(admin)
        .send()
        .await
        .expect("delete object");
    assert_eq!(deleted.status(), reqwest::StatusCode::NO_CONTENT);

    // A malformed cursor must be refused rather than crashing the listing.
    let bad_cursor = client
        .get(format!(
            "{base}/api/v1/buckets/console-bucket/objects?continuation_token=not-base64!!"
        ))
        .bearer_auth(admin)
        .send()
        .await
        .expect("malformed cursor");
    assert_eq!(bad_cursor.status(), reqwest::StatusCode::BAD_REQUEST);

    shutdown_tx.send(()).expect("request shutdown");
    timeout(Duration::from_secs(5), server)
        .await
        .expect("bounded shutdown")
        .expect("server task")
        .expect("clean shutdown");
}

/// The management plane and the scrape plane must expose the same numbers.
///
/// The console cannot read `/metrics`, so it reads the same values from
/// `/api/v1/system/metrics`. If those ever diverge, the console would be
/// reporting something Prometheus disagrees with.
#[tokio::test]
async fn metrics_are_served_to_both_planes_with_separate_credentials() {
    let directory = tempdir().expect("temporary directory");
    let mut config = Config::default();
    config.storage.data_directory = directory.path().join("data");
    config.server.shutdown_grace_period_seconds = 2;
    config.auth.root_access_key = Some("test-access".into());
    config.auth.root_secret_key = Some(SecretValue::new("test-secret-at-least-sixteen"));
    config.auth.credential_master_key = Some(SecretValue::new(
        "test-credential-master-key-at-least-32-bytes",
    ));
    config.auth.management_system_token = Some(SecretValue::new(
        "test-system-management-token-32-bytes-long",
    ));
    config.auth.metrics_scrape_token = Some(SecretValue::new(
        "test-metrics-scrape-token-32-bytes-long-x",
    ));

    let runtime = record_store_server::initialize(&config)
        .await
        .expect("initialize server");
    let api_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let address = api_listener.local_addr().expect("listener address");
    let s3_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind S3 listener");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(runtime.serve(s3_listener, api_listener, async move {
        let _ = shutdown_rx.await;
    }));

    let client = reqwest::Client::new();
    let admin = "test-system-management-token-32-bytes-long";
    let scrape = "test-metrics-scrape-token-32-bytes-long-x";
    let base = format!("http://{address}");

    // Neither credential opens the other plane.
    let cross = client
        .get(format!("{base}/metrics"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("scrape with a management token");
    assert_eq!(cross.status(), 401, "a management token must not scrape");
    let cross = client
        .get(format!("{base}/api/v1/system/metrics"))
        .bearer_auth(scrape)
        .send()
        .await
        .expect("management read with a scrape token");
    assert_eq!(
        cross.status(),
        401,
        "a scrape token must not read the management plane"
    );

    let anonymous = client
        .get(format!("{base}/api/v1/system/metrics"))
        .send()
        .await
        .expect("anonymous management metrics");
    assert_eq!(anonymous.status(), 401);

    let json = client
        .get(format!("{base}/api/v1/system/metrics"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("management metrics")
        .json::<serde_json::Value>()
        .await
        .expect("management metrics JSON");
    let exposition = client
        .get(format!("{base}/metrics"))
        .bearer_auth(scrape)
        .send()
        .await
        .expect("prometheus metrics")
        .text()
        .await
        .expect("prometheus body");

    // Standalone reports no cluster section rather than zeroes that would read
    // as a broken cluster.
    assert!(
        json.get("cluster").is_none(),
        "standalone must not report cluster metrics: {json}"
    );

    for (field, metric) in [
        ("object_count", "record_store_objects_total"),
        ("bucket_count", "record_store_buckets_total"),
        ("version_count", "record_store_versions_total"),
        ("logical_bytes", "record_store_storage_logical_bytes"),
        ("physical_bytes", "record_store_storage_physical_bytes"),
    ] {
        let value = json["storage"][field]
            .as_u64()
            .unwrap_or_else(|| panic!("{field} is a number"));
        assert!(
            exposition.contains(&format!("{metric} {value}\n")),
            "{metric} must match the JSON field {field} ({value}); body was:\n{exposition}"
        );
    }
    for (field, metric) in [
        ("requests", "record_store_requests_total"),
        ("errors", "record_store_errors_total"),
        ("upload_bytes", "record_store_upload_bytes_total"),
        ("download_bytes", "record_store_download_bytes_total"),
    ] {
        let value = json[field]
            .as_u64()
            .unwrap_or_else(|| panic!("{field} is a number"));
        assert!(
            exposition.contains(&format!("{metric} {value}\n")),
            "{metric} must match the JSON field {field} ({value})"
        );
    }

    let _ = shutdown_tx.send(());
    let _ = timeout(Duration::from_secs(10), server).await;
}

/// Server-side copy through the management plane.
///
/// The console offers copy, so the bytes must move inside Record Store rather than being
/// downloaded and re-uploaded by the browser.
#[tokio::test]
async fn management_api_copies_objects_server_side() {
    let directory = tempdir().expect("temporary directory");
    let mut config = Config::default();
    config.storage.data_directory = directory.path().join("data");
    config.server.shutdown_grace_period_seconds = 2;
    config.auth.root_access_key = Some("test-access".into());
    config.auth.root_secret_key = Some(SecretValue::new("test-secret-at-least-sixteen"));
    config.auth.credential_master_key = Some(SecretValue::new(
        "test-credential-master-key-at-least-32-bytes",
    ));
    config.auth.management_system_token = Some(SecretValue::new(
        "test-system-management-token-32-bytes-long",
    ));
    config.auth.management_auditor_token = Some(SecretValue::new(
        "test-auditor-management-token-32-bytes-long",
    ));

    let runtime = record_store_server::initialize(&config)
        .await
        .expect("initialize server");
    let api_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let address = api_listener.local_addr().expect("listener address");
    let s3_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind S3 listener");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(runtime.serve(s3_listener, api_listener, async move {
        let _ = shutdown_rx.await;
    }));

    let client = reqwest::Client::new();
    let admin = "test-system-management-token-32-bytes-long";
    let auditor = "test-auditor-management-token-32-bytes-long";
    let base = format!("http://{address}");

    for name in ["copy-source", "copy-target"] {
        let created = client
            .post(format!("{base}/api/v1/buckets"))
            .bearer_auth(admin)
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await
            .expect("create bucket");
        assert_eq!(created.status(), 201, "creating {name}");
    }

    let payload = "copy me exactly";
    let uploaded = client
        .put(format!(
            "{base}/api/v1/buckets/copy-source/object/nested/report.txt"
        ))
        .bearer_auth(admin)
        .header("content-type", "text/plain")
        .body(payload)
        .send()
        .await
        .expect("upload the source object");
    assert_eq!(uploaded.status(), 201);
    let source: serde_json::Value = uploaded.json().await.expect("source JSON");

    // An auditor must not be able to create an object by copying one.
    let refused = client
        .post(format!(
            "{base}/api/v1/buckets/copy-target/object-copy/copied/report.txt"
        ))
        .bearer_auth(auditor)
        .json(&serde_json::json!({
            "source_bucket": "copy-source",
            "source_key": "nested/report.txt"
        }))
        .send()
        .await
        .expect("auditor copy attempt");
    assert_eq!(refused.status(), 403);

    let copied = client
        .post(format!(
            "{base}/api/v1/buckets/copy-target/object-copy/copied/report.txt"
        ))
        .bearer_auth(admin)
        .json(&serde_json::json!({
            "source_bucket": "copy-source",
            "source_key": "nested/report.txt"
        }))
        .send()
        .await
        .expect("copy the object");
    assert_eq!(copied.status(), 201);
    let destination: serde_json::Value = copied.json().await.expect("copy JSON");

    // The copy is byte-identical, which the checksum proves, and it is a
    // distinct object version rather than a second reference to the source.
    assert_eq!(destination["checksum"], source["checksum"]);
    assert_eq!(destination["size"], source["size"]);
    assert_eq!(destination["key"], "copied/report.txt");
    assert_ne!(destination["version_id"], source["version_id"]);
    // Internal identifiers must not leak through the management surface.
    assert!(destination.get("id").is_none());
    assert!(destination.get("payload_format").is_none());

    let served = client
        .get(format!(
            "{base}/api/v1/buckets/copy-target/object-content/copied/report.txt"
        ))
        .bearer_auth(admin)
        .send()
        .await
        .expect("download the copy")
        .text()
        .await
        .expect("copy body");
    assert_eq!(served, payload);

    // The source is untouched by the copy.
    let source_still_there = client
        .get(format!(
            "{base}/api/v1/buckets/copy-source/object-content/nested/report.txt"
        ))
        .bearer_auth(admin)
        .send()
        .await
        .expect("download the source")
        .text()
        .await
        .expect("source body");
    assert_eq!(source_still_there, payload);

    // A missing source is a not-found, not a partially created destination.
    let missing = client
        .post(format!(
            "{base}/api/v1/buckets/copy-target/object-copy/orphan.txt"
        ))
        .bearer_auth(admin)
        .json(&serde_json::json!({
            "source_bucket": "copy-source",
            "source_key": "does/not/exist.txt"
        }))
        .send()
        .await
        .expect("copy a missing object");
    assert_eq!(missing.status(), 404);
    let body: serde_json::Value = missing.json().await.expect("error JSON");
    assert_eq!(body["error"]["code"], "OBJECT_NOT_FOUND");

    // Every object route reports a missing key as not-found. These once fell
    // through to a 500 because the service error had no mapping.
    for path in [
        "api/v1/buckets/copy-source/object/does/not/exist.txt",
        "api/v1/buckets/copy-source/object-content/does/not/exist.txt",
    ] {
        let response = client
            .get(format!("{base}/{path}"))
            .bearer_auth(admin)
            .send()
            .await
            .expect("read a missing object");
        assert_eq!(response.status(), 404, "GET {path}");
    }
    let verified = client
        .post(format!(
            "{base}/api/v1/verify/objects/copy-source/does/not/exist.txt"
        ))
        .bearer_auth(admin)
        .send()
        .await
        .expect("verify a missing object");
    assert_eq!(verified.status(), 404);

    let _ = shutdown_tx.send(());
    let _ = timeout(Duration::from_secs(10), server).await;
}

/// Lifecycle rules can be edited and disabled, not only created and deleted.
#[tokio::test]
async fn management_api_updates_lifecycle_rules() {
    let directory = tempdir().expect("temporary directory");
    let mut config = Config::default();
    config.storage.data_directory = directory.path().join("data");
    config.server.shutdown_grace_period_seconds = 2;
    config.auth.root_access_key = Some("test-access".into());
    config.auth.root_secret_key = Some(SecretValue::new("test-secret-at-least-sixteen"));
    config.auth.credential_master_key = Some(SecretValue::new(
        "test-credential-master-key-at-least-32-bytes",
    ));
    config.auth.management_system_token = Some(SecretValue::new(
        "test-system-management-token-32-bytes-long",
    ));

    let runtime = record_store_server::initialize(&config)
        .await
        .expect("initialize server");
    let api_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let address = api_listener.local_addr().expect("listener address");
    let s3_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind S3 listener");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(runtime.serve(s3_listener, api_listener, async move {
        let _ = shutdown_rx.await;
    }));

    let client = reqwest::Client::new();
    let admin = "test-system-management-token-32-bytes-long";
    let base = format!("http://{address}");

    for name in ["lifecycle-a", "lifecycle-b"] {
        let created = client
            .post(format!("{base}/api/v1/buckets"))
            .bearer_auth(admin)
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await
            .expect("create bucket");
        assert_eq!(created.status(), 201);
    }

    let created: serde_json::Value = client
        .post(format!("{base}/api/v1/buckets/lifecycle-a/lifecycle"))
        .bearer_auth(admin)
        .json(&serde_json::json!({ "prefix": "backups/", "expiration": 90 }))
        .send()
        .await
        .expect("create rule")
        .json()
        .await
        .expect("rule JSON");
    let rule_id = created["id"].as_str().expect("rule id").to_owned();
    assert_eq!(created["enabled"], true);

    // Editing replaces the whole rule, and an explicit null clears a field.
    let updated: serde_json::Value = client
        .put(format!(
            "{base}/api/v1/buckets/lifecycle-a/lifecycle/{rule_id}"
        ))
        .bearer_auth(admin)
        .json(&serde_json::json!({
            "prefix": "archive/",
            "enabled": false,
            "expiration": null,
            "noncurrent_version_expiration": 30
        }))
        .send()
        .await
        .expect("update rule")
        .json()
        .await
        .expect("updated JSON");
    assert_eq!(updated["prefix"], "archive/");
    assert_eq!(updated["enabled"], false);
    assert!(updated["expiration"].is_null());
    assert_eq!(updated["noncurrent_version_expiration"], 30);
    // Identity and creation time survive an edit; only `updated_at` moves.
    assert_eq!(updated["id"], created["id"]);
    assert_eq!(updated["created_at"], created["created_at"]);

    let listed: serde_json::Value = client
        .get(format!("{base}/api/v1/buckets/lifecycle-a/lifecycle"))
        .bearer_auth(admin)
        .send()
        .await
        .expect("list rules")
        .json()
        .await
        .expect("list JSON");
    assert_eq!(listed.as_array().expect("array").len(), 1);
    assert_eq!(listed[0]["prefix"], "archive/");

    // A rule identifier from one bucket cannot be used to edit another's.
    let cross = client
        .put(format!(
            "{base}/api/v1/buckets/lifecycle-b/lifecycle/{rule_id}"
        ))
        .bearer_auth(admin)
        .json(&serde_json::json!({ "prefix": "", "enabled": true, "expiration": 5 }))
        .send()
        .await
        .expect("cross-bucket update");
    assert_eq!(cross.status(), 404);

    // A rule with no action at all is refused rather than silently stored.
    let empty = client
        .put(format!(
            "{base}/api/v1/buckets/lifecycle-a/lifecycle/{rule_id}"
        ))
        .bearer_auth(admin)
        .json(&serde_json::json!({ "prefix": "archive/", "enabled": true }))
        .send()
        .await
        .expect("empty rule update");
    assert_eq!(empty.status(), 400);

    let _ = shutdown_tx.send(());
    let _ = timeout(Duration::from_secs(10), server).await;
}
