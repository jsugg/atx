//! Checked duration parsing and canonical formatting.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

pub(crate) const MAX_DURATION_SECONDS: u64 = 365 * 24 * 60 * 60;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct DurationSeconds(u64);

impl DurationSeconds {
    pub(crate) fn new(seconds: u64) -> Result<Self, DurationError> {
        if !(1..=MAX_DURATION_SECONDS).contains(&seconds) {
            return Err(DurationError::OutOfRange);
        }
        Ok(Self(seconds))
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

impl FromStr for DurationSeconds {
    type Err = DurationError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let input = input.trim();
        if input.is_empty() {
            return Err(DurationError::Empty);
        }

        let bytes = input.as_bytes();
        let mut index = 0;
        let mut previous_rank = 4;
        let mut total = 0_u64;

        while index < bytes.len() {
            let number_start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            if number_start == index {
                return Err(DurationError::ExpectedNumber);
            }

            let number = input[number_start..index]
                .parse::<u64>()
                .map_err(|_| DurationError::Overflow)?;
            let unit = *bytes.get(index).ok_or(DurationError::ExpectedUnit)?;
            index += 1;

            let (rank, multiplier) = match unit {
                b'h' => {
                    if number == 0 {
                        return Err(DurationError::ZeroHours);
                    }
                    (3, 60 * 60)
                }
                b'm' => (2, 60),
                b's' => (1, 1),
                _ => return Err(DurationError::ExpectedUnit),
            };
            if rank >= previous_rank {
                return Err(DurationError::UnitOrder);
            }
            previous_rank = rank;

            let component = number
                .checked_mul(multiplier)
                .ok_or(DurationError::Overflow)?;
            total = total
                .checked_add(component)
                .ok_or(DurationError::Overflow)?;

            if index < bytes.len() && bytes[index].is_ascii_whitespace() {
                while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                    index += 1;
                }
                if index == bytes.len() {
                    return Err(DurationError::ExpectedNumber);
                }
            }
        }

        Self::new(total)
    }
}

impl fmt::Display for DurationSeconds {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hours = self.0 / 3_600;
        let minutes = (self.0 % 3_600) / 60;
        let seconds = self.0 % 60;

        if hours > 0 {
            write!(formatter, "{hours}h")?;
        }
        if minutes > 0 {
            write!(formatter, "{minutes}m")?;
        }
        if seconds > 0 {
            write!(formatter, "{seconds}s")?;
        }
        Ok(())
    }
}

impl Serialize for DurationSeconds {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DurationSeconds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum DurationError {
    #[error("duration cannot be empty")]
    Empty,
    #[error("expected a decimal duration component")]
    ExpectedNumber,
    #[error("expected one of h, m, or s")]
    ExpectedUnit,
    #[error("duration units must be unique and ordered h, m, s")]
    UnitOrder,
    #[error("the hour component must be nonzero")]
    ZeroHours,
    #[error("duration arithmetic overflowed")]
    Overflow,
    #[error("duration must be between one second and 365 days")]
    OutOfRange,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::str::FromStr;

    use proptest::prelude::*;

    use super::{DurationSeconds, MAX_DURATION_SECONDS};

    #[test]
    fn accepted_forms_are_canonicalized() {
        let cases = [
            ("30s", "30s"),
            ("2m", "2m"),
            ("5h", "5h"),
            ("1m30s", "1m30s"),
            ("2h15m", "2h15m"),
            ("1h 30m 15s", "1h30m15s"),
            ("90s", "1m30s"),
            ("120m", "2h"),
        ];
        for (input, canonical) in cases {
            assert_eq!(
                DurationSeconds::from_str(input)
                    .expect("accepted duration")
                    .to_string(),
                canonical
            );
        }
    }

    #[test]
    fn rejects_invalid_grammar_and_bounds() {
        for input in ["", "0s", "-5m", "1.5h", "30s1m", "1m1m", "1 h"] {
            assert!(DurationSeconds::from_str(input).is_err(), "{input}");
        }
        assert!(DurationSeconds::from_str("8761h").is_err());
        assert!(DurationSeconds::from_str("18446744073709551616s").is_err());
    }

    proptest! {
        #[test]
        fn canonical_values_round_trip(seconds in 1_u64..=MAX_DURATION_SECONDS) {
            let duration = DurationSeconds::new(seconds).expect("generated in range");
            let decoded = duration.to_string().parse::<DurationSeconds>();
            prop_assert_eq!(decoded, Ok(duration));
        }

        #[test]
        fn arbitrary_utf8_never_panics(input in "\\PC{0,128}") {
            let _ = DurationSeconds::from_str(&input);
        }
    }
}
