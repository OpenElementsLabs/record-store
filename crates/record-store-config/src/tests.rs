use std::fs;
use std::path::PathBuf;

use tempfile::tempdir;

use super::*;

fn credentials() -> [(&'static str, &'static str); 2] {
    [
        ("RECORD_STORE_ROOT_ACCESS_KEY", "test-access"),
        (
            "RECORD_STORE_ROOT_SECRET_KEY",
            "test-secret-at-least-sixteen",
        ),
    ]
}

fn valid_config() -> Config {
    Config::load_with_environment(None, credentials()).expect("defaults must be valid")
}

#[test]
fn file_and_environment_overlay_defaults_in_order() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("record-store.toml");
    fs::write(
        &path,
        r#"
            [server]
            s3_bind = "127.0.0.1:7700"

            [storage]
            data_directory = "/srv/record-store"
        "#,
    )
    .expect("write configuration");
    let mut environment = credentials().to_vec();
    environment.push(("RECORD_STORE_API_BIND", "127.0.0.1:7701"));
    environment.push(("RECORD_STORE_LOG", "record_store=debug"));
    let config =
        Config::load_with_environment(Some(&path), environment).expect("valid configuration");
    assert_eq!(
        config.server.s3_bind,
        "127.0.0.1:7700".parse().expect("bind")
    );
    assert_eq!(
        config.server.api_bind,
        "127.0.0.1:7701".parse().expect("bind")
    );
    assert_eq!(
        config.storage.data_directory,
        PathBuf::from("/srv/record-store")
    );
    assert_eq!(config.observability.log_filter, "record_store=debug");
}

#[test]
fn defaults_use_record_store_ports_and_require_credentials() {
    let config = Config::default();
    assert_eq!(config.server.s3_bind.port(), 7_600);
    assert_eq!(config.server.api_bind.port(), 7_601);
    assert!(config.validate().is_err());
}

#[test]
fn secrets_are_redacted_from_debug_output() {
    let config = Config::load_with_environment(None, credentials()).expect("configuration");
    let debug = format!("{config:?}");
    assert!(!debug.contains("test-secret-at-least-sixteen"));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn metrics_use_a_dedicated_validated_secret() {
    let mut environment = credentials().to_vec();
    environment.push((
        "RECORD_STORE_METRICS_SCRAPE_TOKEN",
        "dedicated-test-metrics-token-at-least-32-bytes",
    ));
    let config =
        Config::load_with_environment(None, environment).expect("metrics token configuration");
    assert_eq!(
        config
            .auth
            .metrics_scrape_token
            .as_ref()
            .expect("configured metrics token")
            .expose(),
        "dedicated-test-metrics-token-at-least-32-bytes"
    );

    let mut duplicate = credentials().to_vec();
    duplicate.extend([
        (
            "RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN",
            "one-shared-token-that-is-at-least-32-bytes",
        ),
        (
            "RECORD_STORE_METRICS_SCRAPE_TOKEN",
            "one-shared-token-that-is-at-least-32-bytes",
        ),
    ]);
    assert!(matches!(
        Config::load_with_environment(None, duplicate),
        Err(ConfigError::Validation(message)) if message.contains("metrics_scrape_token")
    ));
}

#[test]
fn sharing_policy_is_configurable_from_file_and_environment() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("record-store.toml");
    fs::write(
        &path,
        r#"
            [sharing]
            require_expiration = true
            maximum_lifetime_days = 30
            share_base_url = "https://record-store.example.com/"
            embed_base_url = "https://storage.example.com/"
        "#,
    )
    .expect("write configuration");
    let mut environment = credentials().to_vec();
    environment.push(("RECORD_STORE_SHARING_EMBEDS_ENABLED", "false"));
    environment.push(("RECORD_STORE_SHARING_PASSWORD_ATTEMPTS_PER_MINUTE", "3"));
    let config =
        Config::load_with_environment(Some(&path), environment).expect("valid configuration");

    assert!(config.sharing.shares_enabled);
    assert!(!config.sharing.embeds_enabled);
    assert!(config.sharing.require_expiration);
    assert_eq!(config.sharing.maximum_lifetime_days, 30);
    assert_eq!(config.sharing.password_attempts_per_minute, 3);
    assert_eq!(
        config.sharing.normalized_share_base_url().as_deref(),
        Some("https://record-store.example.com")
    );
    // Embeds are published on the storage endpoint, never on the console:
    // a site loading an asset must not have to reach the management plane.
    assert_eq!(
        config.effective_embed_base_url(),
        "https://storage.example.com"
    );
}

