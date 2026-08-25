//! End-to-end behaviour of preview, share links, and embed links.
//!
//! These run against a real server process with real durable state, because the
//! properties that matter here are not properties of a function: that a revoked
//! token stops working on the *next request*, that a range response really is a
//! range, that a hostile media type never reaches a browser inline, and that a
//! public visitor learns nothing about the object they failed to open.

use std::net::SocketAddr;

use oes_config::{Config, SecretValue};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use tokio::{net::TcpListener, sync::oneshot};

const ADMIN: &str = "test-system-management-token-32-bytes-long";
const AUDITOR: &str = "test-auditor-management-token-32-bytes-long";

/// A single-byte transparent GIF, used wherever a real image is needed.
const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;";

/// The smallest thing a browser will accept as a PDF.
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";

struct Harness {
    address: SocketAddr,
    /// The storage data plane, where embed URLs are published.
    s3_address: SocketAddr,
    client: Client,
    shutdown: Option<oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<Result<(), oes_server::StartupError>>,
    _directory: TempDir,
}

impl Harness {
    async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    async fn start_with(customise: impl FnOnce(&mut Config)) -> Self {
        let directory = tempdir().expect("temporary directory");
        // The listeners are bound before the configuration is finalised so the
        // configured addresses match the ones actually served. Embed URLs are
        // derived from the S3 listener address, and a test whose config claimed
        // a different port would be asserting against a link that resolves
        // nowhere.
        let api_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let address = api_listener.local_addr().expect("listener address");
        let s3_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind S3 listener");
        let s3_address = s3_listener.local_addr().expect("S3 listener address");

        let mut config = Config::default();
        config.server.api_bind = address;
        config.server.s3_bind = s3_address;
        config.storage.data_directory = directory.path().join("data");
        config.server.shutdown_grace_period_seconds = 2;
        config.auth.root_access_key = Some("test-access".into());
        config.auth.root_secret_key = Some(SecretValue::new("test-secret-at-least-sixteen"));
        config.auth.credential_master_key = Some(SecretValue::new(
            "test-credential-master-key-at-least-32-bytes",
        ));
        config.auth.management_system_token = Some(SecretValue::new(ADMIN));
        config.auth.management_auditor_token = Some(SecretValue::new(AUDITOR));
        customise(&mut config);

        let runtime = oes_server::initialize(&config)
            .await
            .expect("initialize server");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(runtime.serve(s3_listener, api_listener, async move {
            let _ = shutdown_rx.await;
        }));
        Self {
            address,
            s3_address,
            client: Client::new(),
            shutdown: Some(shutdown_tx),
            server,
            _directory: directory,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    /// An embed path on the storage listener, which is where embeds are served.
    fn embed_url(&self, token: &str) -> String {
        format!("http://{}/e/{token}", self.s3_address)
    }

    async fn create_bucket(&self, name: &str) {
        let created = self
            .client
            .post(self.url("/api/v1/buckets"))
            .bearer_auth(ADMIN)
            .json(&json!({ "name": name }))
            .send()
            .await
            .expect("create bucket");
        assert_eq!(created.status(), StatusCode::CREATED, "creating {name}");
    }

    async fn upload(&self, bucket: &str, key: &str, content_type: &str, body: &[u8]) -> Value {
        let response = self
            .client
            .put(self.url(&format!("/api/v1/buckets/{bucket}/object/{key}")))
            .bearer_auth(ADMIN)
            .header("content-type", content_type)
            .body(body.to_vec())
            .send()
            .await
            .expect("upload object");
        assert_eq!(response.status(), StatusCode::CREATED, "uploading {key}");
        response.json().await.expect("object JSON")
    }

    async fn create_share(&self, bucket: &str, key: &str, body: Value) -> Value {
        let response = self
            .client
            .post(self.url(&format!("/api/v1/buckets/{bucket}/object-shares/{key}")))
            .bearer_auth(ADMIN)
            .json(&body)
            .send()
            .await
            .expect("create share");
        assert_eq!(response.status(), StatusCode::CREATED, "creating a share");
        response.json().await.expect("share JSON")
    }

    async fn create_embed(&self, bucket: &str, key: &str, body: Value) -> reqwest::Response {
        self.client
            .post(self.url(&format!("/api/v1/buckets/{bucket}/object-embeds/{key}")))
            .bearer_auth(ADMIN)
            .json(&body)
            .send()
            .await
            .expect("create embed")
    }

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.server.await;
    }
}

/// Extracts the opaque token from a capability URL.
fn token_of(url: &str) -> &str {
    url.rsplit('/').next().expect("a token segment")
}

#[tokio::test]
async fn a_share_link_serves_the_object_and_stops_the_moment_it_is_revoked() {
    let oes = Harness::start().await;
    oes.create_bucket("share-bucket").await;
    oes.upload(
        "share-bucket",
        "reports/summary.txt",
        "text/plain",
        b"quarterly summary\n",
    )
    .await;

    let issued = oes
        .create_share(
            "share-bucket",
            "reports/summary.txt",
            json!({ "label": "Board review" }),
        )
        .await;
    let url = issued["url"].as_str().expect("share URL");
    let token = token_of(url);
    let share_id = issued["share"]["id"].as_str().expect("share id");

    // The URL the API returns must be opaque: no bucket, no key, no version.
    assert!(
        !url.contains("share-bucket"),
        "the URL names the bucket: {url}"
    );
    assert!(!url.contains("summary"), "the URL names the object: {url}");
    assert_eq!(token.len(), 43, "capability tokens carry 256 bits: {token}");

    let descriptor: Value = oes
        .client
        .get(oes.url(&format!("/s/{token}")))
        .send()
        .await
        .expect("share descriptor")
        .json()
        .await
        .expect("descriptor JSON");
    assert_eq!(descriptor["state"], "open");
    assert_eq!(descriptor["file_name"], "summary.txt");
    assert_eq!(descriptor["preview"], "text");
    assert_eq!(descriptor["can_view"], true);
    assert_eq!(descriptor["can_download"], true);
    // Nothing internal is disclosed to a recipient.
    for leak in ["bucket", "key", "version_id", "checksum", "etag", "id"] {
        assert!(
            descriptor.get(leak).is_none(),
            "the public descriptor exposes {leak}: {descriptor}"
        );
    }

    let content = oes
        .client
        .get(oes.url(&format!("/s/{token}/content")))
        .send()
        .await
        .expect("share content");
    assert_eq!(content.status(), StatusCode::OK);
    assert_eq!(
        content
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(
        content
            .headers()
            .get("x-content-type-options")
            .and_then(|value| value.to_str().ok()),
        Some("nosniff")
    );
    // A revocable link must not be cached anywhere on the way back.
    let cache = content
        .headers()
        .get("cache-control")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(cache.contains("no-store"), "share cache policy was {cache}");
    let policy = content
        .headers()
        .get("content-security-policy")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        policy.contains("sandbox"),
        "share content policy was {policy}"
    );
    assert_eq!(content.text().await.expect("body"), "quarterly summary\n");

    // Downloading is a different response from viewing the same object.
    let download = oes
        .client
        .get(oes.url(&format!("/s/{token}/content?download=true")))
        .send()
        .await
        .expect("share download");
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(
        download
            .headers()
            .get("content-disposition")
            .and_then(|value| value.to_str().ok()),
        Some("attachment; filename=\"summary.txt\"")
    );

    let revoked = oes
        .client
        .post(oes.url(&format!("/api/v1/shares/{share_id}/revoke")))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("revoke share");
    assert_eq!(revoked.status(), StatusCode::OK);

    // The critical invariant: revocation is authoritative on the next request.
    for path in [
        format!("/s/{token}"),
        format!("/s/{token}/content"),
        format!("/s/{token}/content?download=true"),
    ] {
        let refused = oes
            .client
            .get(oes.url(&path))
            .send()
            .await
            .expect("request after revocation");
        assert_eq!(
            refused.status(),
            StatusCode::NOT_FOUND,
            "{path} still worked after revocation"
        );
    }

    oes.stop().await;
}

#[tokio::test]
async fn an_unknown_token_is_answered_exactly_like_a_revoked_one() {
    let oes = Harness::start().await;
    oes.create_bucket("indistinguishable").await;
    oes.upload("indistinguishable", "note.txt", "text/plain", b"secret")
        .await;
    let issued = oes
        .create_share("indistinguishable", "note.txt", json!({ "label": "Gone" }))
        .await;
    let token = token_of(issued["url"].as_str().expect("url")).to_owned();
    let share_id = issued["share"]["id"].as_str().expect("id").to_owned();
    oes.client
        .post(oes.url(&format!("/api/v1/shares/{share_id}/revoke")))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("revoke");

    let stranger = "A".repeat(43);
    let revoked_body = oes
        .client
        .get(oes.url(&format!("/s/{token}")))
        .send()
        .await
        .expect("revoked share")
        .text()
        .await
        .expect("body");
    let unknown_body = oes
        .client
        .get(oes.url(&format!("/s/{stranger}")))
        .send()
        .await
        .expect("unknown share")
        .text()
        .await
        .expect("body");

    // The bodies differ only in their request identifier. Confirming that a
    // guessed token names a real link is most of what a prober wanted.
    let strip = |body: &str| {
        serde_json::from_str::<Value>(body).map(|mut value| {
            if let Some(error) = value.get_mut("error") {
                error
                    .as_object_mut()
                    .map(|object| object.remove("request_id"));
            }
            value
        })
    };
    assert_eq!(
        strip(&revoked_body).expect("revoked JSON"),
        strip(&unknown_body).expect("unknown JSON")
    );

    oes.stop().await;
}

#[tokio::test]
async fn an_expired_share_and_an_exhausted_one_both_stop_working() {
    let oes = Harness::start().await;
    oes.create_bucket("limits").await;
    oes.upload("limits", "contract.txt", "text/plain", b"signed\n")
        .await;

    let expired = oes
        .create_share(
            "limits",
            "contract.txt",
            json!({
                "label": "Already gone",
                "expires_at": (chrono::Utc::now() + chrono::Duration::seconds(1)).to_rfc3339(),
            }),
        )
        .await;
    let expiring_token = token_of(expired["url"].as_str().expect("url")).to_owned();

    let budgeted = oes
        .create_share(
            "limits",
            "contract.txt",
            json!({ "label": "Two opens", "maximum_access_count": 2 }),
        )
        .await;
    let budgeted_token = token_of(budgeted["url"].as_str().expect("url")).to_owned();

    for attempt in 0..2 {
        let response = oes
            .client
            .get(oes.url(&format!("/s/{budgeted_token}/content")))
            .send()
            .await
            .expect("budgeted content");
        assert_eq!(response.status(), StatusCode::OK, "delivery {attempt}");
        // A budget is only strict if ranges cannot be used to take the object a
        // slice at a time, so a budgeted share refuses them outright.
        assert_eq!(
            response
                .headers()
                .get("accept-ranges")
                .and_then(|value| value.to_str().ok()),
            Some("none")
        );
    }
    let spent = oes
        .client
        .get(oes.url(&format!("/s/{budgeted_token}/content")))
        .send()
        .await
        .expect("third delivery");
    assert_eq!(spent.status(), StatusCode::NOT_FOUND);

    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
    let stale = oes
        .client
        .get(oes.url(&format!("/s/{expiring_token}/content")))
        .send()
        .await
        .expect("expired content");
    assert_eq!(stale.status(), StatusCode::NOT_FOUND);

    oes.stop().await;
}

#[tokio::test]
async fn a_password_protected_share_discloses_nothing_until_it_is_unlocked() {
    let oes = Harness::start().await;
    oes.create_bucket("locked").await;
    oes.upload("locked", "salaries.txt", "text/plain", b"confidential\n")
        .await;

    let issued = oes
        .create_share(
            "locked",
            "salaries.txt",
            json!({ "label": "Payroll", "password": "correct horse battery" }),
        )
        .await;
    let token = token_of(issued["url"].as_str().expect("url")).to_owned();

    let challenge: Value = oes
        .client
        .get(oes.url(&format!("/s/{token}")))
        .send()
        .await
        .expect("challenge")
        .json()
        .await
        .expect("challenge JSON");
    assert_eq!(challenge["state"], "password_required");
    // Not even the file name is disclosed before the password is verified.
    assert!(challenge["file_name"].is_null(), "{challenge}");
    assert!(challenge["size"].is_null(), "{challenge}");

    let unauthorized = oes
        .client
        .get(oes.url(&format!("/s/{token}/content")))
        .send()
        .await
        .expect("content without a password");
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let wrong = oes
        .client
        .post(oes.url(&format!("/s/{token}/unlock")))
        .json(&json!({ "password": "not the password" }))
        .send()
        .await
        .expect("wrong password");
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let unlocked: Value = oes
        .client
        .post(oes.url(&format!("/s/{token}/unlock")))
        .json(&json!({ "password": "correct horse battery" }))
        .send()
        .await
        .expect("correct password")
        .json()
        .await
        .expect("ticket JSON");
    let ticket = unlocked["ticket"].as_str().expect("ticket").to_owned();

    let content = oes
        .client
        .get(oes.url(&format!("/s/{token}/content")))
        .header("x-oes-share-ticket", &ticket)
        .send()
        .await
        .expect("content with a ticket");
    assert_eq!(content.status(), StatusCode::OK);
    assert_eq!(content.text().await.expect("body"), "confidential\n");

    // The descriptor opens too, so the page that just verified the password does
    // not challenge the visitor again on the very next request.
    let unlocked_descriptor: Value = oes
        .client
        .get(oes.url(&format!("/s/{token}")))
        .header("x-oes-share-ticket", &ticket)
        .send()
        .await
        .expect("descriptor with a ticket")
        .json()
        .await
        .expect("descriptor JSON");
    assert_eq!(unlocked_descriptor["state"], "open");
    assert_eq!(unlocked_descriptor["file_name"], "salaries.txt");

    // A ticket is not a password: revocation still ends it immediately.
    let share_id = issued["share"]["id"].as_str().expect("id").to_owned();
    oes.client
        .post(oes.url(&format!("/api/v1/shares/{share_id}/revoke")))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("revoke");
    let after = oes
        .client
        .get(oes.url(&format!("/s/{token}/content")))
        .header("x-oes-share-ticket", &ticket)
        .send()
        .await
        .expect("content after revocation");
    assert_eq!(after.status(), StatusCode::NOT_FOUND);

    oes.stop().await;
}

#[tokio::test]
async fn repeated_password_guesses_are_throttled_without_locking_the_link() {
    let oes = Harness::start_with(|config| {
        config.sharing.password_attempts_per_minute = 3;
    })
    .await;
    oes.create_bucket("bruteforce").await;
    oes.upload("bruteforce", "vault.txt", "text/plain", b"nothing here\n")
        .await;
    let issued = oes
        .create_share(
            "bruteforce",
            "vault.txt",
            json!({ "label": "Guarded", "password": "correct horse battery" }),
        )
        .await;
    let token = token_of(issued["url"].as_str().expect("url")).to_owned();

    let mut throttled = false;
    for attempt in 0..6 {
        let response = oes
            .client
            .post(oes.url(&format!("/s/{token}/unlock")))
            .json(&json!({ "password": format!("guess-{attempt}") }))
            .send()
            .await
            .expect("guess");
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            assert!(
                response.headers().contains_key("retry-after"),
                "a throttled response must say when to retry"
            );
            throttled = true;
            break;
        }
    }
    assert!(throttled, "password guessing was never throttled");

