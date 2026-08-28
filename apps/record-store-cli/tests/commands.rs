//! End-to-end coverage of the CLI's management commands.
//!
//! Each test runs the real binary against a mock management API. Driving the
//! built executable rather than calling the handlers directly keeps the process
//! environment out of the test process — the crate forbids `unsafe`, so the
//! credentials a request helper reads can only be supplied to a child process.

use std::process::Output;

use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Runs the CLI against `endpoint` with management credentials supplied.
async fn run(endpoint: &str, arguments: &[&str]) -> Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_record-store"));
    command
        .args(arguments)
        .arg("--endpoint")
        .arg(endpoint)
        .env("RECORD_STORE_MANAGEMENT_TOKEN", "test-management-token")
        .env_remove("RECORD_STORE_CONFIG_FILE")
        .env_remove("RECORD_STORE_ROOT_ACCESS_KEY")
        .env_remove("RECORD_STORE_ROOT_SECRET_KEY");
    command.output().await.expect("run the CLI")
}

/// Runs the CLI and asserts it succeeded, returning its standard output.
async fn stdout_of(endpoint: &str, arguments: &[&str]) -> String {
    let output = run(endpoint, arguments).await;
    assert!(
        output.status.success(),
        "`{}` failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout is UTF-8")
}

/// Runs the CLI expecting failure, returning its standard error.
async fn stderr_of(endpoint: &str, arguments: &[&str]) -> String {
    let output = run(endpoint, arguments).await;
    assert!(
        !output.status.success(),
        "`{}` unexpectedly succeeded",
        arguments.join(" ")
    );
    String::from_utf8(output.stderr).expect("stderr is UTF-8")
}

async fn mock(server: &MockServer, verb: &str, route: &str, status: u16, body: Value) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

/// The catalog's own `Bucket`, with every optional field left to its default.
fn bucket_json(name: &str) -> Value {
    json!({
        "id": "0195f0c8-0000-7000-8000-000000000001",
        "organization_id": "0195f0c8-0000-7000-8000-000000000002",
        "name": name,
        "created_at": "2026-01-01T00:00:00Z",
    })
}

/// Every management call must present the operator's credential. A request that
/// reached the API unauthenticated would be a silent privilege problem, so the
/// header is asserted rather than assumed.
#[tokio::test]
async fn management_requests_carry_the_operator_credential() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/buckets"))
        .and(header("authorization", "Bearer test-management-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;

    stdout_of(&server.uri(), &["bucket", "list"]).await;
}

/// Without any credential the CLI must refuse before sending anything, and say
/// which variable is missing.
#[tokio::test]
async fn a_command_without_credentials_explains_what_is_missing() {
    let server = MockServer::start().await;
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_record-store"))
        .args(["bucket", "list", "--endpoint", &server.uri()])
        .env_remove("RECORD_STORE_MANAGEMENT_TOKEN")
        .env_remove("RECORD_STORE_ROOT_ACCESS_KEY")
        .env_remove("RECORD_STORE_ROOT_SECRET_KEY")
        .output()
        .await
        .expect("run the CLI");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("RECORD_STORE_ROOT_ACCESS_KEY"), "{stderr}");
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        0,
        "no request may be sent without a credential"
    );
}

#[tokio::test]
async fn bucket_list_prints_names_plainly_and_json_on_request() {
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/api/v1/buckets",
        200,
        json!([bucket_json("photos"), bucket_json("archive")]),
    )
    .await;

    let plain = stdout_of(&server.uri(), &["bucket", "list"]).await;
    assert_eq!(plain.lines().collect::<Vec<_>>(), vec!["photos", "archive"]);

    let encoded = stdout_of(&server.uri(), &["--json", "bucket", "list"]).await;
    let parsed: Value = serde_json::from_str(&encoded).expect("JSON output");
    assert_eq!(parsed.as_array().expect("array").len(), 2);
    assert_eq!(parsed[0]["name"], "photos");
}

