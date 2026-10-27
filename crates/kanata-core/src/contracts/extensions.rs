use super::*;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExtensionKey(String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtensionKeyError {
    Empty,
    TooLong,
    NotNamespaced,
    InvalidSegment,
}

impl ExtensionKey {
    pub fn parse(value: impl Into<String>) -> Result<Self, ExtensionKeyError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ExtensionKeyError::Empty);
        }
        if value.len() > 128 {
            return Err(ExtensionKeyError::TooLong);
        }
        let segments: Vec<_> = value.split('.').collect();
        if segments.len() < 2 {
            return Err(ExtensionKeyError::NotNamespaced);
        }
        if segments
            .iter()
            .any(|segment| !valid_extension_segment(segment))
        {
            return Err(ExtensionKeyError::InvalidSegment);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn valid_extension_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!(chars.next(), Some(character) if character.is_ascii_lowercase())
        && chars.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

impl Serialize for ExtensionKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ExtensionKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

impl fmt::Display for ExtensionKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "extension key is empty",
            Self::TooLong => "extension key exceeds 128 bytes",
            Self::NotNamespaced => "extension key must contain a namespace",
            Self::InvalidSegment => "extension key contains an invalid namespace segment",
        })
    }
}

impl std::error::Error for ExtensionKeyError {}

pub use kanata_contract::limits::{
    MAX_EXTENSION_BYTES, MAX_EXTENSION_DEPTH, MAX_EXTENSION_ENTRIES, MAX_INPUT_AUDIO_BYTES,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtensionError {
    TooManyEntries,
    TooLarge,
    TooDeep,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Extensions(BTreeMap<ExtensionKey, Value>);

impl Extensions {
    pub fn try_from_map(entries: BTreeMap<ExtensionKey, Value>) -> Result<Self, ExtensionError> {
        validate_extensions(&entries)?;
        Ok(Self(entries))
    }

    pub fn insert(&mut self, key: ExtensionKey, value: Value) -> Result<(), ExtensionError> {
        let mut entries = self.0.clone();
        entries.insert(key, value);
        validate_extensions(&entries)?;
        self.0 = entries;
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ExtensionKey, &Value)> {
        self.0.iter()
    }
}

impl<'de> Deserialize<'de> for Extensions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let entries = BTreeMap::deserialize(deserializer)?;
        Self::try_from_map(entries).map_err(D::Error::custom)
    }
}

fn validate_extensions(entries: &BTreeMap<ExtensionKey, Value>) -> Result<(), ExtensionError> {
    if entries.len() > MAX_EXTENSION_ENTRIES {
        return Err(ExtensionError::TooManyEntries);
    }
    if entries
        .values()
        .any(|value| extension_depth(value) > MAX_EXTENSION_DEPTH)
    {
        return Err(ExtensionError::TooDeep);
    }
    if serde_json::to_vec(entries)
        .expect("JSON values serialize")
        .len()
        > MAX_EXTENSION_BYTES
    {
        return Err(ExtensionError::TooLarge);
    }
    Ok(())
}

pub(super) fn extension_depth(value: &Value) -> usize {
    match value {
        Value::Array(values) => 1 + values.iter().map(extension_depth).max().unwrap_or(0),
        Value::Object(values) => 1 + values.values().map(extension_depth).max().unwrap_or(0),
        _ => 0,
    }
}

impl fmt::Display for ExtensionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooManyEntries => "extension count exceeds the contract limit",
            Self::TooLarge => "extensions exceed the contract byte limit",
            Self::TooDeep => "extension nesting exceeds the contract depth limit",
        })
    }
}

impl std::error::Error for ExtensionError {}