#[test]
fn the_embed_address_falls_back_from_config_to_endpoint_to_listener() {
    let mut config = valid_config();
    assert_eq!(
        config.effective_embed_base_url(),
        "http://127.0.0.1:7600",
        "an unspecified bind address must be rendered as something reachable"
    );

    config.cluster.s3_endpoint = Some("storage.internal:7600".to_owned());
    assert_eq!(
        config.effective_embed_base_url(),
        "http://storage.internal:7600"
    );

    config.sharing.embed_base_url = Some("https://cdn.example.com/".to_owned());
    assert_eq!(config.effective_embed_base_url(), "https://cdn.example.com");
}

#[test]
fn sharing_defaults_are_permissive_but_bounded() {
    let config = valid_config();
    assert!(config.sharing.shares_enabled);
    assert!(config.sharing.embeds_enabled);
    assert_eq!(config.sharing.maximum_lifetime_days, 365);
    assert_eq!(config.sharing.preview_text_limit_bytes, 1024 * 1024);
    assert!(config.sharing.normalized_share_base_url().is_none());
    assert!(config.sharing.normalized_embed_base_url().is_none());
}

#[test]
fn unsafe_sharing_policy_values_are_refused_at_load() {
    for (name, value) in [
        ("RECORD_STORE_SHARING_MAXIMUM_ACCESS_COUNT", "0"),
        ("RECORD_STORE_SHARING_PASSWORD_ATTEMPTS_PER_MINUTE", "0"),
        ("RECORD_STORE_SHARING_TOKEN_PROBES_PER_MINUTE", "0"),
        ("RECORD_STORE_SHARING_UNLOCK_LIFETIME_HOURS", "0"),
        ("RECORD_STORE_SHARING_PREVIEW_TEXT_LIMIT_BYTES", "16"),
        ("RECORD_STORE_SHARING_MAXIMUM_LIFETIME_DAYS", "100000"),
        ("RECORD_STORE_SHARING_SHARE_BASE_URL", "javascript:alert(1)"),
        (
            "RECORD_STORE_SHARING_SHARE_BASE_URL",
            "record-store.example.com",
        ),
        ("RECORD_STORE_SHARING_EMBED_BASE_URL", "javascript:alert(1)"),
        ("RECORD_STORE_SHARING_EMBED_BASE_URL", "storage.example.com"),
    ] {
        let mut environment = credentials().to_vec();
        environment.push((name, value));
        assert!(
            matches!(
                Config::load_with_environment(None, environment),
                Err(ConfigError::Validation(_))
            ),
            "accepted unsafe sharing value {name}={value}"
        );
    }
}

#[test]
fn unknown_file_fields_and_invalid_environment_are_rejected() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("record-store.toml");
    fs::write(&path, "[server]\nsecret_backdoor = true\n").expect("write configuration");
    assert!(matches!(
        Config::load_with_environment(Some(&path), credentials()),
        Err(ConfigError::ParseFile { .. })
    ));
    let mut environment = credentials().to_vec();
    environment.push(("RECORD_STORE_S3_BIND", "not-an-address"));
    let error = Config::load_with_environment(None, environment).expect_err("invalid environment");
    assert!(error.to_string().contains("RECORD_STORE_S3_BIND"));
    assert!(!error.to_string().contains("test-secret"));
}

#[test]
fn temporary_directory_defaults_under_data_root() {
    let mut config = Config::default();
    config.storage.data_directory = PathBuf::from("state");
    assert_eq!(
        config.storage.effective_temporary_directory(),
        PathBuf::from("state/tmp")
    );
}

#[test]
fn object_encryption_requires_the_explicit_master_key() {
    let mut without_key = credentials().to_vec();
    without_key.push(("RECORD_STORE_STORAGE_ENCRYPTION_ENABLED", "true"));
    assert!(matches!(
        Config::load_with_environment(None, without_key),
        Err(ConfigError::Validation(message)) if message.contains("credential_master_key")
    ));

    let mut configured = credentials().to_vec();
    configured.push(("RECORD_STORE_STORAGE_ENCRYPTION_ENABLED", "true"));
    configured.push((
        "RECORD_STORE_CREDENTIAL_MASTER_KEY",
        "stable-test-master-key-at-least-thirty-two-bytes",
    ));
    let config = Config::load_with_environment(None, configured).expect("encrypted config");
    assert!(config.storage.encryption_enabled);
}

