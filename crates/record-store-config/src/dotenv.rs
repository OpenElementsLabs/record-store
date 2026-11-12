//! Optional `.env` file read from the working directory for local development.

use std::{
    env,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

use crate::{ConfigError, error::parse_environment};

/// File name looked up in the working directory.
const FILE_NAME: &str = ".env";

/// Process variable that turns `.env` loading off when set to `false`. It is
/// read from the process environment only: a file cannot switch itself off.
const SWITCH: &str = "RECORD_STORE_DOTENV";

/// Names the TOML file when `--config` is not given.
const CONFIG_FILE: &str = "RECORD_STORE_CONFIG_FILE";

/// Returns the process environment layered over `.env` in the working
/// directory, so an exported variable always wins over the file.
///
/// The pairs are returned rather than written into the process environment, so
/// they reach configuration only and are never inherited by child processes.
pub fn resolved_environment() -> Result<Vec<(OsString, OsString)>, ConfigError> {
    layered(Path::new(FILE_NAME), env::vars_os().collect())
}

fn layered(
    path: &Path,
    process: Vec<(OsString, OsString)>,
) -> Result<Vec<(OsString, OsString)>, ConfigError> {
    let enabled = match process.iter().find(|(key, _)| key == SWITCH) {
        Some((_, value)) => {
            let value = value
                .to_str()
                .ok_or(ConfigError::NonUnicodeEnvironment(SWITCH))?;
            parse_environment::<bool>(SWITCH, value)?
        }
        None => true,
    };
    let mut environment = if enabled { read(path)? } else { Vec::new() };
    environment.extend(process);
    Ok(environment)
}

/// Returns the configuration file named by `RECORD_STORE_CONFIG_FILE`, so the
/// variable works from `.env` too and not only when exported.
pub(crate) fn config_file(environment: &[(OsString, OsString)]) -> Option<PathBuf> {
    environment
        .iter()
        .rev()
        .find(|(key, _)| key == CONFIG_FILE)
        .map(|(_, value)| PathBuf::from(value))
        .filter(|path| !path.as_os_str().is_empty())
}

/// Reads `KEY=value` pairs from a dotenv file. A missing file yields no pairs.
///
/// The pairs are returned rather than written into the process environment, so
/// they reach configuration only and are never inherited by child processes.
pub(crate) fn read(path: &Path) -> Result<Vec<(OsString, OsString)>, ConfigError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(env_file_error(path, error.to_string())),
    };
    refuse_world_writable(path, &metadata)?;
    dotenvy::from_path_iter(path)
        .map_err(|error| env_file_error(path, describe(path, &error)))?
        .map(|entry| {
            entry
                .map(|(key, value)| (key.into(), value.into()))
                .map_err(|error| env_file_error(path, describe(path, &error)))
        })
        .collect()
}

