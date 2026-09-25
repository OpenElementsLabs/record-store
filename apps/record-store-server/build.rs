//! Embeds the commit a binary was built from, as `RECORD_STORE_BUILD_COMMIT`.
//!
//! The version names a release; the commit names the build. Unreleased builds
//! carry the previous release's version, so without the commit a deployment, a
//! backup and a bug report cannot say which code they came from.
//!
//! `RECORD_STORE_BUILD_COMMIT` in the build environment wins: the container and
//! release builds pass it, because their build context has no `.git`. A build
//! from a checkout reads `HEAD`. Anything else reports `unknown` rather than a
//! guess.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=RECORD_STORE_BUILD_COMMIT");
    let commit = std::env::var("RECORD_STORE_BUILD_COMMIT")
        .ok()
        .filter(|value| is_commit(value))
        .or_else(checkout_head)
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=RECORD_STORE_BUILD_COMMIT={commit}");
}

fn checkout_head() -> Option<String> {
    // Rebuild when HEAD moves: HEAD itself, the branch it names, and the packed
    // refs a branch may live in. Only files that exist are named, because a
    // missing one would make every build rerun this script.
    for name in ["HEAD", "packed-refs"] {
        watch(&git(&["rev-parse", "--git-path", name])?);
    }
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch(&git(&["rev-parse", "--git-path", &branch])?);
    }
    git(&["rev-parse", "HEAD"]).filter(|value| is_commit(value))
}

fn watch(path: &str) {
    if Path::new(path).exists() {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn git(arguments: &[&str]) -> Option<String> {
    let output = Command::new("git").args(arguments).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
        .map(|text| text.trim().to_owned())
}

fn is_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
