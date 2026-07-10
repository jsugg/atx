//! Command execution specification.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_ARGUMENT_BYTES: usize = 128 * 1024;
const MAX_SERIALIZED_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutionMode {
    Direct,
    Shell,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct SecretString(String);

impl SecretString {
    fn new(value: String) -> Result<Self, ExecutionError> {
        if value.contains('\0') {
            return Err(ExecutionError::ContainsNul("environment value"));
        }
        Ok(Self(value))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl Serialize for SecretString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("[REDACTED]")
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct Environment(BTreeMap<String, SecretString>);

impl Environment {
    pub(crate) const fn empty() -> Self {
        Self(BTreeMap::new())
    }

    pub(crate) fn from_pairs<K, V, I>(pairs: I) -> Result<Self, ExecutionError>
    where
        K: Into<String>,
        V: Into<String>,
        I: IntoIterator<Item = (K, V)>,
    {
        let mut values = BTreeMap::new();
        for (key, value) in pairs {
            let key = key.into();
            validate_environment_key(&key)?;
            values.insert(key, SecretString::new(value.into())?);
        }
        Ok(Self(values))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &SecretString)> {
        self.0.iter().map(|(key, value)| (key.as_str(), value))
    }

    fn serialized_size(&self) -> usize {
        self.0
            .iter()
            .map(|(key, value)| key.len() + value.expose().len() + 2)
            .sum()
    }
}

fn validate_environment_key(key: &str) -> Result<(), ExecutionError> {
    let mut bytes = key.bytes();
    let Some(first) = bytes.next() else {
        return Err(ExecutionError::InvalidEnvironmentKey);
    };
    if !(first == b'_' || first.is_ascii_alphabetic())
        || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
    {
        return Err(ExecutionError::InvalidEnvironmentKey);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StdinPolicy {
    Null,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutputPolicy {
    BoundedFile,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ExecutionSpec {
    mode: ExecutionMode,
    argv: Vec<String>,
    working_directory: PathBuf,
    environment: Environment,
    stdin: StdinPolicy,
    stdout: OutputPolicy,
    stderr: OutputPolicy,
    shell_path: Option<PathBuf>,
}

impl ExecutionSpec {
    pub(crate) fn new(
        mode: ExecutionMode,
        argv: Vec<String>,
        working_directory: String,
        environment: Environment,
    ) -> Result<Self, ExecutionError> {
        if argv.is_empty() {
            return Err(ExecutionError::EmptyArguments);
        }
        if mode == ExecutionMode::Shell && argv.len() != 1 {
            return Err(ExecutionError::ShellArgumentCount);
        }
        for argument in &argv {
            if argument.contains('\0') {
                return Err(ExecutionError::ContainsNul("argument"));
            }
            if argument.len() > MAX_ARGUMENT_BYTES {
                return Err(ExecutionError::ArgumentTooLarge);
            }
        }

        let working_directory = PathBuf::from(working_directory);
        if !working_directory.is_absolute() {
            return Err(ExecutionError::WorkingDirectoryNotAbsolute);
        }

        let serialized_size = argv.iter().map(String::len).sum::<usize>()
            + environment.serialized_size()
            + working_directory.as_os_str().len();
        if serialized_size > MAX_SERIALIZED_BYTES {
            return Err(ExecutionError::SerializedSizeExceeded);
        }

        Ok(Self {
            mode,
            argv,
            working_directory,
            environment,
            stdin: StdinPolicy::Null,
            stdout: OutputPolicy::BoundedFile,
            stderr: OutputPolicy::BoundedFile,
            shell_path: (mode == ExecutionMode::Shell).then(|| PathBuf::from("/bin/sh")),
        })
    }

    pub(crate) fn mode(&self) -> ExecutionMode {
        self.mode
    }

    pub(crate) fn argv(&self) -> &[String] {
        &self.argv
    }

    pub(crate) fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    pub(crate) fn environment(&self) -> &Environment {
        &self.environment
    }

    pub(crate) fn shell_path(&self) -> Option<&Path> {
        self.shell_path.as_deref()
    }

    pub(crate) fn to_persistence_json(&self) -> Result<String, ExecutionStorageError> {
        let environment = self
            .environment
            .iter()
            .map(|(key, value)| (key.to_owned(), value.expose().to_owned()))
            .collect();
        serde_json::to_string(&PersistedExecutionSpec {
            mode: self.mode,
            argv: self.argv.clone(),
            working_directory: self.working_directory.clone(),
            environment,
            stdin: self.stdin,
            stdout: self.stdout,
            stderr: self.stderr,
            shell_path: self.shell_path.clone(),
        })
        .map_err(|error| ExecutionStorageError::Json(error.to_string()))
    }

    pub(crate) fn from_persistence_json(input: &str) -> Result<Self, ExecutionStorageError> {
        let stored: PersistedExecutionSpec = serde_json::from_str(input)
            .map_err(|error| ExecutionStorageError::Json(error.to_string()))?;
        if stored.stdin != StdinPolicy::Null
            || stored.stdout != OutputPolicy::BoundedFile
            || stored.stderr != OutputPolicy::BoundedFile
        {
            return Err(ExecutionStorageError::InvalidPolicy);
        }
        let environment =
            Environment::from_pairs(stored.environment).map_err(ExecutionStorageError::Invalid)?;
        let mut execution = Self::new(
            stored.mode,
            stored.argv,
            stored.working_directory.to_string_lossy().into_owned(),
            environment,
        )
        .map_err(ExecutionStorageError::Invalid)?;
        match (stored.mode, stored.shell_path) {
            (ExecutionMode::Direct, None) => {}
            (ExecutionMode::Shell, Some(path)) if path.is_absolute() => {
                execution.shell_path = Some(path);
            }
            _ => return Err(ExecutionStorageError::InvalidShell),
        }
        Ok(execution)
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct PersistedExecutionSpec {
    mode: ExecutionMode,
    argv: Vec<String>,
    working_directory: PathBuf,
    environment: BTreeMap<String, String>,
    stdin: StdinPolicy,
    stdout: OutputPolicy,
    stderr: OutputPolicy,
    shell_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum ExecutionError {
    #[error("command arguments cannot be empty")]
    EmptyArguments,
    #[error("shell mode requires exactly one command string")]
    ShellArgumentCount,
    #[error("{0} cannot contain NUL")]
    ContainsNul(&'static str),
    #[error("one command argument exceeds 128 KiB")]
    ArgumentTooLarge,
    #[error("argv and environment exceed 1 MiB")]
    SerializedSizeExceeded,
    #[error("working directory must be absolute")]
    WorkingDirectoryNotAbsolute,
    #[error("invalid environment variable name")]
    InvalidEnvironmentKey,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum ExecutionStorageError {
    #[error("stored execution JSON is invalid: {0}")]
    Json(String),
    #[error("stored execution is invalid: {0}")]
    Invalid(ExecutionError),
    #[error("stored execution policies are unsupported")]
    InvalidPolicy,
    #[error("stored shell path does not match execution mode")]
    InvalidShell,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{Environment, ExecutionMode, ExecutionSpec};

    #[test]
    fn direct_mode_preserves_arguments() {
        let spec = ExecutionSpec::new(
            ExecutionMode::Direct,
            vec![
                "printf".to_owned(),
                "%s\n".to_owned(),
                "a; rm -rf /".to_owned(),
            ],
            "/tmp".to_owned(),
            Environment::empty(),
        )
        .expect("valid direct execution");

        assert_eq!(spec.argv(), ["printf", "%s\n", "a; rm -rf /"]);
    }

    #[test]
    fn shell_mode_requires_one_string() {
        assert!(
            ExecutionSpec::new(
                ExecutionMode::Shell,
                vec!["echo".to_owned(), "hello".to_owned()],
                "/tmp".to_owned(),
                Environment::empty(),
            )
            .is_err()
        );
    }

    #[test]
    fn environment_values_are_redacted_everywhere() {
        let environment =
            Environment::from_pairs([("TOKEN", "swordfish")]).expect("valid environment value");
        let spec = ExecutionSpec::new(
            ExecutionMode::Direct,
            vec!["true".to_owned()],
            "/tmp".to_owned(),
            environment,
        )
        .expect("valid execution");

        assert!(!format!("{spec:?}").contains("swordfish"));
        assert!(
            !serde_json::to_string(&spec)
                .expect("serializable")
                .contains("swordfish")
        );
    }

    #[test]
    fn rejects_nul_and_oversize_arguments() {
        assert!(
            ExecutionSpec::new(
                ExecutionMode::Direct,
                vec!["bad\0arg".to_owned()],
                "/tmp".to_owned(),
                Environment::empty(),
            )
            .is_err()
        );
        assert!(
            ExecutionSpec::new(
                ExecutionMode::Direct,
                vec!["x".repeat(128 * 1024 + 1)],
                "/tmp".to_owned(),
                Environment::empty(),
            )
            .is_err()
        );
    }

    #[test]
    fn validates_environment_cwd_and_total_size() {
        assert!(Environment::from_pairs([("BAD-KEY", "value")]).is_err());
        assert!(
            ExecutionSpec::new(
                ExecutionMode::Direct,
                vec!["true".to_owned()],
                "relative".to_owned(),
                Environment::empty(),
            )
            .is_err()
        );
        assert!(
            ExecutionSpec::new(
                ExecutionMode::Direct,
                vec!["x".repeat(128 * 1024); 8],
                "/tmp".to_owned(),
                Environment::empty(),
            )
            .is_err()
        );
    }

    #[test]
    fn getters_preserve_validated_values() {
        let environment = Environment::from_pairs([("PATH", "/bin")]).expect("valid environment");
        let spec = ExecutionSpec::new(
            ExecutionMode::Direct,
            vec!["true".to_owned()],
            "/tmp".to_owned(),
            environment,
        )
        .expect("valid execution");

        assert_eq!(spec.mode(), ExecutionMode::Direct);
        assert_eq!(spec.working_directory(), std::path::Path::new("/tmp"));
        let values = spec.environment().iter().collect::<Vec<_>>();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].0, "PATH");
        assert_eq!(values[0].1.expose(), "/bin");
    }
}