/// The bucket name travels in the request body, not the path. Sending it the
/// wrong way would create a bucket with the wrong name or none at all.
#[tokio::test]
async fn bucket_create_sends_the_name_in_the_request_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/buckets"))
        .and(body_json(json!({"name": "photos"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(bucket_json("photos")))
        .expect(1)
        .mount(&server)
        .await;

    let output = stdout_of(&server.uri(), &["bucket", "create", "photos"]).await;
    assert_eq!(output.trim(), "photos");
}

#[tokio::test]
async fn bucket_delete_targets_the_named_bucket_and_confirms() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/buckets/photos"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let output = stdout_of(&server.uri(), &["bucket", "delete", "photos"]).await;
    assert_eq!(output.trim(), "deleted photos");
}

#[tokio::test]
async fn bucket_versioning_reads_and_writes_the_same_resource() {
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/api/v1/buckets/photos/versioning",
        200,
        json!({"versioning": "disabled"}),
    )
    .await;
    mock(
        &server,
        "PUT",
        "/api/v1/buckets/photos/versioning",
        200,
        json!({"versioning": "enabled"}),
    )
    .await;

    let read = stdout_of(&server.uri(), &["bucket", "versioning", "get", "photos"]).await;
    assert!(read.contains("disabled"), "{read}");

    let enabled = stdout_of(&server.uri(), &["bucket", "versioning", "enable", "photos"]).await;
    assert!(enabled.contains("enabled"), "{enabled}");
}

/// A created account's secret is shown exactly once, so the create command has
/// to surface it rather than printing only the identifier.
#[tokio::test]
async fn creating_a_service_account_prints_the_secret_it_will_never_see_again() {
    let server = MockServer::start().await;
    mock(
        &server,
        "POST",
        "/api/v1/service-accounts",
        201,
        json!({
            "account": {
                "id": "0195f0c8-0000-7000-8000-000000000010",
                "name": "backups",
                "enabled": true,
            },
            "access_key_id": "AKIAEXAMPLE",
            "secret_access_key": "s3cr3t-shown-once",
        }),
    )
    .await;

    let output = stdout_of(&server.uri(), &["service-account", "create", "backups"]).await;
    assert!(output.contains("AKIAEXAMPLE"), "{output}");
    assert!(output.contains("s3cr3t-shown-once"), "{output}");
}

#[tokio::test]
async fn service_accounts_can_be_listed_inspected_and_switched_off() {
    let server = MockServer::start().await;
    let id = "0195f0c8-0000-7000-8000-000000000010";
    mock(
        &server,
        "GET",
        "/api/v1/service-accounts",
        200,
        json!([{"id": id, "name": "backups", "enabled": true}]),
    )
    .await;
    mock(
        &server,
        "GET",
        &format!("/api/v1/service-accounts/{id}"),
        200,
        json!({"id": id, "name": "backups", "enabled": true}),
    )
    .await;
    mock(
        &server,
        "PUT",
        &format!("/api/v1/service-accounts/{id}/status"),
        200,
        json!({"id": id, "enabled": false}),
    )
    .await;

    assert!(
        stdout_of(&server.uri(), &["service-account", "list"])
            .await
            .contains("backups")
    );
    assert!(
        stdout_of(&server.uri(), &["service-account", "inspect", id])
            .await
            .contains("backups")
    );
    stdout_of(&server.uri(), &["service-account", "disable", id]).await;
    stdout_of(&server.uri(), &["service-account", "enable", id]).await;
}

/// The audit command's filters have to reach the API as query parameters; a
/// filter that is silently dropped would show an operator the wrong events.
#[tokio::test]
async fn audit_filters_are_sent_as_query_parameters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/audit/events"))
        .and(query_param("limit", "5"))
        .and(query_param("principal", "root"))
        .and(query_param("operation", "DeleteBucket"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"events": []})))
        .expect(1)
        .mount(&server)
        .await;

    stdout_of(
        &server.uri(),
        &[
            "audit",
            "--limit",
            "5",
            "--principal",
            "root",
            "--operation",
            "DeleteBucket",
        ],
    )
    .await;
}

#[tokio::test]
async fn storage_inspection_and_repair_reach_their_own_routes() {
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/api/v1/storage/inspect",
        200,
        json!({"orphan_payloads": []}),
    )
    .await;
    mock(
        &server,
        "POST",
        "/api/v1/storage/repair",
        200,
        json!({"removed": 0}),
    )
    .await;

    stdout_of(&server.uri(), &["storage", "inspect"]).await;
    stdout_of(&server.uri(), &["storage", "repair"]).await;
}

#[tokio::test]
async fn verification_covers_a_single_object_and_a_whole_bucket() {
    let server = MockServer::start().await;
    mock(
        &server,
        "POST",
        "/api/v1/verify/objects/photos/holiday.jpg",
        200,
        json!({"verified": true}),
    )
    .await;
    mock(
        &server,
        "POST",
        "/api/v1/verify/buckets/photos",
        200,
        json!({"verified_objects": 3, "failures": []}),
    )
    .await;

    stdout_of(
        &server.uri(),
        &["verify", "object", "photos", "holiday.jpg"],
    )
    .await;
    let bucket = stdout_of(&server.uri(), &["verify", "bucket", "photos"]).await;
    assert!(bucket.contains('3'), "{bucket}");
}

