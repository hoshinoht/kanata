//! Enums that appear on the wire as plain strings.

use serde::{Deserialize, Serialize};

/// A route operation: `chat`, `transcription`, `embeddings` or `speech`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Chat,
    Transcription,
    Embeddings,
    Speech,
}

impl Operation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Transcription => "transcription",
            Self::Embeddings => "embeddings",
            Self::Speech => "speech",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "chat" => Some(Self::Chat),
            "transcription" => Some(Self::Transcription),
            "embeddings" => Some(Self::Embeddings),
            "speech" => Some(Self::Speech),
            _ => None,
        }
    }
}

/// A `reasoning_effort` level.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

/// Where a route's backend runs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustZone {
    Local,
    PrivateNetwork,
    External,
}

/// Speech output container.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeechFormat {
    #[default]
    Mp3,
    Wav,
}

impl SpeechFormat {
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Mp3 => "audio/mpeg",
            Self::Wav => "audio/wav",
        }
    }
}
