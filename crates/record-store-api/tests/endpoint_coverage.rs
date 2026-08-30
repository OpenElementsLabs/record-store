//! Guards against management endpoints losing their consumer.
//!
//! Every route the management router registers must be deliberately classified
//! here. Adding a route without classifying it fails this test, which is the
//! point: an endpoint nobody calls and nobody tests is how an API grows surface
//! that silently rots.
//!
//! The classification is also checked against the console's own API layer, so a
//! route declared as console-facing cannot quietly stop being called. That check
//! reads the console's source text rather than importing anything from it, so
//! nothing here couples the Rust build to the frontend bundle.

/// Who a management route exists for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Audience {
    /// Reached by the web console, and expected in its API layer.
    Console,
    /// Reached by the operational CLI only.
    Cli,
    /// Reached by both.
    ConsoleAndCli,
    /// Unauthenticated operational probe or scrape target, consumed by
    /// orchestrators and Prometheus rather than by Record Store's own clients.
    Probe,
    /// Unauthenticated public capability delivery, where the opaque token in
    /// the path is the entire authorization. Reached by share recipients and by
    /// other people's websites, never by the console's authenticated API layer.
    ///
    /// `/s/...` is mounted on the management listener and proxied by the
    /// console, because a share is a page a person opens. `/e/...` is mounted on
    /// the storage listener instead, because an embed serves object bytes into
    /// somebody else's page and must not require reaching the management plane.
    PublicCapability,
}

impl Audience {
    const fn expects_console(self) -> bool {
        matches!(self, Self::Console | Self::ConsoleAndCli)
    }
}

