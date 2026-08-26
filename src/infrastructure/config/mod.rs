//! Configuration adapter.

use std::fs;
use std::path::{Path, PathBuf};

use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{DurationSeconds, RuntimeTier};

const DEFAULT_TIMEZONE: &str = "local";
const DEFAULT_SHELL: &str = "/bin/sh";
const DEFAULT_CANCEL_GRACE: &str = "10s";
const DEFAULT_HISTORY_DAYS: u16 = 30;
const DEFAULT_TERMINAL_JOB_DAYS: u16 = 30;
const DEFAULT_MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
const MAX_RETENTION_DAYS: u16 = 3_650;
const MAX_LOG_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verbosity {
    Quiet,
    Normal,
    Verbose,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ConfigOverrides {
    pub(crate) default_timezone: Option<String>,
    pub(crate) default_runtime: Option<RuntimeTier>,
    pub(crate) default_shell: Option<PathBuf>,
    pub(crate) cancel_grace: Option<String>,
    pub(crate) history_days: Option<u16>,
    pub(crate) terminal_job_days: Option<u16>,
    pub(crate) max_log_bytes_per_stream: Option<u64>,
    pub(crate) color: Option<ColorMode>,
    pub(crate) verbosity: Option<Verbosity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Config {
    default_timezone: String,
    default_runtime: RuntimeTier,
    default_shell: PathBuf,
    cancel_grace: DurationSeconds,
    history_days: u16,
    terminal_job_days: u16,
    max_log_bytes_per_stream: u64,
    color: ColorMode,
    verbosity: Verbosity,
}

impl Config {
    pub(crate) fn default_timezone(&self) -> &str {
        &self.default_timezone
    }

    pub(crate) const fn default_runtime(&self) -> RuntimeTier {
        self.default_runtime
    }

    pub(crate) fn default_shell(&self) -> &Path {
        &self.default_shell
    }

    pub(crate) const fn cancel_grace(&self) -> DurationSeconds {
        self.cancel_grace
    }

    pub(crate) const fn history_days(&self) -> u16 {
        self.history_days
    }

    pub(crate) const fn terminal_job_days(&self) -> u16 {
        self.terminal_job_days
    }

    pub(crate) const fn max_log_bytes_per_stream(&self) -> u64 {
        self.max_log_bytes_per_stream
    }

    pub(crate) const fn color(&self) -> ColorMode {
        self.color
    }

    pub(crate) const fn verbosity(&self) -> Verbosity {
        self.verbosity
    }

    pub(crate) fn redacted(&self) -> RedactedConfig<'_> {
        RedactedConfig {
            default_timezone: &self.default_timezone,
            default_runtime: self.default_runtime,
            default_shell: &self.default_shell,
            cancel_grace: self.cancel_grace.to_string(),
            history_days: self.history_days,
            terminal_job_days: self.terminal_job_days,
            max_log_bytes_per_stream: self.max_log_bytes_per_stream,
            color: self.color,
            verbosity: self.verbosity,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RedactedConfig<'a> {
    default_timezone: &'a str,
    default_runtime: RuntimeTier,
    default_shell: &'a Path,
    cancel_grace: String,
    history_days: u16,
    terminal_job_days: u16,
    max_log_bytes_per_stream: u64,
    color: ColorMode,
    verbosity: Verbosity,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    default_timezone: Option<String>,
    default_runtime: Option<RuntimeTier>,
    default_shell: Option<PathBuf>,
    cancel_grace: Option<String>,
    history_days: Option<u16>,
    terminal_job_days: Option<u16>,
    max_log_bytes_per_stream: Option<u64>,
    color: Option<ColorMode>,
    verbosity: Option<Verbosity>,
}

impl RawConfig {
    fn apply(&mut self, other: Self) {
        macro_rules! replace_some {
            ($($field:ident),+ $(,)?) => {
                $(if other.$field.is_some() {
                    self.$field = other.$field;
                })+
            };
        }
        replace_some!(
            default_timezone,
            default_runtime,
            default_shell,
            cancel_grace,
            history_days,
            terminal_job_days,
            max_log_bytes_per_stream,
            color,
            verbosity,
        );
    }
}

impl From<ConfigOverrides> for RawConfig {
    fn from(value: ConfigOverrides) -> Self {
        Self {
            default_timezone: value.default_timezone,
            default_runtime: value.default_runtime,
            default_shell: value.default_shell,
            cancel_grace: value.cancel_grace,
            history_days: value.history_days,
            terminal_job_days: value.terminal_job_days,
            max_log_bytes_per_stream: value.max_log_bytes_per_stream,
            color: value.color,
            verbosity: value.verbosity,
        }
    }
}

pub(crate) fn load_config(
    file: Option<&str>,
    environment: &[(String, String)],
    overrides: ConfigOverrides,
) -> Result<Config, ConfigError> {
    let mut raw: RawConfig = file
        .map(toml::from_str)
        .transpose()
        .map_err(|error| ConfigError::Toml(error.to_string()))?
        .unwrap_or_default();
    raw.apply(environment_layer(environment)?);
    raw.apply(overrides.into());
    validate(raw)
}

/// Load the effective configuration for a background process (supervisor or
/// run monitor): file layer plus environment layer, no CLI overrides.
///
/// Every documented resource control must reach these processes through this
/// boundary; hardcoding defaults here silently discards operator settings.
pub(crate) fn load_process_config(state_directory: &Path) -> Result<Config, ConfigError> {
    use std::os::unix::ffi::OsStrExt;

    let config_path = state_directory.join("config.toml");
    let file = match fs::read_to_string(&config_path) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(ConfigError::Read(format!(
                "cannot read {}: {error}",
                config_path.display()
            )));
        }
    };
    // vars_os + explicit validation: std::env::vars() panics on non-UTF-8
    // entries. Only ATX_* names cross the configuration boundary.
    let mut environment = Vec::new();
    for (key, value) in std::env::vars_os() {
        if !key.as_os_str().as_bytes().starts_with(b"ATX_") {
            continue;
        }
        let key = key.into_string().map_err(|invalid| {
            ConfigError::Read(format!(
                "environment variable name is not valid UTF-8: {}",
                invalid.to_string_lossy()
            ))
        })?;
        let value = value.into_string().map_err(|_| {
            ConfigError::Read(format!("environment variable {key} is not valid UTF-8"))
        })?;
        environment.push((key, value));
    }
    load_config(file.as_deref(), &environment, ConfigOverrides::default())
}

fn environment_layer(environment: &[(String, String)]) -> Result<RawConfig, ConfigError> {
    let mut raw = RawConfig::default();
    for (key, value) in environment {
        match key.as_str() {
            "ATX_DEFAULT_TIMEZONE" => raw.default_timezone = Some(value.clone()),
            "ATX_DEFAULT_RUNTIME" => {
                raw.default_runtime = Some(parse_runtime(value, key)?);
            }
            "ATX_DEFAULT_SHELL" => raw.default_shell = Some(PathBuf::from(value)),
            "ATX_CANCEL_GRACE" => raw.cancel_grace = Some(value.clone()),
            "ATX_HISTORY_DAYS" => raw.history_days = Some(parse_env(value, key)?),
            "ATX_TERMINAL_JOB_DAYS" => {
                raw.terminal_job_days = Some(parse_env(value, key)?);
            }
            "ATX_MAX_LOG_BYTES_PER_STREAM" => {
                raw.max_log_bytes_per_stream = Some(parse_env(value, key)?);
            }
            "ATX_COLOR" => raw.color = Some(parse_color(value, key)?),
            "ATX_VERBOSITY" => raw.verbosity = Some(parse_verbosity(value, key)?),
            _ if key.starts_with("ATX_") => {
                return Err(ConfigError::UnknownEnvironment(key.clone()));
            }
            _ => {}
        }
    }
    Ok(raw)
}

fn parse_env<T: std::str::FromStr>(value: &str, key: &str) -> Result<T, ConfigError> {
    value.parse().map_err(|_| ConfigError::InvalidEnvironment {
        name: key.to_owned(),
    })
}

fn parse_runtime(value: &str, key: &str) -> Result<RuntimeTier, ConfigError> {
    match value {
        "session" => Ok(RuntimeTier::Session),
        "durable" => Ok(RuntimeTier::Durable),
        _ => Err(ConfigError::InvalidEnvironment {
            name: key.to_owned(),
        }),
    }
}

fn parse_color(value: &str, key: &str) -> Result<ColorMode, ConfigError> {
    match value {
        "auto" => Ok(ColorMode::Auto),
        "always" => Ok(ColorMode::Always),
        "never" => Ok(ColorMode::Never),
        _ => Err(ConfigError::InvalidEnvironment {
            name: key.to_owned(),
        }),
    }
}

fn parse_verbosity(value: &str, key: &str) -> Result<Verbosity, ConfigError> {
    match value {
        "quiet" => Ok(Verbosity::Quiet),
        "normal" => Ok(Verbosity::Normal),
        "verbose" => Ok(Verbosity::Verbose),
        _ => Err(ConfigError::InvalidEnvironment {
            name: key.to_owned(),
        }),
    }
}

fn validate(raw: RawConfig) -> Result<Config, ConfigError> {
    let default_timezone = raw
        .default_timezone
        .unwrap_or_else(|| DEFAULT_TIMEZONE.to_owned());
    if default_timezone != "local" && TimeZone::get(&default_timezone).is_err() {
        return Err(ConfigError::InvalidField("default_timezone"));
    }

    let default_shell = raw
        .default_shell
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SHELL));
    if !default_shell.is_absolute() || default_shell.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(ConfigError::InvalidField("default_shell"));
    }

    let cancel_grace = raw
        .cancel_grace
        .unwrap_or_else(|| DEFAULT_CANCEL_GRACE.to_owned())
        .parse()
        .map_err(|_| ConfigError::InvalidField("cancel_grace"))?;
    let history_days = checked_days(raw.history_days.unwrap_or(DEFAULT_HISTORY_DAYS))?;
    let terminal_job_days =
        checked_days(raw.terminal_job_days.unwrap_or(DEFAULT_TERMINAL_JOB_DAYS))?;
    let max_log_bytes_per_stream = raw
        .max_log_bytes_per_stream
        .unwrap_or(DEFAULT_MAX_LOG_BYTES);
    if !(1..=MAX_LOG_BYTES).contains(&max_log_bytes_per_stream) {
        return Err(ConfigError::InvalidField("max_log_bytes_per_stream"));
    }

    Ok(Config {
        default_timezone,
        default_runtime: raw.default_runtime.unwrap_or(RuntimeTier::Session),
        default_shell,
        cancel_grace,
        history_days,
        terminal_job_days,
        max_log_bytes_per_stream,
        color: raw.color.unwrap_or(ColorMode::Auto),
        verbosity: raw.verbosity.unwrap_or(Verbosity::Normal),
    })
}

