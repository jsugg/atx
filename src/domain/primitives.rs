//! Small validated values shared by jobs and runs.

use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;

use jiff::Timestamp;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum PrimitiveError {
    #[error("{field} cannot contain NUL")]
    ContainsNul { field: &'static str },
    #[error("{field} is longer than {maximum} Unicode scalar values")]
    TooLong { field: &'static str, maximum: usize },
    #[error("{field} must be greater than zero")]
    Zero { field: &'static str },
    #[error("{field} overflowed")]
    Overflow { field: &'static str },
    #[error("timestamp is outside the supported range")]
    TimestampRange,
}

fn validate_text(
    value: String,
    field: &'static str,
    maximum: usize,
) -> Result<String, PrimitiveError> {
    if value.contains('\0') {
        return Err(PrimitiveError::ContainsNul { field });
    }
    if value.chars().count() > maximum {
        return Err(PrimitiveError::TooLong { field, maximum });
    }
    Ok(value)
}

macro_rules! validated_text {
    ($name:ident, $field:literal, $maximum:expr) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub(crate) struct $name(String);

        impl $name {
            pub(crate) fn new(value: impl Into<String>) -> Result<Self, PrimitiveError> {
                validate_text(value.into(), $field, $maximum).map(Self)
            }

            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

validated_text!(Name, "name", 256);
validated_text!(Description, "description", 4096);

macro_rules! nonzero_counter {
    ($name:ident, $field:literal) => {
        #[derive(
            Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub(crate) struct $name(NonZeroU64);

        impl $name {
            pub(crate) fn new(value: u64) -> Result<Self, PrimitiveError> {
                NonZeroU64::new(value)
                    .map(Self)
                    .ok_or(PrimitiveError::Zero { field: $field })
            }

            pub(crate) const fn get(self) -> u64 {
                self.0.get()
            }
        }
    };
}

nonzero_counter!(Revision, "revision");
nonzero_counter!(Sequence, "sequence");

impl Revision {
    pub(crate) fn next(self) -> Result<Self, PrimitiveError> {
        self.get()
            .checked_add(1)
            .ok_or(PrimitiveError::Overflow { field: "revision" })
            .and_then(Self::new)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct UtcTimestamp(Timestamp);

impl UtcTimestamp {
    pub(crate) const fn from_jiff(timestamp: Timestamp) -> Self {
        Self(timestamp)
    }

    #[cfg(test)]
    pub(crate) fn from_second(second: i64) -> Result<Self, PrimitiveError> {
        Timestamp::new(second, 0)
            .map(Self)
            .map_err(|_| PrimitiveError::TimestampRange)
    }

    pub(crate) const fn as_jiff(self) -> Timestamp {
        self.0
    }
}

impl fmt::Display for UtcTimestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for UtcTimestamp {
    type Err = PrimitiveError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<Timestamp>()
            .map(Self)
            .map_err(|_| PrimitiveError::TimestampRange)
    }
}

impl Serialize for UtcTimestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for UtcTimestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{Description, Name, Revision, Sequence, UtcTimestamp};

    #[test]
    fn names_check_scalar_limit_and_nul() {
        assert!(Name::new("x".repeat(256)).is_ok());
        assert!(Name::new("x".repeat(257)).is_err());
        assert!(Name::new("bad\0name").is_err());
    }

    #[test]
    fn descriptions_check_scalar_limit_and_nul() {
        assert!(Description::new("x".repeat(4096)).is_ok());
        assert!(Description::new("x".repeat(4097)).is_err());
        assert!(Description::new("bad\0description").is_err());
    }

    #[test]
    fn revisions_and_sequences_are_nonzero_and_checked() {
        assert!(Revision::new(0).is_err());
        assert!(Sequence::new(0).is_err());
        assert_eq!(
            Revision::new(1).and_then(Revision::next).map(Revision::get),
            Ok(2)
        );
        assert!(Revision::new(u64::MAX).and_then(Revision::next).is_err());
    }

    #[test]
    fn timestamp_round_trips_as_rfc3339() {
        let timestamp = UtcTimestamp::from_second(1_784_204_100).expect("valid timestamp");
        let encoded = serde_json::to_string(&timestamp).expect("timestamp should serialize");
        let decoded: UtcTimestamp =
            serde_json::from_str(&encoded).expect("timestamp should deserialize");
        assert_eq!(decoded, timestamp);
    }

    #[test]
    fn getters_return_validated_values() {
        let name = Name::new("tea").expect("valid name");
        let description = Description::new("brew timer").expect("valid description");
        let sequence = Sequence::new(7).expect("valid sequence");
        let timestamp = UtcTimestamp::from_second(10).expect("valid timestamp");

        assert_eq!(name.as_str(), "tea");
        assert_eq!(description.as_str(), "brew timer");
        assert_eq!(sequence.get(), 7);
        assert_eq!(timestamp.as_jiff().as_second(), 10);
        assert!(serde_json::from_str::<Name>(&format!("\"{}\"", "x".repeat(257))).is_err());
    }
}
