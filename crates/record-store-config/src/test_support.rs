//! Shared fixtures for configuration tests.

use crate::Config;

pub(crate) fn credentials() -> [(&'static str, &'static str); 2] {
    [
        ("RECORD_STORE_ROOT_ACCESS_KEY", "test-access"),
        (
            "RECORD_STORE_ROOT_SECRET_KEY",
            "test-secret-at-least-sixteen",
        ),
    ]
}

pub(crate) fn valid_config() -> Config {
    Config::load_with_environment(None, credentials()).expect("defaults must be valid")
}