fn checked_days(value: u16) -> Result<u16, ConfigError> {
    if !(1..=MAX_RETENTION_DAYS).contains(&value) {
        return Err(ConfigError::InvalidField("retention_days"));
    }
    Ok(value)
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum ConfigError {
    #[error("invalid TOML configuration: {0}")]
    Toml(String),
    #[error("{0}")]
    Read(String),
    #[error("unknown ATX environment variable {0}")]
    UnknownEnvironment(String),
    #[error("invalid value for environment variable {name}")]
    InvalidEnvironment { name: String },
    #[error("invalid configuration field {0}")]
    InvalidField(&'static str),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{ColorMode, ConfigOverrides, RuntimeTier, Verbosity, load_config};

    fn env(values: &[(&str, &str)]) -> Vec<(String, String)> {
        values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn layers_use_documented_precedence() {
        let config = load_config(
            Some("history_days = 10\ndefault_runtime = \"durable\""),
            &env(&[("ATX_HISTORY_DAYS", "20")]),
            ConfigOverrides {
                history_days: Some(40),
                ..ConfigOverrides::default()
            },
        )
        .expect("valid layered config");

        assert_eq!(config.history_days(), 40);
        assert_eq!(config.default_runtime(), RuntimeTier::Durable);
        assert_eq!(config.terminal_job_days(), 30);
    }

    #[test]
    fn unknown_and_malformed_values_are_rejected() {
        assert!(load_config(Some("histroy_days = 10"), &[], ConfigOverrides::default()).is_err());
        assert!(
            load_config(
                None,
                &env(&[("ATX_HISTROY_DAYS", "10")]),
                ConfigOverrides::default(),
            )
            .is_err()
        );
        assert!(
            load_config(
                Some("cancel_grace = \"later\""),
                &[],
                ConfigOverrides::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn safety_bounds_are_checked() {
        for source in [
            "max_log_bytes_per_stream = 0",
            "max_log_bytes_per_stream = 1073741825",
            "history_days = 0",
            "terminal_job_days = 3651",
            "default_shell = \"relative/sh\"",
            "default_timezone = \"Mars/Olympus\"",
        ] {
            assert!(
                load_config(Some(source), &[], ConfigOverrides::default()).is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn default_shell_with_an_embedded_nul_byte_is_rejected() {
        assert!(
            load_config(
                None,
                &env(&[("ATX_DEFAULT_SHELL", "/bin/sh\0")]),
                ConfigOverrides::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn redacted_view_contains_only_effective_safe_values() {
        let unrelated_secret = "do-not-leak";
        let config = load_config(
            None,
            &env(&[("HOME", unrelated_secret), ("ATX_VERBOSITY", "quiet")]),
            ConfigOverrides::default(),
        )
        .expect("valid config");

        let output = serde_json::to_string(&config.redacted()).expect("serialize redacted config");
        assert!(!output.contains(unrelated_secret));
        assert!(output.contains("\"verbosity\":\"quiet\""));
        assert!(output.contains("\"max_log_bytes_per_stream\":10485760"));
    }

    #[test]
    fn numeric_environment_variables_reject_unparseable_values() {
        for (key, value) in [
            ("ATX_HISTORY_DAYS", "not-a-number"),
            ("ATX_TERMINAL_JOB_DAYS", "abc"),
            ("ATX_MAX_LOG_BYTES_PER_STREAM", "huge"),
        ] {
            assert!(
                load_config(None, &env(&[(key, value)]), ConfigOverrides::default()).is_err(),
                "{key}={value}"
            );
        }
    }

    #[test]
    fn enum_environment_variables_reject_unknown_values() {
        for (key, value) in [
            ("ATX_DEFAULT_RUNTIME", "batch"),
            ("ATX_COLOR", "sometimes"),
            ("ATX_VERBOSITY", "loud"),
        ] {
            assert!(
                load_config(None, &env(&[(key, value)]), ConfigOverrides::default()).is_err(),
                "{key}={value}"
            );
        }
    }

    #[test]
    fn valid_enum_environment_variables_parse_into_their_variants() {
        let config = load_config(
            None,
            &env(&[
                ("ATX_DEFAULT_RUNTIME", "durable"),
                ("ATX_COLOR", "always"),
                ("ATX_VERBOSITY", "verbose"),
            ]),
            ConfigOverrides::default(),
        )
        .expect("valid enum environment");

        assert_eq!(config.default_runtime(), RuntimeTier::Durable);
        assert_eq!(config.color(), ColorMode::Always);
        assert_eq!(config.verbosity(), Verbosity::Verbose);
    }

    #[test]
    fn timezone_default_and_valid_named_zone_are_accepted() {
        assert_eq!(
            load_config(None, &[], ConfigOverrides::default())
                .expect("default config")
                .default_timezone(),
            "local"
        );

        let config = load_config(
            None,
            &env(&[("ATX_DEFAULT_TIMEZONE", "UTC")]),
            ConfigOverrides::default(),
        )
        .expect("valid named timezone");
        assert_eq!(config.default_timezone(), "UTC");
    }

    #[test]
    fn shell_default_and_absolute_path_are_accepted() {
        assert_eq!(
            load_config(None, &[], ConfigOverrides::default())
                .expect("default config")
                .default_shell(),
            std::path::Path::new("/bin/sh")
        );

        let config = load_config(
            None,
            &env(&[("ATX_DEFAULT_SHELL", "/usr/bin/zsh")]),
            ConfigOverrides::default(),
        )
        .expect("absolute shell path");
        assert_eq!(config.default_shell(), std::path::Path::new("/usr/bin/zsh"));
    }

    #[test]
    fn overrides_populate_every_configurable_field() {
        let config = load_config(
            None,
            &[],
            ConfigOverrides {
                default_timezone: Some("UTC".to_owned()),
                default_runtime: Some(RuntimeTier::Durable),
                default_shell: Some(std::path::PathBuf::from("/bin/bash")),
                cancel_grace: Some("5s".to_owned()),
                history_days: Some(7),
                terminal_job_days: Some(14),
                max_log_bytes_per_stream: Some(1024),
                color: Some(ColorMode::Never),
                verbosity: Some(Verbosity::Quiet),
            },
        )
        .expect("fully overridden config");

        assert_eq!(config.default_timezone(), "UTC");
        assert_eq!(config.default_runtime(), RuntimeTier::Durable);
        assert_eq!(config.default_shell(), std::path::Path::new("/bin/bash"));
        assert_eq!(config.cancel_grace().to_string(), "5s");
        assert_eq!(config.history_days(), 7);
        assert_eq!(config.terminal_job_days(), 14);
        assert_eq!(config.max_log_bytes_per_stream(), 1024);
        assert_eq!(config.color(), ColorMode::Never);
        assert_eq!(config.verbosity(), Verbosity::Quiet);
    }

    #[test]
    fn max_log_bytes_accepts_bounds_and_rejects_out_of_range() {
        assert_eq!(
            load_config(
                None,
                &env(&[("ATX_MAX_LOG_BYTES_PER_STREAM", "1")]),
                ConfigOverrides::default(),
            )
            .expect("minimum accepted")
            .max_log_bytes_per_stream(),
            1
        );
        assert_eq!(
            load_config(
                None,
                &env(&[("ATX_MAX_LOG_BYTES_PER_STREAM", "1073741824")]),
                ConfigOverrides::default(),
            )
            .expect("maximum accepted")
            .max_log_bytes_per_stream(),
            1024 * 1024 * 1024
        );
        assert!(
            load_config(
                None,
                &env(&[("ATX_MAX_LOG_BYTES_PER_STREAM", "1073741825")]),
                ConfigOverrides::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn full_toml_document_parses_with_defaults_for_omitted_fields() {
        let config = load_config(
            Some(
                "default_timezone = \"local\"\n\
                 default_runtime = \"session\"\n\
                 default_shell = \"/bin/sh\"\n\
                 cancel_grace = \"10s\"\n\
                 history_days = 30\n\
                 terminal_job_days = 30\n\
                 max_log_bytes_per_stream = 10485760\n\
                 color = \"auto\"\n\
                 verbosity = \"normal\"",
            ),
            &[],
            ConfigOverrides::default(),
        )
        .expect("fully specified inline config");

        assert_eq!(config.default_timezone(), "local");
        assert_eq!(config.cancel_grace().to_string(), "10s");
        assert_eq!(config.history_days(), 30);
        assert_eq!(config.terminal_job_days(), 30);
        assert_eq!(config.max_log_bytes_per_stream(), 10 * 1024 * 1024);
        assert_eq!(config.color(), ColorMode::Auto);
        assert_eq!(config.verbosity(), Verbosity::Normal);
    }
}