    oes.stop().await;
}

#[tokio::test]
async fn a_share_serves_byte_ranges_so_media_can_be_seeked() {
    let oes = Harness::start().await;
    oes.create_bucket("ranges").await;
    let body: Vec<u8> = (0..4096_u32).map(|index| (index % 251) as u8).collect();
    oes.upload("ranges", "clip.mp4", "video/mp4", &{
        // A real MP4 signature, so the media type is corroborated by the bytes.
        let mut bytes = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom".to_vec();
        bytes.extend_from_slice(&body);
        bytes
    })
    .await;

    let issued = oes
        .create_share("ranges", "clip.mp4", json!({ "label": "Screening" }))
        .await;
    let token = token_of(issued["url"].as_str().expect("url")).to_owned();

    let full = oes
        .client
        .get(oes.url(&format!("/s/{token}/content")))
        .send()
        .await
        .expect("full read");
    assert_eq!(full.status(), StatusCode::OK);
    assert_eq!(
        full.headers()
            .get("accept-ranges")
            .and_then(|value| value.to_str().ok()),
        Some("bytes")
    );
    let complete = full.bytes().await.expect("full body");

    let partial = oes
        .client
        .get(oes.url(&format!("/s/{token}/content")))
        .header("range", "bytes=100-199")
        .send()
        .await
        .expect("partial read");
    assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        partial
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok()),
        Some(format!("bytes 100-199/{}", complete.len()).as_str())
    );
    assert_eq!(
        partial
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok()),
        Some("100")
    );
    let slice = partial.bytes().await.expect("partial body");
    assert_eq!(slice.len(), 100);
    // The served bytes are the authorized bytes, not merely the right count.
    assert_eq!(&slice[..], &complete[100..200]);

    oes.stop().await;
}

