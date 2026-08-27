use crate::{COLLECTION_INTERVAL, DEFAULT_RETENTION_DAYS, SECONDS_PER_DAY, WEB_LISTEN_ADDRESS};
use serde::Deserialize;
use std::fs;
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_collection_interval_seconds")]
    pub collection_interval_seconds: u64,
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,
    #[serde(default = "default_listen_address")]
    pub listen_address: String,
    #[serde(default = "default_mcp_token")]
    pub mcp_token: String,
    #[serde(default)]
    pub metrics: MetricsConfig,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    #[serde(default = "enabled_by_default")]
    pub cpu_percent: bool,
    #[serde(default = "enabled_by_default")]
    pub ram_used_bytes: bool,
    #[serde(default = "enabled_by_default")]
    pub disk_used_bytes: bool,
    #[serde(default = "enabled_by_default")]
    pub uptime_seconds: bool,
    #[serde(default = "enabled_by_default")]
    pub load_average_1m: bool,
    #[serde(default = "enabled_by_default")]
    pub load_average_5m: bool,
    #[serde(default = "enabled_by_default")]
    pub load_average_15m: bool,
    #[serde(default = "enabled_by_default")]
    pub swap_used_bytes: bool,
    #[serde(default = "enabled_by_default")]
    pub network_rx_bytes: bool,
    #[serde(default = "enabled_by_default")]
    pub network_tx_bytes: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            collection_interval_seconds: default_collection_interval_seconds(),
            retention_days: default_retention_days(),
            listen_address: default_listen_address(),
            mcp_token: default_mcp_token(),
            metrics: MetricsConfig::default(),
        }
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            cpu_percent: true,
            ram_used_bytes: true,
            disk_used_bytes: true,
            uptime_seconds: true,
            load_average_1m: true,
            load_average_5m: true,
            load_average_15m: true,
            swap_used_bytes: true,
            network_rx_bytes: true,
            network_tx_bytes: true,
        }
    }
}

impl Config {
    fn from_toml(contents: &str) -> Result<Self, String> {
        let config: Config = toml::from_str(contents).map_err(|error| error.to_string())?;

        if config.collection_interval_seconds == 0 {
            return Err("collection_interval_seconds must be greater than zero".to_owned());
        }

        if config.retention_days == 0 {
            return Err("retention_days must be greater than zero".to_owned());
        }

        if config.retention_days.checked_mul(SECONDS_PER_DAY).is_none() {
            return Err("retention_days is too large".to_owned());
        }

        if config.listen_address.is_empty() {
            return Err("listen_address must not be empty".to_owned());
        }

        Ok(config)
    }

    pub fn collection_interval(&self) -> Duration {
        Duration::from_secs(self.collection_interval_seconds)
    }

    pub fn retention(&self) -> Duration {
        Duration::from_secs(self.retention_days * SECONDS_PER_DAY)
    }

    pub fn listen_address(&self) -> &str {
        &self.listen_address
    }
}

pub fn default_collection_interval_seconds() -> u64 {
    COLLECTION_INTERVAL.as_secs()
}

pub fn default_retention_days() -> u64 {
    DEFAULT_RETENTION_DAYS
}

pub fn default_mcp_token() -> String {
    "dev-mcp-token-replace-me".to_owned()
}

pub fn default_listen_address() -> String {
    WEB_LISTEN_ADDRESS.to_owned()
}

pub fn enabled_by_default() -> bool {
    true
}

pub fn resolve_config_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();

    // ./terakzor.toml (current working directory)
    candidates.push(std::path::PathBuf::from("terakzor.toml"));

    // ~/.config/terakzor/terakzor.toml  (or OS equivalent)
    if let Some(config_dir) = dirs::config_dir() {
        candidates.push(config_dir.join("terakzor").join("terakzor.toml"));
    }

    // /etc/terakzor/terakzor.toml (non-Windows only)
    #[cfg(not(target_os = "windows"))]
    candidates.push(std::path::PathBuf::from("/etc/terakzor/terakzor.toml"));

    candidates
}

pub fn config_env_value(value: Option<std::ffi::OsString>) -> stoolap::Result<Option<String>> {
    match value {
        None => Ok(None),
        Some(value) => value.into_string().map(Some).map_err(|_| {
            stoolap::Error::internal("TERAKZOR_CONFIG must contain a valid UTF-8 path")
        }),
    }
}