/// Anyone able to write the file could set root credentials or turn off
/// encryption, so a file every local user can change is not trusted.
#[cfg(unix)]
fn refuse_world_writable(path: &Path, metadata: &fs::Metadata) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt;

    if metadata.permissions().mode() & 0o002 != 0 {
        return Err(env_file_error(
            path,
            "the file is writable by every user; remove that permission with chmod o-w".into(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn refuse_world_writable(_path: &Path, _metadata: &fs::Metadata) -> Result<(), ConfigError> {
    Ok(())
}

/// A parse failure carries the offending line, which may hold a secret, so it
/// is described by its line number instead.
fn describe(path: &Path, error: &dotenvy::Error) -> String {
    const OMITTED: &str = "its contents are omitted because they may hold secrets";
    match error {
        dotenvy::Error::LineParse(line, _) => match line_number(path, line) {
            Some(number) => format!("line {number} could not be parsed; {OMITTED}"),
            None => format!("a line could not be parsed; {OMITTED}"),
        },
        other => other.to_string(),
    }
}

/// dotenvy reports the offending text but not where it is, so it is located
/// in the file again.
fn line_number(path: &Path, line: &str) -> Option<usize> {
    let contents = fs::read_to_string(path).ok()?;
    let offset = contents.find(line.trim_end())?;
    Some(contents[..offset].matches('\n').count() + 1)
}

fn env_file_error(path: &Path, reason: String) -> ConfigError {
    ConfigError::EnvFile {
        path: path.to_path_buf(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, test_support::credentials};

    fn write(directory: &tempfile::TempDir, contents: &str) -> std::path::PathBuf {
        let path = directory.path().join(FILE_NAME);
        fs::write(&path, contents).expect("write .env");
        path
    }

    #[test]
    fn missing_file_yields_nothing() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let pairs = read(&directory.path().join(FILE_NAME)).expect("missing file is fine");
        assert!(pairs.is_empty());
    }

    #[test]
    fn file_supplies_configuration() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = write(
            &directory,
            "# local credentials\n\
             RECORD_STORE_ROOT_ACCESS_KEY=dotenv-access\n\
             RECORD_STORE_ROOT_SECRET_KEY=\"dotenv-secret-at-least-sixteen\"\n",
        );
        let config = Config::load_with_environment(None, read(&path).expect("read"))
            .expect("configuration from .env");
        let (access, secret) = config.root_credentials().expect("credentials");
        assert_eq!(access, "dotenv-access");
        assert_eq!(secret.expose(), "dotenv-secret-at-least-sixteen");
    }

    #[test]
    fn exported_variables_override_the_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = write(
            &directory,
            "RECORD_STORE_ROOT_ACCESS_KEY=dotenv-access\n\
             RECORD_STORE_ROOT_SECRET_KEY=dotenv-secret-at-least-sixteen\n",
        );
        let process = credentials()
            .map(|(key, value)| (key.into(), value.into()))
            .to_vec();
        let environment = layered(&path, process).expect("read");
        let config = Config::load_with_environment(None, environment).expect("configuration");
        let (access, secret) = config.root_credentials().expect("credentials");
        assert_eq!(access, "test-access");
        assert_eq!(secret.expose(), "test-secret-at-least-sixteen");
    }

    #[test]
    fn parse_errors_do_not_reveal_values() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = write(
            &directory,
            "RECORD_STORE_ROOT_SECRET_KEY='unterminated-secret-value\n",
        );
        let message = read(&path).expect_err("malformed file").to_string();
        assert!(!message.contains("unterminated-secret-value"), "{message}");
        assert!(message.contains(".env"), "{message}");
    }

    #[test]
    fn parse_errors_name_the_line() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = write(
            &directory,
            "RECORD_STORE_ROOT_ACCESS_KEY=fine\n\
             \n\
             NOT A VALID LINE secret-value\n",
        );
        let message = read(&path).expect_err("malformed file").to_string();
        assert!(message.contains("line 3 could not be parsed"), "{message}");
        assert!(!message.contains("secret-value"), "{message}");
    }

    #[test]
    fn the_switch_turns_the_file_off() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = write(&directory, "RECORD_STORE_ROOT_ACCESS_KEY=from-file\n");
        let off = layered(&path, vec![(SWITCH.into(), "false".into())]).expect("switched off");
        assert!(
            !off.iter()
                .any(|(key, _)| key == "RECORD_STORE_ROOT_ACCESS_KEY")
        );
        let on = layered(&path, vec![(SWITCH.into(), "true".into())]).expect("switched on");
        assert!(
            on.iter()
                .any(|(key, _)| key == "RECORD_STORE_ROOT_ACCESS_KEY")
        );
        let error = layered(&path, vec![(SWITCH.into(), "no".into())]).expect_err("bad switch");
        assert!(error.to_string().contains(SWITCH), "{error}");
    }

    #[test]
    fn the_file_can_name_the_configuration_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = write(&directory, "RECORD_STORE_CONFIG_FILE=from-file.toml\n");
        let environment = layered(&path, Vec::new()).expect("read");
        assert_eq!(config_file(&environment), Some("from-file.toml".into()));

        let exported =
            layered(&path, vec![(CONFIG_FILE.into(), "exported.toml".into())]).expect("read");
        assert_eq!(config_file(&exported), Some("exported.toml".into()));
    }

    #[cfg(unix)]
    #[test]
    fn world_writable_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = write(&directory, "RECORD_STORE_ROOT_ACCESS_KEY=planted\n");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).expect("chmod");
        let message = read(&path).expect_err("world-writable file").to_string();
        assert!(message.contains("writable by every user"), "{message}");
    }
}