#[tokio::test]
async fn stored_active_content_is_never_served_inline() {
    let oes = Harness::start().await;
    oes.create_bucket("hostile").await;

    // Three shapes of the same attack: content declared as what it is, content
    // declared as something harmless, and script-like text.
    oes.upload(
        "hostile",
        "page.html",
        "text/html",
        b"<script>alert(document.domain)</script>",
    )
    .await;
    oes.upload(
        "hostile",
        "drawing.svg",
        "image/svg+xml",
        b"<svg xmlns=\"http://www.w3.org/2000/svg\" onload=\"alert(1)\"></svg>",
    )
    .await;
    oes.upload(
        "hostile",
        "disguised.png",
        "image/png",
        b"<html><body><script>alert(1)</script></body></html>",
    )
    .await;

    for key in ["page.html", "drawing.svg", "disguised.png"] {
        let preview = oes
            .client
            .get(oes.url(&format!("/api/v1/buckets/hostile/object-preview/{key}")))
            .bearer_auth(ADMIN)
            .send()
            .await
            .expect("preview attempt");
        assert_eq!(
            preview.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{key} was previewed inline"
        );

        let issued = oes
            .create_share("hostile", key, json!({ "label": key }))
            .await;
        let token = token_of(issued["url"].as_str().expect("url")).to_owned();

        let inline = oes
            .client
            .get(oes.url(&format!("/s/{token}/content")))
            .send()
            .await
            .expect("inline share attempt");
        assert_eq!(
            inline.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{key} was shared inline"
        );

        // Downloading is still offered: the bytes are the operator's, and an
        // attachment is not an interpretation.
        let download = oes
            .client
            .get(oes.url(&format!("/s/{token}/content?download=true")))
            .send()
            .await
            .expect("download attempt");
        assert_eq!(
            download.status(),
            StatusCode::OK,
            "{key} could not be downloaded"
        );
        let headers = download.headers().clone();
        assert_eq!(
            headers
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/octet-stream"),
            "{key} was downloaded as an interpretable type"
        );
        assert!(
            headers
                .get("content-disposition")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("attachment")),
            "{key} was not offered as an attachment"
        );
        assert_eq!(
            headers
                .get("x-content-type-options")
                .and_then(|value| value.to_str().ok()),
            Some("nosniff")
        );
    }

    // Creating an inline embed of active content is refused outright.
    let refused = oes
        .create_embed(
            "hostile",
            "page.html",
            json!({ "label": "Malicious", "disposition": "inline" }),
        )
        .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    oes.stop().await;
}