pub fn command_line_args(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> stoolap::Result<Vec<String>> {
    args.into_iter()
        .map(|arg| {
            arg.into_string().map_err(|_| {
                stoolap::Error::internal("command-line argument must contain valid UTF-8")
            })
        })
        .collect()
}

pub fn find_config_path(
    cli_arg: Option<&str>,
    env_var: Option<&str>,
    candidates: &[std::path::PathBuf],
) -> Result<Option<std::path::PathBuf>, String> {
    // 1. --config flag (explicit; missing = fatal)
    if let Some(raw) = cli_arg {
        let path = std::path::PathBuf::from(raw);
        if path.is_file() {
            return Ok(Some(path));
        }
        return Err(format!(
            "--config path does not exist or is not a file: {}",
            path.display()
        ));
    }

    // 2. $TERAKZOR_CONFIG env var (explicit; missing = fatal)
    if let Some(raw) = env_var {
        let path = std::path::PathBuf::from(raw);
        if path.is_file() {
            return Ok(Some(path));
        }
        return Err(format!(
            "TERAKZOR_CONFIG path does not exist or is not a file: {}",
            path.display()
        ));
    }

    // 3-5. Candidate paths (implicit; missing = silent skip)
    for candidate in candidates {
        if candidate.is_file() {
            return Ok(Some(candidate.clone()));
        }
    }

    Ok(None)
}

pub fn parse_config_arg(args: &[String]) -> Result<Option<&str>, String> {
    match args.iter().position(|arg| arg == "--config") {
        Some(index) => args
            .get(index + 1)
            .map(String::as_str)
            .map(Some)
            .ok_or_else(|| "--config requires a path".to_owned()),
        None => Ok(None),
    }
}

pub fn load_config(path: &Path) -> stoolap::Result<Config> {
    match fs::read_to_string(path) {
        Ok(contents) => Config::from_toml(&contents).map_err(|error| {
            stoolap::Error::internal(format!("invalid config at {}: {error}", path.display()))
        }),
        Err(error) => Err(stoolap::Error::internal(format!(
            "could not read config at {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod config_path_tests {

    use crate::config::*;

    use std::{ffi::OsString, path::PathBuf};

    // helper: create a real file in a tempdir so `exists()` returns true
    fn make_file(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, "").unwrap();
        path
    }

    #[test]
    fn cli_arg_takes_priority_when_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = make_file(&dir, "my.toml");
        let result = find_config_path(Some(cfg.to_str().unwrap()), None, &[]).unwrap();
        assert_eq!(result, Some(cfg));
    }

    #[test]
    fn cli_arg_takes_priority_over_env_var() {
        let dir = tempfile::tempdir().unwrap();
        let cli = make_file(&dir, "cli.toml");
        let env = make_file(&dir, "env.toml");

        let result = find_config_path(
            Some(cli.to_str().unwrap()),
            Some(env.to_str().unwrap()),
            &[],
        )
        .unwrap();

        assert_eq!(result, Some(cli));
    }

    #[test]
    fn cli_arg_takes_priority_over_candidate_file() {
        let dir = tempfile::tempdir().unwrap();
        let cli = make_file(&dir, "cli.toml");
        let candidate = make_file(&dir, "candidate.toml");

        let result = find_config_path(Some(cli.to_str().unwrap()), None, &[candidate]).unwrap();

        assert_eq!(result, Some(cli));
    }

    #[test]
    fn config_arg_returns_the_supplied_path() {
        let args = vec![
            "terakzor".to_owned(),
            "--config".to_owned(),
            "custom.toml".to_owned(),
        ];

        assert_eq!(parse_config_arg(&args).unwrap(), Some("custom.toml"));
    }

    #[test]
    fn config_arg_errors_when_path_is_missing() {
        let args = vec!["terakzor".to_owned(), "--config".to_owned()];
        let error = parse_config_arg(&args).unwrap_err();

        assert!(error.contains("--config requires a path"), "{error}");
    }

    #[test]
    fn cli_arg_errors_when_file_missing() {
        let err = find_config_path(Some("/no/such/file.toml"), None, &[]).unwrap_err();
        assert!(err.contains("/no/such/file.toml"), "got: {err}");
    }

    #[test]
    fn cli_arg_errors_when_path_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();

        let err = find_config_path(Some(path), None, &[]).unwrap_err();

        assert!(err.contains(path), "got: {err}");
    }

    #[test]
    fn env_var_used_when_no_cli_arg_and_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = make_file(&dir, "env.toml");
        let result = find_config_path(None, Some(cfg.to_str().unwrap()), &[]).unwrap();
        assert_eq!(result, Some(cfg));
    }

    #[test]
    fn env_var_takes_priority_over_candidate_file() {
        let dir = tempfile::tempdir().unwrap();
        let env = make_file(&dir, "env.toml");
        let candidate = make_file(&dir, "candidate.toml");

        let result = find_config_path(None, Some(env.to_str().unwrap()), &[candidate]).unwrap();

        assert_eq!(result, Some(env));
    }

    #[test]
    fn env_var_errors_when_file_missing() {
        let err = find_config_path(None, Some("/ghost.toml"), &[]).unwrap_err();
        assert!(err.contains("/ghost.toml"), "got: {err}");
    }

    #[test]
    fn env_var_errors_when_path_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();

        let err = find_config_path(None, Some(path), &[]).unwrap_err();

        assert!(err.contains(path), "got: {err}");
    }

    #[test]
    fn first_existing_candidate_wins() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.toml"); // does NOT exist
        let b = make_file(&dir, "b.toml"); // exists
        let c = make_file(&dir, "c.toml"); // exists but lower priority
        let result = find_config_path(None, None, &[a, b.clone(), c]).unwrap();
        assert_eq!(result, Some(b));
    }

    #[test]
    fn directory_candidate_is_skipped_for_a_later_file() {
        let dir = tempfile::tempdir().unwrap();
        let directory_candidate = dir.path().join("config-directory");
        std::fs::create_dir(&directory_candidate).unwrap();
        let file_candidate = make_file(&dir, "terakzor.toml");

        let result =
            find_config_path(None, None, &[directory_candidate, file_candidate.clone()]).unwrap();

        assert_eq!(result, Some(file_candidate));
    }

    #[test]
    fn config_env_value_handles_missing_and_utf8_values() {
        assert_eq!(config_env_value(None).unwrap(), None);
        assert_eq!(
            config_env_value(Some(OsString::from("config.toml"))).unwrap(),
            Some("config.toml".to_owned())
        );
    }

    #[test]
    fn command_line_args_converts_utf8_values() {
        let args =
            command_line_args([OsString::from("terakzor"), OsString::from("--config")]).unwrap();

        assert_eq!(args, ["terakzor", "--config"]);
    }

    #[cfg(unix)]
    #[test]
    fn command_line_args_rejects_non_utf8_values() {
        use std::os::unix::ffi::OsStringExt;

        let error = command_line_args([OsString::from_vec(vec![0xFF])]).unwrap_err();

        assert!(
            error.to_string().contains("command-line argument"),
            "{error}"
        );
        assert!(error.to_string().contains("UTF-8"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn config_env_value_rejects_non_utf8_values() {
        use std::os::unix::ffi::OsStringExt;

        let error = config_env_value(Some(OsString::from_vec(vec![0xFF]))).unwrap_err();

        assert!(error.to_string().contains("TERAKZOR_CONFIG"), "{error}");
        assert!(error.to_string().contains("UTF-8"), "{error}");
    }

    #[test]
    fn returns_none_when_no_candidates_exist() {
        let result = find_config_path(
            None,
            None,
            &[PathBuf::from("/no/a"), PathBuf::from("/no/b")],
        )
        .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn returns_none_when_no_candidates_given() {
        let result = find_config_path(None, None, &[]).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn candidates_include_cwd_and_user_dir() {
        let candidates = super::resolve_config_candidates();
        assert_eq!(candidates[0], std::path::PathBuf::from("terakzor.toml"));
        if let Some(config_dir) = dirs::config_dir() {
            assert_eq!(
                candidates[1],
                config_dir.join("terakzor").join("terakzor.toml")
            );
        }
    }
}
