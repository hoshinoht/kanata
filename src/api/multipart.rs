use axum::{extract::Request, http::header};

use crate::core::{Extensions, ValidatedFile};

const MAX_TEXT_FIELD_BYTES: usize = 8 * 1024;
const MAX_TEXT_FIELD_COUNT: usize = 5;
const MAX_MULTIPART_METADATA_BYTES: usize = 64 * 1024;

pub(super) struct MultipartWire {
    pub(super) model: String,
    pub(super) file: ValidatedFile,
    pub(super) language: Option<String>,
    pub(super) prompt: Option<String>,
    pub(super) response_format: Option<String>,
    pub(super) extensions: Extensions,
}

#[derive(Clone, Copy)]
pub(super) enum MultipartInputError {
    Invalid,
    TooLarge,
}

pub(super) async fn parse_multipart(
    request: Request,
    max_audio: usize,
) -> Result<MultipartWire, MultipartInputError> {
    let max_text_bytes = MAX_TEXT_FIELD_BYTES
        .checked_mul(MAX_TEXT_FIELD_COUNT)
        .ok_or(MultipartInputError::TooLarge)?;
    let max_envelope_bytes = max_audio
        .checked_add(max_text_bytes)
        .and_then(|bytes| bytes.checked_add(MAX_MULTIPART_METADATA_BYTES))
        .ok_or(MultipartInputError::TooLarge)?;
    let max_envelope =
        u64::try_from(max_envelope_bytes).map_err(|_| MultipartInputError::TooLarge)?;
    let max_audio_limit = u64::try_from(max_audio).map_err(|_| MultipartInputError::TooLarge)?;
    let mut content_types = request.headers().get_all(header::CONTENT_TYPE).iter();
    let Some(content_type) = content_types.next() else {
        return Err(MultipartInputError::Invalid);
    };
    if content_types.next().is_some() {
        return Err(MultipartInputError::Invalid);
    }
    let content_type = content_type
        .to_str()
        .map_err(|_| MultipartInputError::Invalid)?;
    let boundary =
        multer::parse_boundary(content_type).map_err(|_| MultipartInputError::Invalid)?;
    let limits = multer::SizeLimit::new()
        .whole_stream(max_envelope)
        .per_field(MAX_TEXT_FIELD_BYTES as u64)
        .for_field("file", max_audio_limit);
    let constraints = multer::Constraints::new()
        .size_limit(limits)
        .allowed_fields(vec![
            "model",
            "file",
            "language",
            "prompt",
            "response_format",
            "extensions",
        ]);
    let mut multipart = multer::Multipart::with_constraints(
        request.into_body().into_data_stream(),
        boundary,
        constraints,
    );
    let mut model = None;
    let mut file = None;
    let mut language = None;
    let mut prompt = None;
    let mut response_format = None;
    let mut extensions = None;
    while let Some(mut field) = multipart.next_field().await.map_err(multipart_error)? {
        let name = field.name().ok_or(MultipartInputError::Invalid)?.to_owned();
        let file_name = field.file_name().map(str::to_owned);
        let media_type = field.content_type().map(ToString::to_string);
        if name != "file" && file_name.is_some() {
            return Err(MultipartInputError::Invalid);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
            let limit = if name == "file" {
                max_audio
            } else {
                MAX_TEXT_FIELD_BYTES
            };
            if bytes
                .len()
                .checked_add(chunk.len())
                .is_none_or(|length| length > limit)
            {
                return Err(MultipartInputError::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        match name.as_str() {
            "file" if file.is_none() => {
                let filename = file_name.ok_or(MultipartInputError::Invalid)?;
                let media_type = media_type.ok_or(MultipartInputError::Invalid)?;
                if !safe_filename(&filename) || !accepted_audio_type(&media_type) {
                    return Err(MultipartInputError::Invalid);
                }
                file = Some(
                    ValidatedFile::new(filename, media_type, bytes)
                        .map_err(|_| MultipartInputError::Invalid)?,
                );
            }
            "model" if model.is_none() => model = Some(text_field(bytes)?),
            "language" if language.is_none() => language = Some(text_field(bytes)?),
            "prompt" if prompt.is_none() => prompt = Some(text_field(bytes)?),
            "response_format" if response_format.is_none() => {
                response_format = Some(text_field(bytes)?)
            }
            "extensions" if extensions.is_none() => {
                extensions = Some(
                    serde_json::from_str(&text_field(bytes)?)
                        .map_err(|_| MultipartInputError::Invalid)?,
                )
            }
            _ => return Err(MultipartInputError::Invalid),
        }
    }
    let model = model
        .filter(|value: &String| valid_metadata(value))
        .ok_or(MultipartInputError::Invalid)?;
    if language
        .as_deref()
        .is_some_and(|value| !valid_metadata(value))
        || prompt
            .as_deref()
            .is_some_and(|value| !valid_metadata(value))
    {
        return Err(MultipartInputError::Invalid);
    }
    if !matches!(
        response_format.as_deref(),
        None | Some("json") | Some("text")
    ) {
        return Err(MultipartInputError::Invalid);
    }
    Ok(MultipartWire {
        model,
        file: file.ok_or(MultipartInputError::Invalid)?,
        language,
        prompt,
        response_format,
        extensions: extensions.unwrap_or_default(),
    })
}

fn multipart_error(error: multer::Error) -> MultipartInputError {
    match error {
        multer::Error::FieldSizeExceeded { .. } | multer::Error::StreamSizeExceeded { .. } => {
            MultipartInputError::TooLarge
        }
        _ => MultipartInputError::Invalid,
    }
}

fn text_field(bytes: Vec<u8>) -> Result<String, MultipartInputError> {
    String::from_utf8(bytes).map_err(|_| MultipartInputError::Invalid)
}

fn valid_metadata(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TEXT_FIELD_BYTES
        && !value.bytes().any(|byte| byte.is_ascii_control())
}

fn safe_filename(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| !byte.is_ascii_control() && !matches!(byte, b'/' | b'\\' | b':'))
}

fn accepted_audio_type(value: &str) -> bool {
    matches!(
        value,
        "audio/flac"
            | "audio/mpeg"
            | "audio/mp4"
            | "audio/ogg"
            | "audio/wav"
            | "audio/webm"
            | "audio/x-wav"
    )
}
