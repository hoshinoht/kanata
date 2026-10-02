use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use std::collections::BTreeSet;

use super::ModelAlias;

pub const MAX_SPEECH_CHARACTERS: usize = 4096;
pub const MAX_SPEECH_INPUT_BYTES: usize = 16_384;
pub const MAX_SPEECH_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

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

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SpeechSpeed(f64);

impl Eq for SpeechSpeed {}
impl Default for SpeechSpeed {
    fn default() -> Self {
        Self(1.0)
    }
}
impl SpeechSpeed {
    pub fn new(value: f64) -> Option<Self> {
        (value.is_finite() && (0.25..=4.0).contains(&value)).then_some(Self(value))
    }
    pub fn get(self) -> f64 {
        self.0
    }
}
impl<'de> Deserialize<'de> for SpeechSpeed {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(f64::deserialize(deserializer)?)
            .ok_or_else(|| D::Error::custom("invalid speech speed"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SpeechPolicy {
    pub voices: BTreeSet<String>,
    pub formats: BTreeSet<SpeechFormat>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechRequest {
    pub model: ModelAlias,
    pub input: String,
    pub voice: String,
    #[serde(default)]
    pub response_format: SpeechFormat,
    #[serde(default)]
    pub speed: SpeechSpeed,
}

impl SpeechRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.model.0.trim().is_empty() {
            return Err("model");
        }
        if self.input.trim().is_empty()
            || self.input.len() > MAX_SPEECH_INPUT_BYTES
            || self.input.chars().count() > MAX_SPEECH_CHARACTERS
        {
            return Err("input");
        }
        let controls = self.input.to_ascii_lowercase();
        if ["[pause:", "[voice:", "[rate:"]
            .iter()
            .any(|tag| controls.contains(tag))
        {
            return Err("input");
        }
        if !valid_speech_voice(&self.voice) {
            return Err("voice");
        }
        Ok(())
    }
    pub fn check_policy(&self, policy: &SpeechPolicy) -> Result<(), &'static str> {
        if !policy.voices.contains(&self.voice) {
            return Err("voice");
        }
        if !policy.formats.contains(&self.response_format) {
            return Err("response_format");
        }
        Ok(())
    }
}

pub fn valid_speech_voice(voice: &str) -> bool {
    !voice.is_empty()
        && voice.len() <= 64
        && voice
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpeechResponse {
    pub model: ModelAlias,
    pub format: SpeechFormat,
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for SpeechResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeechResponse")
            .field("model", &self.model)
            .field("format", &self.format)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl SpeechResponse {
    pub fn valid_for(&self, request: &SpeechRequest) -> bool {
        self.model == request.model
            && self.format == request.response_format
            && valid_speech_audio(&self.bytes, self.format)
    }
}

pub fn valid_speech_audio(bytes: &[u8], format: SpeechFormat) -> bool {
    if bytes.is_empty() || bytes.len() > MAX_SPEECH_RESPONSE_BYTES {
        return false;
    }
    match format {
        SpeechFormat::Wav => valid_wav(bytes),
        SpeechFormat::Mp3 => {
            let mut at = 0;
            if bytes.starts_with(b"ID3") {
                if bytes.len() < 10 || bytes[6..10].iter().any(|byte| byte & 0x80 != 0) {
                    return false;
                }
                let size = bytes[6..10]
                    .iter()
                    .fold(0usize, |size, byte| (size << 7) | usize::from(*byte));
                at = 10 + size;
                if bytes[3] == 4 && bytes[5] & 0x10 != 0 {
                    at += 10;
                }
            }
            bytes.get(at..at + 4).is_some_and(|frame| {
                frame[0] == 0xff
                    && frame[1] & 0xe0 == 0xe0
                    && frame[1] & 0x18 != 0x08
                    && frame[1] & 0x06 != 0
                    && frame[2] & 0xf0 != 0
                    && frame[2] & 0xf0 != 0xf0
                    && frame[2] & 0x0c != 0x0c
            })
        }
    }
}

fn valid_wav(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return false;
    }
    let size = u32::from_le_bytes(bytes[4..8].try_into().expect("bounded header")) as usize;
    if size.checked_add(8) != Some(bytes.len()) {
        return false;
    }
    let mut at = 12usize;
    let mut format = false;
    let mut data = false;
    while at < bytes.len() {
        let Some(header) = bytes.get(at..at + 8) else {
            return false;
        };
        let length = u32::from_le_bytes(header[4..8].try_into().expect("bounded chunk")) as usize;
        let Some(end) = at.checked_add(8).and_then(|at| at.checked_add(length)) else {
            return false;
        };
        let Some(chunk) = bytes.get(at + 8..end) else {
            return false;
        };
        if &header[..4] == b"fmt " {
            format = chunk.len() >= 16;
        }
        if &header[..4] == b"data" {
            data |= !chunk.is_empty();
        }
        at = end + (length % 2);
    }
    at == bytes.len() && format && data
}