#[tokio::test]
async fn an_embed_honours_its_origin_allowlist_precisely() {
    let oes = Harness::start().await;
    oes.create_bucket("assets").await;
    oes.upload("assets", "brand/logo.gif", "image/gif", GIF)
        .await;

    let issued: Value = oes
        .create_embed(
            "assets",
            "brand/logo.gif",
            json!({
                "label": "Company website",
                "allowed_origins": ["https://example.com"],
            }),
        )
        .await
        .json()
        .await
        .expect("embed JSON");
    let token = token_of(issued["url"].as_str().expect("url")).to_owned();
    let embed_id = issued["embed"]["id"].as_str().expect("id").to_owned();

    // An allowed origin is granted, and the value echoed back is the stored,
    // normalized one rather than whatever the caller sent.
    let allowed = oes
        .client
        .get(oes.embed_url(&token))
        .header("origin", "https://example.com:443")
        .send()
        .await
        .expect("allowed origin");
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(
        allowed
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://example.com")
    );
    assert_eq!(
        allowed
            .headers()
            .get("vary")
            .and_then(|value| value.to_str().ok()),
        Some("Origin")
    );
    assert_eq!(
        allowed
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("image/gif")
    );
    assert_eq!(allowed.bytes().await.expect("bytes").as_ref(), GIF);

    let denied = oes
        .client
        .get(oes.embed_url(&token))
        .header("origin", "https://evil.test")
        .send()
        .await
        .expect("denied origin");
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert!(
        denied
            .headers()
            .get("access-control-allow-origin")
            .is_none(),
        "a denied origin must not receive a grant"
    );

    // A non-browser client presents no origin. It is served, without a grant.
    let anonymous = oes
        .client
        .get(oes.embed_url(&token))
        .send()
        .await
        .expect("no origin");
    assert_eq!(anonymous.status(), StatusCode::OK);
    assert!(
        anonymous
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );

    // A preflight answers with the same decision.
    let preflight = oes
        .client
        .request(reqwest::Method::OPTIONS, oes.embed_url(&token))
        .header("origin", "https://example.com")
        .send()
        .await
        .expect("preflight");
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        preflight
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://example.com")
    );
    assert!(
        preflight
            .headers()
            .get("access-control-allow-headers")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("range")),
        "an embedded media element must be allowed to send Range"
    );

    // Narrowing is an edit; removing every restriction is not.
    let broadened = oes
        .client
        .patch(oes.url(&format!("/api/v1/embeds/{embed_id}")))
        .bearer_auth(ADMIN)
        .json(&json!({ "allowed_origins": [] }))
        .send()
        .await
        .expect("broaden attempt");
    assert_eq!(broadened.status(), StatusCode::BAD_REQUEST);

    let narrowed = oes
        .client
        .patch(oes.url(&format!("/api/v1/embeds/{embed_id}")))
        .bearer_auth(ADMIN)
        .json(&json!({ "allowed_origins": ["https://app.example.com"] }))
        .send()
        .await
        .expect("narrow");
    assert_eq!(narrowed.status(), StatusCode::OK);
    let refused = oes
        .client
        .get(oes.embed_url(&token))
        .header("origin", "https://example.com")
        .send()
        .await
        .expect("previously allowed origin");
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    // Malformed origins never reach storage.
    for hostile in [
        "javascript:alert(1)",
        "data:text/html,x",
        "file:///etc/passwd",
    ] {
        let rejected = oes
            .client
            .patch(oes.url(&format!("/api/v1/embeds/{embed_id}")))
            .bearer_auth(ADMIN)
            .json(&json!({ "allowed_origins": [hostile] }))
            .send()
            .await
            .expect("hostile origin");
        assert_eq!(
            rejected.status(),
            StatusCode::BAD_REQUEST,
            "accepted {hostile}"
        );
    }

    let revoked = oes
        .client
        .post(oes.url(&format!("/api/v1/embeds/{embed_id}/revoke")))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("revoke embed");
    assert_eq!(revoked.status(), StatusCode::OK);
    let after = oes
        .client
        .get(oes.embed_url(&token))
        .header("origin", "https://app.example.com")
        .send()
        .await
        .expect("after revocation");
    assert_eq!(after.status(), StatusCode::NOT_FOUND);

    oes.stop().await;
}

