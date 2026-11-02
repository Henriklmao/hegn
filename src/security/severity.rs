//! Severity levels and their mapping from `tirith-core` verdicts.

use serde::Serialize;
use tirith_core::verdict::Severity as TirithSeverity;

/// Coarse severity used by the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Low => "LOW",
            Self::Medium => "MEDIUM",
            Self::High => "HIGH",
            Self::Critical => "CRITICAL",
        }
    }
}

impl From<TirithSeverity> for Severity {
    fn from(value: TirithSeverity) -> Self {
        match value {
            TirithSeverity::Info => Self::Info,
            TirithSeverity::Low => Self::Low,
            TirithSeverity::Medium => Self::Medium,
            TirithSeverity::High => Self::High,
            TirithSeverity::Critical => Self::Critical,
        }
    }
}