#[tokio::test]
async fn webhooks_and_their_deliveries_can_be_listed() {
    let server = MockServer::start().await;
    let id = "0195f0c8-0000-7000-8000-000000000020";
    mock(
        &server,
        "GET",
        "/api/v1/webhooks",
        200,
        json!([{"id": id, "endpoint": "https://hooks.example", "enabled": true}]),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/webhook-deliveries",
        200,
        json!([{"webhook_id": id, "status": "delivered"}]),
    )
    .await;

    assert!(
        stdout_of(&server.uri(), &["webhook", "list"])
            .await
            .contains("hooks.example")
    );
    assert!(
        stdout_of(&server.uri(), &["webhook", "deliveries"])
            .await
            .contains("delivered")
    );
}

#[tokio::test]
async fn cluster_and_node_state_can_be_inspected() {
    let server = MockServer::start().await;
    let id = "0195f0c8-0000-7000-8000-000000000030";
    mock(
        &server,
        "GET",
        "/api/v1/cluster",
        200,
        json!({"cluster_id": "abc", "health": "healthy", "nodes": []}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/nodes",
        200,
        json!([{"node_id": id}]),
    )
    .await;
    mock(
        &server,
        "GET",
        &format!("/api/v1/nodes/{id}"),
        200,
        json!({"node_id": id, "state": "healthy"}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/repair/status",
        200,
        json!({"queued": 0}),
    )
    .await;
    mock(
        &server,
        "GET",
        "/api/v1/rebalance/status",
        200,
        json!({"running": false}),
    )
    .await;

    let status = stdout_of(&server.uri(), &["cluster", "status"]).await;
    assert!(status.contains("healthy"), "{status}");
    stdout_of(&server.uri(), &["node", "list"]).await;
    stdout_of(&server.uri(), &["node", "inspect", id]).await;
    stdout_of(&server.uri(), &["repair", "status"]).await;
    stdout_of(&server.uri(), &["rebalance", "status"]).await;
}

/// The API's own error body is the only thing that explains a refusal, so it has
/// to reach the operator instead of being replaced by a generic message.
#[tokio::test]
async fn an_api_refusal_surfaces_its_status_and_body() {
    let server = MockServer::start().await;
    mock(
        &server,
        "DELETE",
        "/api/v1/buckets/photos",
        409,
        json!({"error": {"code": "BUCKET_NOT_EMPTY", "message": "bucket is not empty"}}),
    )
    .await;

    let stderr = stderr_of(&server.uri(), &["bucket", "delete", "photos"]).await;
    assert!(stderr.contains("409"), "{stderr}");
    assert!(stderr.contains("BUCKET_NOT_EMPTY"), "{stderr}");
}

/// A body that does not match the expected shape must fail loudly rather than
/// printing an empty or partial result.
#[tokio::test]
async fn an_undecodable_response_is_reported_rather_than_printed_empty() {
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/api/v1/buckets",
        200,
        json!({"unexpected": true}),
    )
    .await;

    let stderr = stderr_of(&server.uri(), &["bucket", "list"]).await;
    assert!(stderr.contains("decode bucket list"), "{stderr}");
}

/// An endpoint that is not listening has to produce an explanation naming the
/// address, not a bare transport error.
#[tokio::test]
async fn an_unreachable_endpoint_names_the_address_it_could_not_reach() {
    let stderr = stderr_of("http://127.0.0.1:1", &["status"]).await;
    assert!(stderr.contains("127.0.0.1:1"), "{stderr}");
}

#[tokio::test]
async fn status_reports_the_deployment_mode_of_a_ready_server() {
    let server = MockServer::start().await;
    mock(&server, "GET", "/ready", 200, json!({"status": "ready"})).await;
    mock(
        &server,
        "GET",
        "/api/v1/system/info",
        200,
        json!({"mode": "standalone", "cluster_id": "cluster-7"}),
    )
    .await;

    let output = stdout_of(&server.uri(), &["status"]).await;
    assert!(output.contains("standalone"), "{output}");
    assert!(output.contains("cluster-7"), "{output}");
}

/// A server that answers but is not ready must be reported as not ready; a
/// non-2xx readiness response is not a usable server.
#[tokio::test]
async fn a_server_that_is_not_ready_is_reported_as_such() {
    let server = MockServer::start().await;
    mock(&server, "GET", "/ready", 503, json!({"status": "starting"})).await;

    let stderr = stderr_of(&server.uri(), &["status"]).await;
    assert!(stderr.contains("not ready"), "{stderr}");
}

#[tokio::test]
async fn the_version_command_needs_no_server_at_all() {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_record-store"))
        .arg("version")
        .output()
        .await
        .expect("run the CLI");
    assert!(output.status.success());
    assert!(!output.stdout.is_empty());
}

const ACCOUNT: &str = "0195f0c8-0000-7000-8000-000000000010";
const CREDENTIAL: &str = "0195f0c8-0000-7000-8000-000000000011";
const POLICY: &str = "0195f0c8-0000-7000-8000-000000000012";
const NODE: &str = "0195f0c8-0000-7000-8000-000000000030";

/// A rotated credential is shown once, so both halves have to be printed.
#[tokio::test]
async fn rotating_a_credential_prints_the_new_secret_once() {
    let server = MockServer::start().await;
    mock(
        &server,
        "POST",
        &format!("/api/v1/service-accounts/{ACCOUNT}/credentials"),
        201,
        json!({"access_key_id": "AKIAROTATED", "secret_access_key": "rotated-secret"}),
    )
    .await;

    let output = stdout_of(&server.uri(), &["credential", "rotate", ACCOUNT]).await;
    assert!(output.contains("AKIAROTATED"), "{output}");
    assert!(output.contains("rotated-secret"), "{output}");
}

#[tokio::test]
async fn a_credential_can_be_switched_off_and_on_again() {
    let server = MockServer::start().await;
    mock(
        &server,
        "PUT",
        &format!("/api/v1/service-accounts/{ACCOUNT}/credentials/{CREDENTIAL}/status"),
        200,
        json!({"enabled": false}),
    )
    .await;

    stdout_of(
        &server.uri(),
        &["credential", "disable", ACCOUNT, CREDENTIAL],
    )
    .await;
    stdout_of(
        &server.uri(),
        &["credential", "enable", ACCOUNT, CREDENTIAL],
    )
    .await;
}

/// The requested lifetime has to reach the API; a dropped value would issue a
/// credential that outlives what the operator asked for.
#[tokio::test]
async fn a_temporary_credential_carries_the_requested_lifetime() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v1/service-accounts/{ACCOUNT}/temporary-credentials"
        )))
        .and(body_json(json!({"expires_in_seconds": 900})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "access_key_id": "AKIATEMP",
            "secret_access_key": "temp-secret",
            "expires_at": "2026-01-01T01:00:00Z",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let output = stdout_of(
        &server.uri(),
        &[
            "credential",
            "temporary",
            ACCOUNT,
            "--expires-in-seconds",
            "900",
        ],
    )
    .await;
    assert!(output.contains("AKIATEMP"), "{output}");
}

#[tokio::test]
async fn policies_can_be_listed_attached_and_detached() {
    let server = MockServer::start().await;
    mock(
        &server,
        "GET",
        "/api/v1/policies",
        200,
        json!([{"id": POLICY, "name": "read-only"}]),
    )
    .await;
    mock(
        &server,
        "PUT",
        &format!("/api/v1/policies/{POLICY}/bindings/{ACCOUNT}"),
        204,
        json!(null),
    )
    .await;
    mock(
        &server,
        "DELETE",
        &format!("/api/v1/policies/{POLICY}/bindings/{ACCOUNT}"),
        204,
        json!(null),
    )
    .await;

    assert!(
        stdout_of(&server.uri(), &["policy", "list"])
            .await
            .contains("read-only")
    );
    stdout_of(&server.uri(), &["policy", "attach", POLICY, ACCOUNT]).await;
    stdout_of(&server.uri(), &["policy", "detach", POLICY, ACCOUNT]).await;
}

/// A policy document is read from disk, so a missing file has to be reported
/// rather than sending an empty body to the API.
#[tokio::test]
async fn creating_a_policy_from_a_missing_file_fails_before_any_request() {
    let server = MockServer::start().await;
    let stderr = stderr_of(
        &server.uri(),
        &["policy", "create", "/nonexistent/policy.json"],
    )
    .await;
    assert!(stderr.contains("policy.json"), "{stderr}");
    assert_eq!(server.received_requests().await.expect("requests").len(), 0);
}

#[tokio::test]
async fn a_policy_document_is_posted_verbatim_from_disk() {
    let server = MockServer::start().await;
    let directory = tempfile::tempdir().expect("temporary directory");
    let file = directory.path().join("policy.json");
    let document = json!({"name": "read-only", "statements": []});
    std::fs::write(&file, document.to_string()).expect("write policy");

    Mock::given(method("POST"))
        .and(path("/api/v1/policies"))
        .and(body_json(document))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": POLICY})))
        .expect(1)
        .mount(&server)
        .await;

    stdout_of(
        &server.uri(),
        &["policy", "create", file.to_str().expect("path")],
    )
    .await;
}

/// Every node lifecycle transition is its own route. Sending one to another's
/// endpoint would put a node into the wrong state.
#[tokio::test]
async fn each_node_lifecycle_transition_uses_its_own_route() {
    let server = MockServer::start().await;
    for action in ["drain", "maintenance", "resume"] {
        Mock::given(method("POST"))
            .and(path(format!("/api/v1/nodes/{NODE}/{action}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state": action})))
            .expect(1)
            .mount(&server)
            .await;
    }

    stdout_of(&server.uri(), &["node", "drain", NODE]).await;
    stdout_of(&server.uri(), &["node", "maintenance", NODE]).await;
    stdout_of(&server.uri(), &["node", "resume", NODE]).await;
}