#[tokio::test]
async fn an_unrestricted_embed_says_so_rather_than_reflecting_an_origin() {
    let oes = Harness::start().await;
    oes.create_bucket("public-assets").await;
    oes.upload("public-assets", "logo.gif", "image/gif", GIF)
        .await;

    let issued: Value = oes
        .create_embed("public-assets", "logo.gif", json!({ "label": "Anywhere" }))
        .await
        .json()
        .await
        .expect("embed JSON");
    let token = token_of(issued["url"].as_str().expect("url")).to_owned();

    let response = oes
        .client
        .get(oes.embed_url(&token))
        .header("origin", "https://any-site.test")
        .send()
        .await
        .expect("unrestricted embed");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("*"),
        "an unrestricted embed states the wildcard rather than reflecting a caller's origin"
    );
    let cache = response
        .headers()
        .get("cache-control")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    // Cached, but briefly: a revocable capability must not outlive its
    // revocation by a year in somebody's CDN.
    assert!(
        cache.contains("max-age=60"),
        "embed cache policy was {cache}"
    );

    // A validator is offered so a page reload revalidates instead of refetching.
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("an ETag")
        .to_owned();
    let revalidated = oes
        .client
        .get(oes.embed_url(&token))
        .header("if-none-match", etag)
        .send()
        .await
        .expect("revalidation");
    assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);

    oes.stop().await;
}

#[tokio::test]
async fn a_capability_targets_exactly_the_version_it_was_created_for() {
    let oes = Harness::start().await;
    oes.create_bucket("versioned").await;
    let versioning = oes
        .client
        .put(oes.url("/api/v1/buckets/versioned/versioning"))
        .bearer_auth(ADMIN)
        .json(&json!({ "versioning": "enabled" }))
        .send()
        .await
        .expect("enable versioning");
    assert!(
        versioning.status().is_success(),
        "{:?}",
        versioning.status()
    );

    let first = oes
        .upload("versioned", "contract.txt", "text/plain", b"first draft\n")
        .await;
    let pinned_version = first["version_id"].as_str().expect("version").to_owned();

    let pinned = oes
        .create_share(
            "versioned",
            "contract.txt",
            json!({ "label": "Signed contract", "version_id": pinned_version }),
        )
        .await;
    let following = oes
        .create_share(
            "versioned",
            "contract.txt",
            json!({ "label": "Living document" }),
        )
        .await;
    let pinned_token = token_of(pinned["url"].as_str().expect("url")).to_owned();
    let following_token = token_of(following["url"].as_str().expect("url")).to_owned();
    assert_eq!(pinned["share"]["version_mode"], "pinned");
    assert_eq!(following["share"]["version_mode"], "current");

    oes.upload("versioned", "contract.txt", "text/plain", b"second draft\n")
        .await;

    let from_pinned = oes
        .client
        .get(oes.url(&format!("/s/{pinned_token}/content")))
        .send()
        .await
        .expect("pinned content")
        .text()
        .await
        .expect("body");
    assert_eq!(
        from_pinned, "first draft\n",
        "a pinned share served the current version"
    );

    let from_following = oes
        .client
        .get(oes.url(&format!("/s/{following_token}/content")))
        .send()
        .await
        .expect("following content")
        .text()
        .await
        .expect("body");
    assert_eq!(from_following, "second draft\n");

    // The console's preview must target the requested version too.
    let historical = oes
        .client
        .get(oes.url(&format!(
            "/api/v1/buckets/versioned/object-preview/contract.txt?version_id={pinned_version}"
        )))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("historical preview")
        .text()
        .await
        .expect("body");
    assert_eq!(historical, "first draft\n");

    // Deleting the key leaves a delete marker. The pinned capability still
    // resolves; the one that follows the current version does not.
    let deleted = oes
        .client
        .delete(oes.url("/api/v1/buckets/versioned/object/contract.txt"))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("delete object");
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let after_delete = oes
        .client
        .get(oes.url(&format!("/s/{following_token}/content")))
        .send()
        .await
        .expect("following after delete");
    assert_eq!(after_delete.status(), StatusCode::NOT_FOUND);

    let still_pinned = oes
        .client
        .get(oes.url(&format!("/s/{pinned_token}/content")))
        .send()
        .await
        .expect("pinned after delete")
        .text()
        .await
        .expect("body");
    assert_eq!(
        still_pinned, "first draft\n",
        "a pinned version behind a delete marker is still readable"
    );

    oes.stop().await;
}

