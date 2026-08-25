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
    notify_tty: Option<PathBuf>,
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
            notify_tty: None,
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

    /// Device path the submitting terminal asked output to be echoed to.
    pub(crate) fn notify_tty(&self) -> Option<&Path> {
        self.notify_tty.as_deref()
    }

    /// Record a terminal device path for fire-and-forget output echo.
    ///
    /// The path is stored, not validated against the filesystem: by fire time
    /// (possibly days later) any tty may legitimately have vanished.
    pub(crate) fn set_notify_tty(&mut self, notify_tty: PathBuf) -> Result<(), ExecutionError> {
        if notify_tty.as_os_str().is_empty()
            || notify_tty.as_os_str().as_encoded_bytes().contains(&0)
        {
            return Err(ExecutionError::InvalidNotifyTty);
        }
        let serialized_size = self.argv.iter().map(String::len).sum::<usize>()
            + self.environment.serialized_size()
            + self.working_directory.as_os_str().len()
            + notify_tty.as_os_str().len();
        if serialized_size > MAX_SERIALIZED_BYTES {
            return Err(ExecutionError::SerializedSizeExceeded);
        }
        self.notify_tty = Some(notify_tty);
        Ok(())
    }

    pub(crate) fn set_shell_path(&mut self, shell_path: PathBuf) -> Result<(), ExecutionError> {
        if self.mode != ExecutionMode::Shell || !shell_path.is_absolute() {
            return Err(ExecutionError::InvalidShellPath);
        }
        self.shell_path = Some(shell_path);
        Ok(())
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
            notify_tty: self.notify_tty.clone(),
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
        if let Some(notify_tty) = stored.notify_tty {
            // Legacy rows never carry the field; a present field must still
            // satisfy the same invariants submit-time validation applied.
            execution
                .set_notify_tty(notify_tty)
                .map_err(|_| ExecutionStorageError::InvalidNotifyTty)?;
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
    #[serde(default)]
    notify_tty: Option<PathBuf>,
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
    #[error("shell path must be absolute and is only valid in shell mode")]
    InvalidShellPath,
    #[error("invalid environment variable name")]
    InvalidEnvironmentKey,
    #[error("terminal device path cannot be empty or contain NUL")]
    InvalidNotifyTty,
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
    #[error("stored terminal device path is invalid")]
    InvalidNotifyTty,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::path::PathBuf;

    use super::{Environment, ExecutionError, ExecutionMode, ExecutionSpec};

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
    fn notify_tty_roundtrips_and_rejects_bad_paths() {
        let mut spec = ExecutionSpec::new(
            ExecutionMode::Direct,
            vec!["true".to_owned()],
            "/tmp".to_owned(),
            Environment::empty(),
        )
        .expect("valid execution");
        assert!(spec.notify_tty().is_none());

        spec.set_notify_tty(PathBuf::from("/dev/ttys001"))
            .expect("valid tty path");
        let persisted = spec.to_persistence_json().expect("persist");
        let restored = ExecutionSpec::from_persistence_json(&persisted).expect("restore");
        assert_eq!(
            restored.notify_tty(),
            Some(std::path::Path::new("/dev/ttys001"))
        );

        assert!(spec.set_notify_tty(PathBuf::new()).is_err());
        assert!(
            ExecutionSpec::from_persistence_json(
                &serde_json::to_string(&serde_json::json!({
                    "mode": "direct",
                    "argv": ["true"],
                    "working_directory": "/tmp",
                    "environment": {},
                    "stdin": "null",
                    "stdout": "bounded_file",
                    "stderr": "bounded_file",
                    "notify_tty": ""
                }))
                .expect("json")
            )
            .is_err()
        );
    }

    #[test]
    fn persistence_json_rejects_wrong_policy_and_shell_shapes() {
        let base = serde_json::json!({
            "mode": "direct",
            "argv": ["true"],
            "working_directory": "/tmp",
            "environment": {},
            "stdin": "null",
            "stdout": "bounded_file",
            "stderr": "bounded_file",
            "shell_path": null
        });
        // Each of the three stored policies must match the only value this
        // build supports; any other shape is rejected before decode.
        for (field, value) in [
            ("stdin", "inherit"),
            ("stdout", "discard"),
            ("stderr", "null"),
        ] {
            let mut wrong_policy = base.clone();
            wrong_policy[field] = serde_json::json!(value);
            assert!(
                ExecutionSpec::from_persistence_json(
                    &serde_json::to_string(&wrong_policy).expect("json")
                )
                .is_err(),
                "{field} = {value} must be rejected"
            );
        }

        let mut shell_without_path = base.clone();
        shell_without_path["mode"] = serde_json::json!("shell");
        assert!(
            ExecutionSpec::from_persistence_json(
                &serde_json::to_string(&shell_without_path).expect("json")
            )
            .is_err()
        );
        let mut direct_with_path = base;
        direct_with_path["shell_path"] = serde_json::json!("/bin/sh");
        assert!(
            ExecutionSpec::from_persistence_json(
                &serde_json::to_string(&direct_with_path).expect("json")
            )
            .is_err()
        );
    }

    #[test]
    fn set_shell_path_rejects_relative_paths_and_direct_mode() {
        let mut spec = ExecutionSpec::new(
            ExecutionMode::Direct,
            vec!["true".to_owned()],
            "/tmp".to_owned(),
            Environment::empty(),
        )
        .expect("valid execution");
        assert!(spec.set_shell_path(PathBuf::from("/bin/sh")).is_err());

        // In shell mode the replacement must still be absolute.
        let mut shell = ExecutionSpec::new(
            ExecutionMode::Shell,
            vec!["echo hi".to_owned()],
            "/tmp".to_owned(),
            Environment::empty(),
        )
        .expect("valid shell execution");
        assert!(shell.set_shell_path(PathBuf::from("relative/sh")).is_err());
    }

    #[test]
    fn environment_key_value_and_empty_argv_edges_are_rejected() {
        // A leading digit fails the first-byte branch; a hyphen after a valid
        // start fails the remainder branch.
        assert!(Environment::from_pairs([("9x", "value")]).is_err());
        assert!(Environment::from_pairs([("A-B", "value")]).is_err());
        assert!(matches!(
            Environment::from_pairs([("TOKEN", "v\0")]),
            Err(ExecutionError::ContainsNul("environment value"))
        ));
        assert!(matches!(
            ExecutionSpec::new(
                ExecutionMode::Direct,
                Vec::new(),
                "/tmp".to_owned(),
                Environment::empty(),
            ),
            Err(ExecutionError::EmptyArguments)
        ));
    }

    #[test]
    fn notify_tty_nul_and_oversize_paths_are_rejected() {
        let mut spec = ExecutionSpec::new(
            ExecutionMode::Direct,
            vec!["true".to_owned()],
            "/tmp".to_owned(),
            Environment::empty(),
        )
        .expect("valid execution");
        assert!(spec.set_notify_tty(PathBuf::from("/dev/tty\0bad")).is_err());

        // The base spec stays under the serialized-size cap on its own; the
        // tty path alone pushes the total past it.
        let oversize = format!("/dev/{}", "t".repeat(super::MAX_SERIALIZED_BYTES));
        assert!(matches!(
            spec.set_notify_tty(PathBuf::from(oversize)),
            Err(ExecutionError::SerializedSizeExceeded)
        ));
    }

    #[test]
    fn persisted_shell_rows_require_an_absolute_shell_path() {
        let shell_row = serde_json::json!({
            "mode": "shell",
            "argv": ["echo hi"],
            "working_directory": "/tmp",
            "environment": {},
            "stdin": "null",
            "stdout": "bounded_file",
            "stderr": "bounded_file",
            "shell_path": "/bin/bash"
        });
        let restored =
            ExecutionSpec::from_persistence_json(&serde_json::to_string(&shell_row).expect("json"))
                .expect("shell row with absolute path loads");
        assert_eq!(
            restored.shell_path(),
            Some(std::path::Path::new("/bin/bash"))
        );

        let mut relative_shell = shell_row;
        relative_shell["shell_path"] = serde_json::json!("sh");
        assert!(
            ExecutionSpec::from_persistence_json(
                &serde_json::to_string(&relative_shell).expect("json")
            )
            .is_err()
        );
    }

    #[test]
    fn legacy_persistence_json_without_notify_tty_loads() {
        let legacy = serde_json::json!({
            "mode": "direct",
            "argv": ["true"],
            "working_directory": "/tmp",
            "environment": {},
            "stdin": "null",
            "stdout": "bounded_file",
            "stderr": "bounded_file",
            "shell_path": null
        });
        let restored =
            ExecutionSpec::from_persistence_json(&serde_json::to_string(&legacy).expect("json"))
                .expect("legacy row loads");
        assert!(restored.notify_tty().is_none());
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
        // An empty key takes the missing-first-byte branch of the validator.
        assert!(Environment::from_pairs([("", "value")]).is_err());
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
    fn total_size_policy_and_persisted_tty_edges_are_enforced() {
        // Every argument fits its own cap; the serialized total does not.
        assert!(matches!(
            ExecutionSpec::new(
                ExecutionMode::Direct,
                vec!["x".repeat(128 * 1024); 9],
                "/tmp".to_owned(),
                Environment::empty(),
            ),
            Err(ExecutionError::SerializedSizeExceeded)
        ));

        let base = serde_json::json!({
            "mode": "direct",
            "argv": ["true"],
            "working_directory": "/tmp",
            "environment": {},
            "stdin": "null",
            "stdout": "bounded_file",
            "stderr": "bounded_file",
            "shell_path": null
        });

        let mut bad_stderr = base.clone();
        bad_stderr["stderr"] = serde_json::json!("null");
        assert!(
            ExecutionSpec::from_persistence_json(
                &serde_json::to_string(&bad_stderr).expect("json")
            )
            .is_err()
        );

        // A persisted notify tty must still satisfy submit-time invariants.
        let mut with_tty = base.clone();
        with_tty["notify_tty"] = serde_json::json!("/dev/ttys001");
        let restored =
            ExecutionSpec::from_persistence_json(&serde_json::to_string(&with_tty).expect("json"))
                .expect("tty row loads");
        assert_eq!(
            restored.notify_tty(),
            Some(std::path::Path::new("/dev/ttys001"))
        );
        assert_eq!(restored.working_directory(), std::path::Path::new("/tmp"));

        let mut empty_tty = base;
        empty_tty["notify_tty"] = serde_json::json!("");
        assert!(
            ExecutionSpec::from_persistence_json(&serde_json::to_string(&empty_tty).expect("json"))
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

    proptest::proptest! {
        /// Stored arguments must survive the persistence round trip byte for
        /// byte, including shell metacharacters that direct mode must never
        /// interpret.
        #[test]
        fn persistence_json_round_trips_arguments(
            argv in proptest::prelude::prop::collection::vec("[^\0]{1,64}", 1..8),
        ) {
            let spec = ExecutionSpec::new(
                ExecutionMode::Direct,
                argv.clone(),
                "/tmp".to_owned(),
                Environment::empty(),
            )
            .expect("valid generated execution");

            let stored = spec.to_persistence_json().expect("serializable spec");
            let parsed =
                ExecutionSpec::from_persistence_json(&stored).expect("stored spec reloads");

            assert_eq!(parsed.mode(), spec.mode());
            assert_eq!(parsed.argv(), spec.argv());
            assert_eq!(parsed.working_directory(), spec.working_directory());
        }
    }
}