/// The declared audience of every management route.
const CLASSIFIED: &[(&str, Audience)] = &[
    ("/api/v1/audit/events", Audience::ConsoleAndCli),
    ("/api/v1/auth/session", Audience::Console),
    ("/api/v1/buckets", Audience::Console),
    ("/api/v1/buckets/{}", Audience::Console),
    ("/api/v1/buckets/{}/lifecycle", Audience::Console),
    ("/api/v1/buckets/{}/lifecycle/{}", Audience::Console),
    ("/api/v1/buckets/{}/object-copy/{}", Audience::Console),
    ("/api/v1/buckets/{}/object-embeds/{}", Audience::Console),
    ("/api/v1/buckets/{}/object-shares/{}", Audience::Console),
    ("/api/v1/buckets/{}/object-content/{}", Audience::Console),
    ("/api/v1/buckets/{}/object-preview/{}", Audience::Console),
    ("/api/v1/buckets/{}/object-versions", Audience::Console),
    ("/api/v1/buckets/{}/object-versions/{}", Audience::Console),
    ("/api/v1/buckets/{}/object/{}", Audience::Console),
    ("/api/v1/buckets/{}/objects", Audience::Console),
    ("/api/v1/buckets/{}/quota", Audience::Console),
    ("/api/v1/buckets/{}/versioning", Audience::ConsoleAndCli),
    ("/api/v1/cluster", Audience::ConsoleAndCli),
    // The console's Drives screen lists devices and drives their lifecycle; the
    // CLI covers the same ground for automation.
    ("/api/v1/devices", Audience::ConsoleAndCli),
    ("/api/v1/placement/explain/{}/{}", Audience::Cli),
    ("/api/v1/storage-classes", Audience::Cli),
    ("/api/v1/storage-classes/{}", Audience::Cli),
    ("/api/v1/nodes/{}/devices", Audience::Cli),
    ("/api/v1/nodes/{}/devices/{}", Audience::Cli),
    (
        "/api/v1/nodes/{}/devices/{}/activate",
        Audience::ConsoleAndCli,
    ),
    ("/api/v1/nodes/{}/devices/{}/drain", Audience::ConsoleAndCli),
    (
        "/api/v1/nodes/{}/devices/{}/maintenance",
        Audience::ConsoleAndCli,
    ),
    (
        "/api/v1/nodes/{}/devices/{}/release",
        Audience::ConsoleAndCli,
    ),
    (
        "/api/v1/nodes/{}/devices/{}/resume",
        Audience::ConsoleAndCli,
    ),
    (
        "/api/v1/nodes/{}/devices/{}/retire",
        Audience::ConsoleAndCli,
    ),
    ("/api/v1/cluster/health", Audience::Console),
    // Cluster bootstrap stays out of the browser on purpose: initializing a
    // cluster and minting node join credentials are operator actions performed
    // from a shell on the host, not from a signed-in web session.
    ("/api/v1/cluster/init", Audience::Cli),
    ("/api/v1/cluster/join-tokens", Audience::Cli),
    ("/api/v1/events", Audience::Console),
    ("/api/v1/lifecycle-rules/{}", Audience::Console),
    ("/api/v1/nodes", Audience::ConsoleAndCli),
    ("/api/v1/nodes/{}", Audience::ConsoleAndCli),
    ("/api/v1/nodes/{}/decommission", Audience::ConsoleAndCli),
    ("/api/v1/nodes/{}/drain", Audience::ConsoleAndCli),
    ("/api/v1/nodes/{}/maintenance", Audience::ConsoleAndCli),
    ("/api/v1/nodes/{}/resume", Audience::ConsoleAndCli),
    ("/api/v1/policies", Audience::ConsoleAndCli),
    ("/api/v1/policies/{}/bindings/{}", Audience::ConsoleAndCli),
    ("/api/v1/rebalance", Audience::ConsoleAndCli),
    ("/api/v1/rebalance/status", Audience::ConsoleAndCli),
    ("/api/v1/repair/status", Audience::ConsoleAndCli),
    ("/api/v1/restore/{}/{}", Audience::Console),
    ("/api/v1/embeds/{}", Audience::Console),
    ("/api/v1/embeds/{}/revoke", Audience::Console),
    ("/api/v1/embeds/{}/url", Audience::Console),
    ("/api/v1/service-accounts", Audience::Console),
    ("/api/v1/service-accounts/{}", Audience::ConsoleAndCli),
    (
        "/api/v1/service-accounts/{}/credentials",
        Audience::ConsoleAndCli,
    ),
    (
        "/api/v1/service-accounts/{}/credentials/{}/status",
        Audience::ConsoleAndCli,
    ),
    (
        "/api/v1/service-accounts/{}/status",
        Audience::ConsoleAndCli,
    ),
    (
        "/api/v1/service-accounts/{}/temporary-credentials",
        Audience::Cli,
    ),
    ("/api/v1/shares/{}", Audience::Console),
    ("/api/v1/shares/{}/revoke", Audience::Console),
    ("/api/v1/shares/{}/url", Audience::Console),
    ("/api/v1/sharing/settings", Audience::Console),
    ("/api/v1/storage/inspect", Audience::ConsoleAndCli),
    ("/api/v1/storage/repair", Audience::ConsoleAndCli),
    ("/api/v1/storage/status", Audience::Console),
    ("/api/v1/storage/usage", Audience::Console),
    ("/api/v1/system/info", Audience::Console),
    ("/api/v1/system/metrics", Audience::Console),
    ("/api/v1/verify/buckets/{}", Audience::ConsoleAndCli),
    ("/api/v1/verify/objects/{}/{}", Audience::ConsoleAndCli),
    ("/api/v1/webhook-deliveries", Audience::ConsoleAndCli),
    ("/api/v1/webhooks", Audience::ConsoleAndCli),
    ("/api/v1/webhooks/{}", Audience::Console),
    ("/api/v1/webhooks/{}/status", Audience::Console),
    ("/e/{}", Audience::PublicCapability),
    ("/health", Audience::Probe),
    ("/metrics", Audience::Probe),
    ("/ready", Audience::Probe),
    ("/s/{}", Audience::PublicCapability),
    ("/s/{}/content", Audience::PublicCapability),
    ("/s/{}/unlock", Audience::PublicCapability),
];

/// The management router's own source, read at compile time.
const API_SOURCE: &str = include_str!("../src/lib.rs");

/// Reads the console's typed API layer.
///
/// The directory is read at test time rather than enumerated with `include_str!`
/// so a newly added API module is picked up automatically instead of silently
/// escaping the coverage check. The console ships in this repository, so its
/// absence is a real failure rather than a reason to skip.
fn console_source() -> String {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../console/lib/api")
        .canonicalize()
        .expect("the console API layer must be present in the repository");
    let mut combined = String::new();
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&directory)
        .expect("read the console API directory")
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "ts"))
        .filter(|path| !path.to_string_lossy().contains(".test."))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no console API modules found in {}",
        directory.display()
    );
    for path in files {
        combined.push_str(&std::fs::read_to_string(&path).expect("read a console API module"));
        combined.push('\n');
    }
    combined
}