#[tokio::test]
async fn a_following_embed_refuses_to_serve_a_type_it_was_not_created_for() {
    let oes = Harness::start().await;
    oes.create_bucket("mutable").await;
    oes.upload("mutable", "asset", "image/gif", GIF).await;

    let issued: Value = oes
        .create_embed("mutable", "asset", json!({ "label": "Site logo" }))
        .await
        .json()
        .await
        .expect("embed JSON");
    let token = token_of(issued["url"].as_str().expect("url")).to_owned();
    assert_eq!(
        oes.client
            .get(oes.embed_url(&token))
            .send()
            .await
            .expect("initial embed")
            .status(),
        StatusCode::OK
    );

    // The object behind the key becomes something that must not be rendered.
    oes.upload(
        "mutable",
        "asset",
        "text/html",
        b"<script>alert(1)</script>",
    )
    .await;

    let after = oes
        .client
        .get(oes.embed_url(&token))
        .send()
        .await
        .expect("embed after replacement");
    assert_eq!(
        after.status(),
        StatusCode::CONFLICT,
        "an embed served a media type it was never approved for"
    );

    oes.stop().await;
}

#[tokio::test]
async fn preview_serves_only_types_it_can_corroborate() {
    let oes = Harness::start().await;
    oes.create_bucket("previews").await;
    oes.upload("previews", "photo.gif", "image/gif", GIF).await;
    oes.upload("previews", "report.pdf", "application/pdf", PDF)
        .await;
    oes.upload("previews", "config.json", "application/json", b"{\"a\":1}")
        .await;
    oes.upload(
        "previews",
        "blob.bin",
        "application/octet-stream",
        b"\x00\x01\x02",
    )
    .await;

    for (key, expected) in [
        ("photo.gif", "image/gif"),
        ("report.pdf", "application/pdf"),
        ("config.json", "application/json; charset=utf-8"),
    ] {
        let preview = oes
            .client
            .get(oes.url(&format!("/api/v1/buckets/previews/object-preview/{key}")))
            .bearer_auth(ADMIN)
            .send()
            .await
            .expect("preview");
        assert_eq!(preview.status(), StatusCode::OK, "previewing {key}");
        assert_eq!(
            preview
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some(expected)
        );
        assert_eq!(
            preview
                .headers()
                .get("content-disposition")
                .and_then(|value| value.to_str().ok()),
            Some("inline")
        );
        let policy = preview
            .headers()
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(policy.contains("sandbox"), "{key} policy was {policy}");
    }

    let unsupported = oes
        .client
        .get(oes.url("/api/v1/buckets/previews/object-preview/blob.bin"))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("unsupported preview");
    assert_eq!(unsupported.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // A download of the same object is still an attachment, unchanged.
    let download = oes
        .client
        .get(oes.url("/api/v1/buckets/previews/object-content/blob.bin"))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("download");
    assert_eq!(download.status(), StatusCode::OK);
    assert!(
        download
            .headers()
            .get("content-disposition")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("attachment")),
        "the download path stopped producing attachments"
    );

    let missing = oes
        .client
        .get(oes.url("/api/v1/buckets/previews/object-preview/absent.txt"))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("missing preview");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    oes.stop().await;
}

#[tokio::test]
async fn capability_administration_requires_the_right_role_and_never_leaks_a_token() {
    let oes = Harness::start().await;
    oes.create_bucket("governed").await;
    oes.upload("governed", "note.txt", "text/plain", b"hello\n")
        .await;

    // An auditor may not mint a capability.
    let refused = oes
        .client
        .post(oes.url("/api/v1/buckets/governed/object-shares/note.txt"))
        .bearer_auth(AUDITOR)
        .json(&json!({ "label": "Not allowed" }))
        .send()
        .await
        .expect("auditor create");
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    let issued = oes
        .create_share("governed", "note.txt", json!({ "label": "Reviewed" }))
        .await;
    let share_id = issued["share"]["id"].as_str().expect("id").to_owned();
    let url = issued["url"].as_str().expect("url").to_owned();

    // Listings and detail responses carry no token at all.
    let listed = oes
        .client
        .get(oes.url("/api/v1/buckets/governed/object-shares/note.txt"))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("list shares")
        .text()
        .await
        .expect("body");
    assert!(
        !listed.contains(token_of(&url)),
        "a share listing carried a live capability token"
    );
    let detail = oes
        .client
        .get(oes.url(&format!("/api/v1/shares/{share_id}")))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("share detail")
        .text()
        .await
        .expect("body");
    assert!(
        !detail.contains(token_of(&url)),
        "a share detail response carried a live capability token"
    );

    // An auditor may read the metadata but never the URL.
    let audited = oes
        .client
        .get(oes.url(&format!("/api/v1/shares/{share_id}")))
        .bearer_auth(AUDITOR)
        .send()
        .await
        .expect("auditor detail");
    assert_eq!(audited.status(), StatusCode::OK);
    let audited_url = oes
        .client
        .get(oes.url(&format!("/api/v1/shares/{share_id}/url")))
        .bearer_auth(AUDITOR)
        .send()
        .await
        .expect("auditor url");
    assert_eq!(audited_url.status(), StatusCode::FORBIDDEN);

    // An administrator can copy the link again, and gets the same one back.
    let revealed: Value = oes
        .client
        .get(oes.url(&format!("/api/v1/shares/{share_id}/url")))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("reveal url")
        .json()
        .await
        .expect("url JSON");
    assert_eq!(revealed["available"], true);
    assert_eq!(revealed["url"], url);

    // A live share's record cannot be deleted; revoking it first is required.
    let premature = oes
        .client
        .delete(oes.url(&format!("/api/v1/shares/{share_id}")))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("premature delete");
    assert_eq!(premature.status(), StatusCode::CONFLICT);
    oes.client
        .post(oes.url(&format!("/api/v1/shares/{share_id}/revoke")))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("revoke");
    let purged = oes
        .client
        .delete(oes.url(&format!("/api/v1/shares/{share_id}")))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("delete record");
    assert_eq!(purged.status(), StatusCode::NO_CONTENT);

    oes.stop().await;
}

