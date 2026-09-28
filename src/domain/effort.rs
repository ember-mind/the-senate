//! Provider-neutral resource intent.
//!
//! Effort expresses HOW MUCH native-runtime effort is requested for a
//! responsibility. It is separate from Role (what responsibility), routing
//! (which runtime destination), and M13a usage telemetry (what was observed).
//! The Senate owns the requested level; each provider adapter owns how that
//! level maps onto its native runtime controls.

use serde::{Deserialize, Serialize};

/// Explicit requested effort level. Adapters translate these; domain code
/// never encodes provider- or model-specific aliases.
///
/// Ordered: `Low < Medium < High < XHigh`, so a raise is a comparison, not a
/// table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EffortLevel {
    Low,
    Medium,
    High,
    XHigh,
}

/// Requested native-runtime effort for one responsibility.
///
/// `NativeDefault` means: preserve the runtime's own configured default
/// behavior exactly, byte-identical to pre-effort-policy invocations. It is
/// deliberately distinct from `Level(Medium)`: a runtime's default may be
/// anything, so the two must never be conflated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum EffortSetting {
    #[default]
    NativeDefault,
    Level(EffortLevel),
}

impl EffortSetting {
    pub const LOW: Self = Self::Level(EffortLevel::Low);
    pub const MEDIUM: Self = Self::Level(EffortLevel::Medium);
    pub const HIGH: Self = Self::Level(EffortLevel::High);
    pub const XHIGH: Self = Self::Level(EffortLevel::XHigh);

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeDefault => "native_default",
            Self::Level(EffortLevel::Low) => "low",
            Self::Level(EffortLevel::Medium) => "medium",
            Self::Level(EffortLevel::High) => "high",
            Self::Level(EffortLevel::XHigh) => "xhigh",
        }
    }

    /// Human label for status surfaces.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NativeDefault => "native default",
            Self::Level(EffortLevel::Low) => "low",
            Self::Level(EffortLevel::Medium) => "medium",
            Self::Level(EffortLevel::High) => "high",
            Self::Level(EffortLevel::XHigh) => "xhigh",
        }
    }
}

/// Malformed or unknown effort encoding. Unknown future settings fail closed;
/// they never silently degrade to `NativeDefault` or `Medium`.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("unknown effort setting {0:?}; supported: native_default, low, medium, high, xhigh")]
pub struct EffortParseError(String);

impl std::str::FromStr for EffortSetting {
    type Err = EffortParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "native_default" => Ok(Self::NativeDefault),
            "low" => Ok(Self::LOW),
            "medium" => Ok(Self::MEDIUM),
            "high" => Ok(Self::HIGH),
            "xhigh" => Ok(Self::XHIGH),
            other => Err(EffortParseError(other.to_owned())),
        }
    }
}

impl std::fmt::Display for EffortSetting {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for EffortSetting {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EffortSetting {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_serialize_and_deserialize_distinctly() {
        for setting in [
            EffortSetting::NativeDefault,
            EffortSetting::LOW,
            EffortSetting::MEDIUM,
            EffortSetting::HIGH,
            EffortSetting::XHIGH,
        ] {
            let encoded = serde_json::to_string(&setting).unwrap();
            let decoded: EffortSetting = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, setting);
        }
        assert_eq!(
            serde_json::to_string(&EffortSetting::NativeDefault).unwrap(),
            "\"native_default\""
        );
        assert_eq!(
            serde_json::to_string(&EffortSetting::HIGH).unwrap(),
            "\"high\""
        );
        assert_eq!(
            serde_json::to_string(&EffortSetting::XHIGH).unwrap(),
            "\"xhigh\""
        );
    }

    /// A raise is a comparison on the level, so the declaration order is the
    /// contract.
    #[test]
    fn levels_are_ordered_low_to_xhigh() {
        assert!(EffortLevel::Low < EffortLevel::Medium);
        assert!(EffortLevel::Medium < EffortLevel::High);
        assert!(EffortLevel::High < EffortLevel::XHigh);
    }

    #[test]
    fn native_default_is_not_medium() {
        assert_ne!(EffortSetting::NativeDefault, EffortSetting::MEDIUM);
        assert_eq!(EffortSetting::default(), EffortSetting::NativeDefault);
    }

    #[test]
    fn unknown_effort_fails_closed() {
        let error = serde_json::from_str::<EffortSetting>("\"turbo\"");
        assert!(error.is_err());
        let error = serde_json::from_str::<EffortSetting>("\"native default\"");
        assert!(error.is_err());
        let error = serde_json::from_str::<EffortSetting>("\"Medium\"");
        assert!(error.is_err());
        // Claude Code also accepts `max`; Codex does not, and The Senate states
        // no level it cannot hand to every native runtime.
        let error = serde_json::from_str::<EffortSetting>("\"max\"");
        assert!(error.is_err());
    }
}