/// Decommissioning is the destructive one: the force flag must travel to the
/// API, because that is what overrides the durability safety check.
#[tokio::test]
async fn decommissioning_sends_the_force_flag_it_was_given() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/api/v1/nodes/{NODE}/decommission")))
        .and(body_json(json!({"force": false})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state": "decommissioned"})))
        .expect(1)
        .mount(&server)
        .await;
    stdout_of(&server.uri(), &["node", "decommission", NODE]).await;
    server.reset().await;

    Mock::given(method("POST"))
        .and(path(format!("/api/v1/nodes/{NODE}/decommission")))
        .and(body_json(json!({"force": true})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state": "decommissioned"})))
        .expect(1)
        .mount(&server)
        .await;
    stdout_of(&server.uri(), &["node", "decommission", NODE, "--force"]).await;
}

#[tokio::test]
async fn a_cluster_can_be_initialized_and_issue_a_join_token() {
    let server = MockServer::start().await;
    mock(
        &server,
        "POST",
        "/api/v1/cluster/init",
        200,
        json!({"cluster_id": "cluster-7", "initialized": true}),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/cluster/join-tokens"))
        .and(body_json(json!({
            "lifetime_seconds": 60,
            "description": "record-store node join",
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"token": "join-me"})))
        .expect(1)
        .mount(&server)
        .await;

    assert!(
        stdout_of(&server.uri(), &["cluster", "init"])
            .await
            .contains("cluster-7")
    );
    assert!(
        stdout_of(
            &server.uri(),
            &["cluster", "issue-join-token", "--lifetime-seconds", "60"]
        )
        .await
        .contains("join-me")
    );
}

/// Repair defaults to a dry run. Deleting payloads without `--apply` would be
/// destructive by default, so both forms are pinned.
#[tokio::test]
async fn storage_repair_is_a_dry_run_unless_apply_is_given() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/storage/repair"))
        .and(body_json(
            json!({"maximum_entries": 100_000, "dry_run": true}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"removed": 0})))
        .expect(1)
        .mount(&server)
        .await;
    stdout_of(&server.uri(), &["storage", "repair"]).await;
    server.reset().await;

    Mock::given(method("POST"))
        .and(path("/api/v1/storage/repair"))
        .and(body_json(json!({"maximum_entries": 5, "dry_run": false})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"removed": 2})))
        .expect(1)
        .mount(&server)
        .await;
    stdout_of(
        &server.uri(),
        &["storage", "repair", "--maximum-entries", "5", "--apply"],
    )
    .await;
}

#[tokio::test]
async fn a_webhook_document_is_posted_from_disk() {
    let server = MockServer::start().await;
    let directory = tempfile::tempdir().expect("temporary directory");
    let file = directory.path().join("webhook.json");
    let document = json!({"endpoint": "https://hooks.example", "secret": "shh"});
    std::fs::write(&file, document.to_string()).expect("write webhook");

    Mock::given(method("POST"))
        .and(path("/api/v1/webhooks"))
        .and(body_json(document))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": POLICY})))
        .expect(1)
        .mount(&server)
        .await;

    stdout_of(
        &server.uri(),
        &["webhook", "create", file.to_str().expect("path")],
    )
    .await;
}

/// `--json` has to change the shape of every command's output, not just the
/// ones that happen to return a document.
#[tokio::test]
async fn the_json_flag_produces_parseable_output_for_action_commands() {
    let server = MockServer::start().await;
    mock(
        &server,
        "DELETE",
        "/api/v1/buckets/photos",
        204,
        json!(null),
    )
    .await;

    let output = stdout_of(&server.uri(), &["--json", "bucket", "delete", "photos"]).await;
    let parsed: Value = serde_json::from_str(&output).expect("JSON output");
    assert_eq!(parsed["deleted"], "photos");
}