#[tokio::test]
async fn capability_activity_is_audited_and_capability_tokens_are_redacted() {
    let oes = Harness::start().await;
    oes.create_bucket("audited").await;
    oes.upload("audited", "note.txt", "text/plain", b"hello\n")
        .await;
    let issued = oes
        .create_share("audited", "note.txt", json!({ "label": "Reviewed" }))
        .await;
    let token = token_of(issued["url"].as_str().expect("url")).to_owned();
    let share_id = issued["share"]["id"].as_str().expect("id").to_owned();

    oes.client
        .post(oes.url(&format!("/api/v1/shares/{share_id}/revoke")))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("revoke");
    // A denied access against a real capability is a security event.
    oes.client
        .get(oes.url(&format!("/s/{token}/content")))
        .send()
        .await
        .expect("denied access");

    let audit = oes
        .client
        .get(oes.url("/api/v1/audit/events?limit=200"))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("audit events")
        .text()
        .await
        .expect("body");

    assert!(
        !audit.contains(&token),
        "the audit trail recorded a live capability token"
    );
    for operation in ["share.created", "share.revoked", "share.denied"] {
        assert!(
            audit.contains(operation),
            "the audit trail is missing {operation}"
        );
    }
    // The non-secret resource identifier is what an investigator follows.
    assert!(
        audit.contains(&share_id),
        "the audit trail does not name the share by its stable identifier"
    );

    oes.stop().await;
}

#[tokio::test]
async fn capability_metrics_are_counted_without_unbounded_labels() {
    let oes = Harness::start_with(|config| {
        config.auth.metrics_scrape_token = Some(SecretValue::new(
            "test-dedicated-metrics-scrape-token-32-bytes-long",
        ));
    })
    .await;
    oes.create_bucket("measured").await;
    oes.upload("measured", "note.txt", "text/plain", b"hello\n")
        .await;
    let issued = oes
        .create_share("measured", "note.txt", json!({ "label": "Counted" }))
        .await;
    let token = token_of(issued["url"].as_str().expect("url")).to_owned();
    oes.client
        .get(oes.url(&format!("/s/{token}/content")))
        .send()
        .await
        .expect("share access");
    oes.client
        .get(oes.url("/api/v1/buckets/measured/object-preview/note.txt"))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("preview");

    let exposition = oes
        .client
        .get(oes.url("/metrics"))
        .bearer_auth("test-dedicated-metrics-scrape-token-32-bytes-long")
        .send()
        .await
        .expect("metrics")
        .text()
        .await
        .expect("body");

    for metric in [
        "oes_preview_requests_total",
        "oes_share_access_total",
        "oes_share_links_active",
        "oes_embeds_active",
        "oes_embed_requests_total",
    ] {
        assert!(exposition.contains(metric), "missing metric {metric}");
    }
    assert!(exposition.contains("oes_share_access_total 1"));
    assert!(exposition.contains("oes_share_links_active 1"));
    // No metric may carry a token, a key, or any other unbounded dimension.
    assert!(
        !exposition.contains(&token),
        "a metric label carried a token"
    );
    assert!(
        !exposition.contains("note.txt"),
        "a metric label carried an object key"
    );
    assert!(
        !exposition.contains('{'),
        "capability metrics gained labels"
    );

    oes.stop().await;
}

#[tokio::test]
async fn deployment_policy_can_forbid_capabilities_outright() {
    let oes = Harness::start_with(|config| {
        config.sharing.shares_enabled = false;
        config.sharing.embeds_enabled = false;
    })
    .await;
    oes.create_bucket("restricted").await;
    oes.upload("restricted", "note.txt", "text/plain", b"hello\n")
        .await;

    let settings: Value = oes
        .client
        .get(oes.url("/api/v1/sharing/settings"))
        .bearer_auth(ADMIN)
        .send()
        .await
        .expect("settings")
        .json()
        .await
        .expect("settings JSON");
    assert_eq!(settings["shares_enabled"], false);
    assert_eq!(settings["embeds_enabled"], false);

    let refused = oes
        .client
        .post(oes.url("/api/v1/buckets/restricted/object-shares/note.txt"))
        .bearer_auth(ADMIN)
        .json(&json!({ "label": "Nope" }))
        .send()
        .await
        .expect("share attempt");
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    oes.stop().await;
}

#[tokio::test]
async fn capabilities_work_normally_over_objects_encrypted_at_rest() {
    // Encryption is a storage-layer concern, and the capability paths must not
    // know or care: they read through the same authoritative object service, so
    // decryption streams rather than producing a temporary plaintext file.
    let oes = Harness::start_with(|config| {
        config.storage.encryption_enabled = true;
    })
    .await;
    oes.create_bucket("encrypted").await;
    let body: Vec<u8> = (0..8192_u32).map(|index| (index % 251) as u8).collect();
    let mut payload = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom".to_vec();
    payload.extend_from_slice(&body);
    oes.upload("encrypted", "clip.mp4", "video/mp4", &payload)
        .await;

    let issued = oes
        .create_share(
            "encrypted",
            "clip.mp4",
            json!({ "label": "Encrypted screening" }),
        )
        .await;
    let token = token_of(issued["url"].as_str().expect("url")).to_owned();

    let full = oes
        .client
        .get(oes.url(&format!("/s/{token}/content")))
        .send()
        .await
        .expect("encrypted content");
    assert_eq!(full.status(), StatusCode::OK);
    let served = full.bytes().await.expect("body");
    assert_eq!(
        served.as_ref(),
        payload.as_slice(),
        "served bytes differ from the stored object"
    );

    // A range over encrypted storage still returns exactly the requested slice.
    let partial = oes
        .client
        .get(oes.url(&format!("/s/{token}/content")))
        .header("range", "bytes=4096-4195")
        .send()
        .await
        .expect("encrypted range");
    assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
    let slice = partial.bytes().await.expect("range body");
    assert_eq!(slice.as_ref(), &payload[4096..4196]);

    oes.stop().await;
}

