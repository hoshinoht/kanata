//! Request and response bounds enforced by the gateway.

/// Upper bound for `prompt_cache_key`; harnesses send 36-byte UUIDs.
pub const MAX_PROMPT_CACHE_KEY_BYTES: usize = 64;

pub const MAX_RESPONSE_SCHEMA_BYTES: usize = 64 * 1024;
pub const MAX_RESPONSE_SCHEMA_DEPTH: usize = 32;
pub const MAX_RESPONSE_SCHEMA_NAME_BYTES: usize = 64;
pub const MAX_RESPONSE_SCHEMA_DESCRIPTION_BYTES: usize = 1024;
pub const MAX_OUTPUT_TOKENS: u32 = 1_048_576;

/// Stop sequences per request, the common chat API limit.
pub const MAX_STOP_SEQUENCES: usize = 4;
/// Bytes per stop sequence.
pub const MAX_STOP_SEQUENCE_BYTES: usize = 256;

/// `temperature` range, inclusive.
pub const MIN_TEMPERATURE: f64 = 0.0;
pub const MAX_TEMPERATURE: f64 = 2.0;
/// `top_p` range: greater than `MIN_TOP_P_EXCLUSIVE`, at most `MAX_TOP_P`.
pub const MIN_TOP_P_EXCLUSIVE: f64 = 0.0;
pub const MAX_TOP_P: f64 = 1.0;
/// `presence_penalty` and `frequency_penalty` range, inclusive.
pub const MIN_PENALTY: f64 = -2.0;
pub const MAX_PENALTY: f64 = 2.0;

/// Reasoning bytes forwarded per response; the rest is dropped.
pub const MAX_REASONING_BYTES: usize = 256 * 1024;
/// Bytes per reasoning item id; upstream ids are short (`rs_…`).
pub const MAX_REASONING_ITEM_ID_BYTES: usize = 256;
/// Bytes per encrypted reasoning payload.
pub const MAX_ENCRYPTED_REASONING_BYTES: usize = 1024 * 1024;

pub const MAX_EXTENSION_ENTRIES: usize = 16;
pub const MAX_EXTENSION_BYTES: usize = 8 * 1024;
pub const MAX_EXTENSION_DEPTH: usize = 4;
pub const MAX_INPUT_AUDIO_BYTES: usize = 25 * 1024 * 1024;

pub const MAX_EMBEDDING_INPUTS: usize = 128;
pub const MAX_EMBEDDING_DIMENSIONS: usize = 16_384;
pub const MAX_EMBEDDING_VALUES: usize = 262_144;

pub const MAX_INPUT_IMAGES: usize = 4;
pub const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_IMAGE_TOTAL_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_IMAGE_DIMENSION: u32 = 4096;
pub const MAX_IMAGE_TOTAL_PIXELS: u64 = 16_777_216;
pub const MAX_IMAGE_BASE64_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;

pub const MAX_SPEECH_CHARACTERS: usize = 4096;
pub const MAX_SPEECH_INPUT_BYTES: usize = 16_384;
pub const MAX_SPEECH_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// `speed` range, inclusive.
pub const MIN_SPEECH_SPEED: f64 = 0.25;
pub const MAX_SPEECH_SPEED: f64 = 4.0;

/// Bytes of output text and arguments in one Responses reply.
pub const MAX_RESPONSES_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
