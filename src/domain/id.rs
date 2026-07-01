//! Validated domain identifiers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use uuid::Uuid;

const ENCODED_LEN: usize = 26;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("identifier must be 26 canonical Crockford Base32 characters")]
pub(crate) struct IdentifierError;

fn digit_byte(digit: u128) -> Option<u8> {
    match digit {
        0 => Some(b'0'),
        1 => Some(b'1'),
        2 => Some(b'2'),
        3 => Some(b'3'),
        4 => Some(b'4'),
        5 => Some(b'5'),
        6 => Some(b'6'),
        7 => Some(b'7'),
        8 => Some(b'8'),
        9 => Some(b'9'),
        10 => Some(b'a'),
        11 => Some(b'b'),
        12 => Some(b'c'),
        13 => Some(b'd'),
        14 => Some(b'e'),
        15 => Some(b'f'),
        16 => Some(b'g'),
        17 => Some(b'h'),
        18 => Some(b'j'),
        19 => Some(b'k'),
        20 => Some(b'm'),
        21 => Some(b'n'),
        22 => Some(b'p'),
        23 => Some(b'q'),
        24 => Some(b'r'),
        25 => Some(b's'),
        26 => Some(b't'),
        27 => Some(b'v'),
        28 => Some(b'w'),
        29 => Some(b'x'),
        30 => Some(b'y'),
        31 => Some(b'z'),
        _ => None,
    }
}

fn encode(value: u128) -> Result<[u8; ENCODED_LEN], IdentifierError> {
    let mut remaining = value;
    let mut encoded = [b'0'; ENCODED_LEN];
    for byte in encoded.iter_mut().rev() {
        *byte = digit_byte(remaining & 31).ok_or(IdentifierError)?;
        remaining >>= 5;
    }
    Ok(encoded)
}

fn decode(value: &str) -> Result<u128, IdentifierError> {
    if value.len() != ENCODED_LEN {
        return Err(IdentifierError);
    }

    value.bytes().try_fold(0_u128, |decoded, byte| {
        let digit = match byte.to_ascii_lowercase() {
            b'0'..=b'9' => byte - b'0',
            b'a' => 10,
            b'b' => 11,
            b'c' => 12,
            b'd' => 13,
            b'e' => 14,
            b'f' => 15,
            b'g' => 16,
            b'h' => 17,
            b'j' => 18,
            b'k' => 19,
            b'm' => 20,
            b'n' => 21,
            b'p' => 22,
            b'q' => 23,
            b'r' => 24,
            b's' => 25,
            b't' => 26,
            b'v' => 27,
            b'w' => 28,
            b'x' => 29,
            b'y' => 30,
            b'z' => 31,
            _ => return Err(IdentifierError),
        };

        decoded
            .checked_mul(32)
            .and_then(|current| current.checked_add(u128::from(digit)))
            .ok_or(IdentifierError)
    })
}

macro_rules! identifier {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub(crate) struct $name(Uuid);

        impl $name {
            pub(crate) fn new() -> Self {
                Self(Uuid::now_v7())
            }

            #[cfg(test)]
            pub(crate) const fn from_u128(value: u128) -> Self {
                Self(Uuid::from_u128(value))
            }

            pub(crate) const fn as_uuid(self) -> Uuid {
                self.0
            }

            #[cfg(test)]
            pub(crate) fn version(self) -> usize {
                self.0.get_version_num()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                let encoded = encode(self.0.as_u128()).map_err(|_| fmt::Error)?;
                let text = std::str::from_utf8(&encoded).map_err(|_| fmt::Error)?;
                formatter.write_str(text)
            }
        }

        impl FromStr for $name {
            type Err = IdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                decode(value).map(|decoded| Self(Uuid::from_u128(decoded)))
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

identifier!(JobId);
identifier!(RunId);

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{JobId, RunId};

    #[test]
    fn zero_uuid_has_canonical_crockford_form() {
        let id = JobId::from_u128(0);
        assert_eq!(id.to_string(), "00000000000000000000000000");
        assert_eq!(id.to_string().parse::<JobId>(), Ok(id));
    }

    #[test]
    fn maximum_uuid_uses_only_128_bits() {
        let id = RunId::from_u128(u128::MAX);
        assert_eq!(id.to_string(), "7zzzzzzzzzzzzzzzzzzzzzzzzz");
        assert_eq!(id.to_string().parse::<RunId>(), Ok(id));
        assert!("8zzzzzzzzzzzzzzzzzzzzzzzzz".parse::<RunId>().is_err());
    }

    #[test]
    fn generated_ids_are_v7_and_unique_in_a_smoke_sample() {
        let first = JobId::new();
        let second = JobId::new();

        assert_eq!(first.version(), 7);
        assert_eq!(second.version(), 7);
        assert_ne!(first, second);
    }

    #[test]
    fn encoded_order_matches_uuid_order() {
        let first = JobId::from_u128(1);
        let second = JobId::from_u128(2);
        assert!(first < second);
        assert!(first.to_string() < second.to_string());
    }

    #[test]
    fn ids_round_trip_through_json_as_strings() {
        let id = JobId::new();
        let encoded = serde_json::to_string(&id).expect("ID should serialize");
        let decoded: JobId = serde_json::from_str(&encoded).expect("ID should deserialize");
        assert_eq!(decoded, id);
    }
}