#[tokio::test]
async fn share_and_embed_responses_never_carry_a_management_credential() {
    let oes = Harness::start().await;
    oes.create_bucket("credential-free").await;
    oes.upload("credential-free", "logo.gif", "image/gif", GIF)
        .await;

    let share = oes
        .create_share("credential-free", "logo.gif", json!({ "label": "Look" }))
        .await;
    let embed: Value = oes
        .create_embed("credential-free", "logo.gif", json!({ "label": "Site" }))
        .await
        .json()
        .await
        .expect("embed JSON");
    let share_token = token_of(share["url"].as_str().expect("url")).to_owned();
    let embed_token = token_of(embed["url"].as_str().expect("url")).to_owned();

    for path in [
        oes.url(&format!("/s/{share_token}")),
        oes.url(&format!("/s/{share_token}/content")),
        oes.embed_url(&embed_token),
    ] {
        let response = oes.client.get(&path).send().await.expect("public request");
        // Every header value, plus the body, is checked: a public response must
        // not contain a management token, the root credential, or anything else
        // that would work anywhere but on this one object.
        let rendered = format!("{:?}", response.headers());
        for secret in [
            ADMIN,
            AUDITOR,
            "test-access",
            "test-secret-at-least-sixteen",
        ] {
            assert!(
                !rendered.contains(secret),
                "{path} leaked a credential in its headers"
            );
        }
        assert!(
            !rendered.to_ascii_lowercase().contains("authorization"),
            "{path} echoed an authorization header"
        );
        let body = response.text().await.expect("body");
        for secret in [ADMIN, AUDITOR, "test-secret-at-least-sixteen"] {
            assert!(
                !body.contains(secret),
                "{path} leaked a credential in its body"
            );
        }
    }

    oes.stop().await;
}

#[tokio::test]
async fn embeds_are_served_by_storage_and_are_absent_from_the_management_plane() {
    // An embed URL is pasted into somebody else's page. It has to resolve on the
    // endpoint a deployment publishes for object bytes, so that a site loading an
    // asset never needs to reach the management plane — which most deployments
    // keep closed to the internet entirely.
    let oes = Harness::start().await;
    oes.create_bucket("published").await;
    oes.upload("published", "logo.gif", "image/gif", GIF).await;

    let issued: Value = oes
        .create_embed("published", "logo.gif", json!({ "label": "Website" }))
        .await
        .json()
        .await
        .expect("embed JSON");
    let url = issued["url"].as_str().expect("url").to_owned();
    let token = token_of(&url).to_owned();

    // The published URL names the storage listener, not the management one.
    assert!(
        url.starts_with(&format!("http://{}", oes.s3_address)),
        "an embed URL must be published on the storage endpoint: {url}"
    );
    assert!(
        !url.contains(&oes.address.to_string()),
        "an embed URL must not point at the management plane: {url}"
    );

    let served = oes
        .client
        .get(oes.embed_url(&token))
        .send()
        .await
        .expect("embed from storage");
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(served.bytes().await.expect("bytes").as_ref(), GIF);

    // The management listener does not serve embeds at all.
    let management = oes
        .client
        .get(oes.url(&format!("/e/{token}")))
        .send()
        .await
        .expect("embed from management");
    assert_eq!(management.status(), StatusCode::NOT_FOUND);

    oes.stop().await;
}

#[tokio::test]
async fn embed_delivery_needs_no_s3_credential_and_reaches_nothing_else() {
    let oes = Harness::start().await;
    oes.create_bucket("guarded").await;
    oes.upload("guarded", "logo.gif", "image/gif", GIF).await;
    let issued: Value = oes
        .create_embed("guarded", "logo.gif", json!({ "label": "Website" }))
        .await
        .json()
        .await
        .expect("embed JSON");
    let token = token_of(issued["url"].as_str().expect("url")).to_owned();

    // The embed route sits alongside the S3 operations rather than inside them,
    // so it is reachable without a signature.
    let anonymous = oes
        .client
        .get(oes.embed_url(&token))
        .send()
        .await
        .expect("unsigned embed");
    assert_eq!(anonymous.status(), StatusCode::OK);

    // Every other S3 operation still demands one. Sharing a listener must not
    // mean sharing an authorization decision.
    for path in ["/", "/guarded", "/guarded/logo.gif"] {
        let refused = oes
            .client
            .get(format!("http://{}{path}", oes.s3_address))
            .send()
            .await
            .expect("unsigned S3 request");
        assert!(
            refused.status() == StatusCode::FORBIDDEN
                || refused.status() == StatusCode::UNAUTHORIZED,
            "{path} was served without a signature: {}",
            refused.status()
        );
    }

    // The token names one object and cannot be steered anywhere else.
    let traversal = oes
        .client
        .get(format!(
            "http://{}/e/{token}/../../guarded/logo.gif",
            oes.s3_address
        ))
        .send()
        .await
        .expect("traversal attempt");
    assert_ne!(traversal.status(), StatusCode::OK);

    oes.stop().await;
}