/// Replaces every `{name}`, `{*name}`, and `${expr}` with a bare `{}`.
///
/// Route parameters are named differently on each side of the wire; comparing
/// their shapes is what matters.
fn normalize(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        // A `${` in TypeScript opens an interpolation; drop the stray `$`.
        let head = rest[..start].strip_suffix('$').unwrap_or(&rest[..start]);
        out.push_str(head);
        out.push_str("{}");
        match rest[start..].find('}') {
            Some(end) => rest = &rest[start + end + 1..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Extracts every route literal the management router registers.
fn registered_routes() -> Vec<String> {
    let mut routes = Vec::new();
    for fragment in API_SOURCE.split(".route(").skip(1) {
        let Some(open) = fragment.find('"') else {
            continue;
        };
        let Some(len) = fragment[open + 1..].find('"') else {
            continue;
        };
        let path = &fragment[open + 1..open + 1 + len];
        if !path.starts_with('/') {
            continue;
        }
        let path = normalize(path);
        if !routes.contains(&path) {
            routes.push(path);
        }
    }
    routes.sort();
    routes
}

/// Extracts every management path the console's API layer builds.
///
/// Each occurrence is located by its opening delimiter and read to the matching
/// one. Splitting on quotes and taking alternate fragments would be simpler but
/// wrong: an apostrophe in a comment shifts the parity for the rest of the file.
fn console_paths() -> Vec<String> {
    const NEEDLE: &str = "/v1/";
    let source = console_source();
    let source = source.as_str();
    let bytes = source.as_bytes();
    let mut paths = Vec::new();
    let mut search = 0;
    while let Some(found) = source[search..].find(NEEDLE) {
        let start = search + found;
        search = start + NEEDLE.len();
        if start == 0 {
            continue;
        }
        let delimiter = bytes[start - 1];
        if !matches!(delimiter, b'`' | b'\'' | b'"') {
            continue;
        }
        let Some(end) = source[start..].find(delimiter as char) else {
            continue;
        };
        let literal = &source[start..start + end];
        let path = format!("/api{}", normalize(literal.trim_end_matches('/')));
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

#[test]
fn every_registered_route_is_classified() {
    let registered = registered_routes();
    let classified: Vec<&str> = CLASSIFIED.iter().map(|(path, _)| *path).collect();

    let unclassified: Vec<&String> = registered
        .iter()
        .filter(|path| !classified.contains(&path.as_str()))
        .collect();
    assert!(
        unclassified.is_empty(),
        "these management routes have no declared audience. Add each one to \
         CLASSIFIED as Console, Cli, ConsoleAndCli, or Probe, or remove the \
         route: {unclassified:?}"
    );
}

#[test]
fn no_classification_outlives_its_route() {
    let registered = registered_routes();
    let stale: Vec<&str> = CLASSIFIED
        .iter()
        .map(|(path, _)| *path)
        .filter(|path| !registered.contains(&(*path).to_owned()))
        .collect();
    assert!(
        stale.is_empty(),
        "these routes are classified but no longer registered; drop them from \
         CLASSIFIED: {stale:?}"
    );
}

#[test]
fn every_console_route_is_reachable_from_the_console() {
    let available = console_paths();
    let missing: Vec<&str> = CLASSIFIED
        .iter()
        .filter(|(_, audience)| audience.expects_console())
        .map(|(path, _)| *path)
        .filter(|path| !available.contains(&(*path).to_owned()))
        .collect();
    assert!(
        missing.is_empty(),
        "these routes are classified as console-facing but the console's API \
         layer does not build them. Either implement the screen or reclassify \
         the route: {missing:?}"
    );
}

#[test]
fn the_console_only_calls_routes_that_exist() {
    let registered = registered_routes();
    let unknown: Vec<&String> = console_paths()
        .iter()
        .filter(|path| !registered.contains(path))
        .cloned()
        .collect::<Vec<_>>()
        .leak()
        .iter()
        .collect();
    assert!(
        unknown.is_empty(),
        "the console builds these paths but the management router does not serve \
         them: {unknown:?}"
    );
}

#[test]
fn the_unauthenticated_surface_is_exactly_probes_and_capability_delivery() {
    let probes: Vec<&str> = CLASSIFIED
        .iter()
        .filter(|(_, audience)| *audience == Audience::Probe)
        .map(|(path, _)| *path)
        .collect();
    // Anything else being anonymous would be an information disclosure; the
    // authenticated surface is asserted end to end in the server tests.
    assert_eq!(probes, vec!["/health", "/metrics", "/ready"]);

    // The public capability routes are the only other anonymous surface, and
    // this list is what stops that surface growing by accident. Every entry
    // takes an opaque token as its first segment and can reach nothing but the
    // single object that token names.
    let public: Vec<&str> = CLASSIFIED
        .iter()
        .filter(|(_, audience)| *audience == Audience::PublicCapability)
        .map(|(path, _)| *path)
        .collect();
    assert_eq!(
        public,
        vec!["/e/{}", "/s/{}", "/s/{}/content", "/s/{}/unlock"]
    );
    for path in public {
        assert!(
            path.starts_with("/s/{}") || path.starts_with("/e/{}"),
            "{path} is public but is not addressed by a capability token"
        );
        assert!(
            !path.starts_with("/api/"),
            "{path} puts public access inside the administrative tree"
        );
    }
}
