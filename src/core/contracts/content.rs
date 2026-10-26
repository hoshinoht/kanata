use super::*;

/// Bytes per reasoning item id; upstream ids are short (`rs_…`).
pub const MAX_REASONING_ITEM_ID_BYTES: usize = 256;
/// Bytes per encrypted reasoning payload.
pub const MAX_ENCRYPTED_REASONING_BYTES: usize = 1024 * 1024;

/// Encrypted reasoning carried between turns without inspection.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ReasoningItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    encrypted_content: String,
}

// The payload is large and opaque; logs get its size only.
impl fmt::Debug for ReasoningItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReasoningItem")
            .field("id", &self.id)
            .field("encrypted_content_len", &self.encrypted_content.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasoningItemError {
    IdTooLong,
    EmptyContent,
    ContentTooLarge,
}

impl fmt::Display for ReasoningItemError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::IdTooLong => "reasoning item id exceeds the contract limit",
            Self::EmptyContent => "encrypted reasoning content is empty",
            Self::ContentTooLarge => "encrypted reasoning content exceeds the contract limit",
        })
    }
}

impl std::error::Error for ReasoningItemError {}

impl ReasoningItem {
    pub fn new(id: Option<String>, encrypted_content: String) -> Result<Self, ReasoningItemError> {
        if id
            .as_ref()
            .is_some_and(|id| id.len() > MAX_REASONING_ITEM_ID_BYTES)
        {
            return Err(ReasoningItemError::IdTooLong);
        }
        if encrypted_content.is_empty() {
            return Err(ReasoningItemError::EmptyContent);
        }
        if encrypted_content.len() > MAX_ENCRYPTED_REASONING_BYTES {
            return Err(ReasoningItemError::ContentTooLarge);
        }
        Ok(Self {
            id,
            encrypted_content,
        })
    }

    getters! {
        pub id: Option<&str> = as_deref,
        pub encrypted_content: &str,
    }
}

impl<'de> Deserialize<'de> for ReasoningItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawReasoningItem {
            #[serde(default)]
            id: Option<String>,
            encrypted_content: String,
        }

        let raw = RawReasoningItem::deserialize(deserializer)?;
        Self::new(raw.id, raw.encrypted_content).map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputAudioFormat {
    Wav,
    Mp3,
}

#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatedAudio {
    format: InputAudioFormat,
    bytes: Vec<u8>,
}

impl fmt::Debug for ValidatedAudio {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedAudio")
            .field("format", &self.format)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioValidationError {
    EmptyBytes,
    TooLarge,
}

impl fmt::Display for AudioValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyBytes => "audio bytes are empty",
            Self::TooLarge => "audio bytes exceed the contract limit",
        })
    }
}

impl std::error::Error for AudioValidationError {}

impl ValidatedAudio {
    pub fn new(format: InputAudioFormat, bytes: Vec<u8>) -> Result<Self, AudioValidationError> {
        Self::with_max_bytes(format, bytes, MAX_INPUT_AUDIO_BYTES)
    }

    pub fn with_max_bytes(
        format: InputAudioFormat,
        bytes: Vec<u8>,
        max_bytes: usize,
    ) -> Result<Self, AudioValidationError> {
        if bytes.is_empty() {
            return Err(AudioValidationError::EmptyBytes);
        }
        if bytes.len() > max_bytes.min(MAX_INPUT_AUDIO_BYTES) {
            return Err(AudioValidationError::TooLarge);
        }
        Ok(Self { format, bytes })
    }

    getters! {
        pub format: InputAudioFormat,
        pub bytes: &[u8],
    }
}

impl<'de> Deserialize<'de> for ValidatedAudio {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawAudio {
            format: InputAudioFormat,
            bytes: Vec<u8>,
        }

        let raw = RawAudio::deserialize(deserializer)?;
        Self::new(raw.format, raw.bytes).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatedFile {
    file_name: String,
    media_type: String,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileValidationError {
    EmptyName,
    InvalidMediaType,
    EmptyBytes,
}

impl fmt::Display for FileValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyName => "file name is empty",
            Self::InvalidMediaType => "file media type is invalid",
            Self::EmptyBytes => "file bytes are empty",
        })
    }
}

impl std::error::Error for FileValidationError {}

impl ValidatedFile {
    pub fn new(
        file_name: impl Into<String>,
        media_type: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Result<Self, FileValidationError> {
        let file_name = file_name.into();
        let media_type = media_type.into();
        if file_name.trim().is_empty() {
            return Err(FileValidationError::EmptyName);
        }
        if !valid_media_type(&media_type) {
            return Err(FileValidationError::InvalidMediaType);
        }
        if bytes.is_empty() {
            return Err(FileValidationError::EmptyBytes);
        }
        Ok(Self {
            file_name,
            media_type,
            bytes,
        })
    }

    getters! {
        pub file_name: &str,
        pub media_type: &str,
        pub bytes: &[u8],
    }
}

impl<'de> Deserialize<'de> for ValidatedFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawFile {
            file_name: String,
            media_type: String,
            bytes: Vec<u8>,
        }

        let raw = RawFile::deserialize(deserializer)?;
        Self::new(raw.file_name, raw.media_type, raw.bytes).map_err(D::Error::custom)
    }
}

fn valid_media_type(value: &str) -> bool {
    let mut parts = value.split('/');
    let Some(type_name) = parts.next() else {
        return false;
    };
    let Some(subtype) = parts.next() else {
        return false;
    };
    parts.next().is_none() && valid_media_token(type_name) && valid_media_token(subtype)
}

fn valid_media_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'))
}