#[test]
fn default_listeners_use_the_documented_record_store_ports() {
    let server = ServerConfig::default();
    assert_eq!(server.s3_bind.port(), 7_600);
    assert_eq!(server.api_bind.port(), 7_601);
    assert_eq!(server.rpc_bind.port(), 7_603);
    assert_eq!(ServerConfig::RESERVED_CONSOLE_PORT, 7_602);
    for port in [
        server.s3_bind.port(),
        server.api_bind.port(),
        server.rpc_bind.port(),
    ] {
        assert_ne!(
            port, 9_000,
            "Record Store must not default to another product's port"
        );
        assert_ne!(
            port, 9_001,
            "Record Store must not default to another product's port"
        );
    }
    assert_eq!(server.mode, DeploymentMode::Standalone);
    assert_eq!(
        server.effective_rpc_advertise(),
        server.rpc_bind.to_string()
    );
}

#[test]
fn listeners_must_be_distinct_and_avoid_the_reserved_console_port() {
    let mut config = valid_config();
    config.server.rpc_bind = config.server.api_bind;
    assert!(config.validate().is_err());

    let mut config = valid_config();
    config.server.rpc_bind = "0.0.0.0:7602".parse().expect("address");
    let error = config
        .validate()
        .expect_err("the reserved console port must be refused");
    assert!(error.to_string().contains("7602"));
}

#[test]
fn cluster_settings_are_validated_strictly() {
    let mut config = valid_config();
    config.cluster.replication_factor = 4;
    assert!(config.validate().is_err());

    let mut config = valid_config();
    config.cluster.storage_class = "NVMe".to_owned();
    assert!(config.validate().is_err());

    let mut config = valid_config();
    config.cluster.failure_domain = "rack".to_owned();
    assert!(config.validate().is_err());

    let mut config = valid_config();
    config.cluster.election_timeout_min_millis = 100;
    assert!(config.validate().is_err());

    let mut config = valid_config();
    config.server.mode = DeploymentMode::Control;
    let error = config
        .validate()
        .expect_err("a control process without seeds cannot reach the cluster");
    assert!(error.to_string().contains("cluster.seeds"));

    let mut config = valid_config();
    config.cluster.tls.certificate_path = Some(PathBuf::from("/tmp/cert.pem"));
    assert!(config.validate().is_err());
}

#[test]
fn cluster_environment_overrides_are_applied() {
    let config = Config::load_with_environment(
        None,
        [
            ("RECORD_STORE_ROOT_ACCESS_KEY", "root-access"),
            (
                "RECORD_STORE_ROOT_SECRET_KEY",
                "root-secret-at-least-sixteen",
            ),
            ("RECORD_STORE_MODE", "cluster"),
            ("RECORD_STORE_RPC_BIND", "0.0.0.0:17603"),
            ("RECORD_STORE_RPC_ADVERTISE", "10.0.1.12:17603"),
            (
                "RECORD_STORE_CLUSTER_SEEDS",
                "storage-1:7603, storage-2:7603",
            ),
            ("RECORD_STORE_CLUSTER_JOIN_TOKEN", "recordstorejoin.token"),
            ("RECORD_STORE_CLUSTER_STORAGE_CLASS", "nvme"),
            ("RECORD_STORE_CLUSTER_FAILURE_DOMAIN", "rack=r1,zone=dc1"),
            ("RECORD_STORE_CLUSTER_REPLICATION_FACTOR", "2"),
            ("RECORD_STORE_CLUSTER_CAPACITY_LOW_WATERMARK_PERCENT", "70"),
            ("RECORD_STORE_CLUSTER_CAPACITY_HIGH_WATERMARK_PERCENT", "80"),
            (
                "RECORD_STORE_CLUSTER_CAPACITY_CRITICAL_WATERMARK_PERCENT",
                "90",
            ),
        ],
    )
    .expect("configuration must load");
    assert_eq!(config.server.mode, DeploymentMode::Cluster);
    assert_eq!(config.server.rpc_bind.port(), 17_603);
    assert_eq!(
        config.server.effective_rpc_advertise(),
        "10.0.1.12:17603",
        "an advertise address must not be assumed equal to the bind address"
    );
    assert_eq!(config.cluster.seeds.len(), 2);
    assert_eq!(config.cluster.storage_class, "nvme");
    assert_eq!(config.cluster.replication_factor, 2);
    assert_eq!(config.cluster.capacity_low_watermark_percent, 70);
    assert_eq!(config.cluster.capacity_high_watermark_percent, 80);
    assert_eq!(config.cluster.capacity_critical_watermark_percent, 90);
    assert!(format!("{:?}", config.cluster.join_token).contains("redacted"));
}
