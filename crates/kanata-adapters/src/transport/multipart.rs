use bytes::Bytes;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::core::GatewayError;

use super::{encoded::EncodedBody, types};

const MAX_FIELDS: usize = 64;
const MAX_NAME_BYTES: usize = 128;
const MAX_FILENAME_BYTES: usize = 255;
const MAX_MEDIA_TYPE_BYTES: usize = 128;
const MAX_BOUNDARY_ATTEMPTS: usize = 64;
const BOUNDARY_PREFIX: &str = "kanata-boundary-";
const BOUNDARY_LENGTH: usize = BOUNDARY_PREFIX.len() + 16;
const FIELD_HEADER_PREFIX: &str = "Content-Disposition: form-data; name=\"";
const FILE_HEADER_MIDDLE: &str = "\"; filename=\"";
const FILE_HEADER_SUFFIX: &str = "\"\r\nContent-Type: ";
const FIELD_HEADER_END: &str = "\"\r\n\r\n";
const FILE_HEADER_END: &str = "\r\n\r\n";

pub(crate) struct MultipartFile {
    name: String,
    filename: String,
    media_type: String,
    bytes: Bytes,
}

pub(crate) struct MultipartRequest {
    fields: Vec<(String, String)>,
    file: MultipartFile,
}

impl MultipartFile {
    pub(crate) fn new(
        name: String,
        filename: String,
        media_type: String,
        bytes: Bytes,
    ) -> Result<Self, GatewayError> {
        if bytes.is_empty()
            || !valid_disposition(&name, MAX_NAME_BYTES)
            || !valid_disposition(&filename, MAX_FILENAME_BYTES)
            || !valid_media_type(&media_type, MAX_MEDIA_TYPE_BYTES)
        {
            return Err(types::internal_error());
        }
        Ok(Self {
            name,
            filename,
            media_type,
            bytes,
        })
    }
}

impl MultipartRequest {
    pub(crate) fn new(
        fields: Vec<(String, String)>,
        file: MultipartFile,
    ) -> Result<Self, GatewayError> {
        if fields
            .len()
            .checked_add(1)
            .is_none_or(|count| count > MAX_FIELDS)
            || fields
                .iter()
                .any(|(name, _)| !valid_disposition(name, MAX_NAME_BYTES))
        {
            return Err(types::internal_error());
        }
        Ok(Self { fields, file })
    }
}

pub(super) fn encode(
    request: MultipartRequest,
    budget: usize,
) -> Result<EncodedBody, GatewayError> {
    encode_inner(request, budget, None)
}

#[cfg(test)]
pub(super) fn encode_with_scan_counter(
    request: MultipartRequest,
    budget: usize,
    scans: &AtomicUsize,
) -> Result<EncodedBody, GatewayError> {
    encode_inner(request, budget, Some(scans))
}

fn encode_inner(
    request: MultipartRequest,
    budget: usize,
    scans: Option<&AtomicUsize>,
) -> Result<EncodedBody, GatewayError> {
    if budget == 0 {
        return Err(types::internal_error());
    }
    let expected_length = preflight_length(&request, budget)?;
    let boundary = choose_boundary(&request, scans)?;
    let MultipartRequest { fields, file } = request;
    let mut chunks = Vec::new();
    let mut total = 0usize;
    for (name, value) in fields {
        push(
            &mut chunks,
            &mut total,
            budget,
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n")
                .into_bytes(),
        )?;
        push(&mut chunks, &mut total, budget, value.into_bytes())?;
        push(&mut chunks, &mut total, budget, b"\r\n".to_vec())?;
    }
    push(
        &mut chunks,
        &mut total,
        budget,
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
            file.name, file.filename, file.media_type
        )
        .into_bytes(),
    )?;
    push_bytes(&mut chunks, &mut total, budget, file.bytes)?;
    push(&mut chunks, &mut total, budget, b"\r\n".to_vec())?;
    push(
        &mut chunks,
        &mut total,
        budget,
        format!("--{boundary}--\r\n").into_bytes(),
    )?;
    if total != expected_length {
        return Err(types::internal_error());
    }
    EncodedBody::from_multipart_chunks(
        format!("multipart/form-data; boundary={boundary}"),
        chunks,
        total,
        budget,
    )
}

fn choose_boundary(
    request: &MultipartRequest,
    scans: Option<&AtomicUsize>,
) -> Result<String, GatewayError> {
    for index in 0..MAX_BOUNDARY_ATTEMPTS {
        if let Some(scans) = scans {
            scans.fetch_add(1, Ordering::Relaxed);
        }
        let candidate = format!("{BOUNDARY_PREFIX}{index:016x}");
        let collision = request.fields.iter().any(|(_, value)| {
            value
                .as_bytes()
                .windows(candidate.len())
                .any(|window| window == candidate.as_bytes())
        }) || request
            .file
            .bytes
            .windows(candidate.len())
            .any(|window| window == candidate.as_bytes());
        if !collision {
            return Ok(candidate);
        }
    }
    Err(types::internal_error())
}

fn preflight_length(request: &MultipartRequest, budget: usize) -> Result<usize, GatewayError> {
    let mut total = 0usize;
    for (name, value) in &request.fields {
        add_len(&mut total, 2)?;
        add_len(&mut total, BOUNDARY_LENGTH)?;
        add_len(&mut total, 2)?;
        add_len(&mut total, FIELD_HEADER_PREFIX.len())?;
        add_len(&mut total, name.len())?;
        add_len(&mut total, FIELD_HEADER_END.len())?;
        add_len(&mut total, value.len())?;
        add_len(&mut total, 2)?;
    }
    add_len(&mut total, 2)?;
    add_len(&mut total, BOUNDARY_LENGTH)?;
    add_len(&mut total, 2)?;
    add_len(&mut total, FIELD_HEADER_PREFIX.len())?;
    add_len(&mut total, request.file.name.len())?;
    add_len(&mut total, FILE_HEADER_MIDDLE.len())?;
    add_len(&mut total, request.file.filename.len())?;
    add_len(&mut total, FILE_HEADER_SUFFIX.len())?;
    add_len(&mut total, request.file.media_type.len())?;
    add_len(&mut total, FILE_HEADER_END.len())?;
    add_len(&mut total, request.file.bytes.len())?;
    add_len(&mut total, 2)?;
    add_len(&mut total, 2)?;
    add_len(&mut total, BOUNDARY_LENGTH)?;
    add_len(&mut total, 4)?;
    if total > budget {
        return Err(types::internal_error());
    }
    Ok(total)
}

fn add_len(total: &mut usize, length: usize) -> Result<(), GatewayError> {
    *total = (*total)
        .checked_add(length)
        .ok_or_else(types::internal_error)?;
    Ok(())
}

fn push(
    chunks: &mut Vec<Bytes>,
    total: &mut usize,
    budget: usize,
    bytes: Vec<u8>,
) -> Result<(), GatewayError> {
    push_bytes(chunks, total, budget, Bytes::from(bytes))
}

fn push_bytes(
    chunks: &mut Vec<Bytes>,
    total: &mut usize,
    budget: usize,
    bytes: Bytes,
) -> Result<(), GatewayError> {
    *total = (*total)
        .checked_add(bytes.len())
        .ok_or_else(types::internal_error)?;
    if *total > budget {
        return Err(types::internal_error());
    }
    chunks.push(bytes);
    Ok(())
}

fn valid_disposition(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.bytes().all(|byte| {
            byte >= 0x20
                && byte != 0x7f
                && byte != b'"'
                && byte != b'\\'
                && byte != b'\r'
                && byte != b'\n'
        })
}

fn valid_media_type(value: &str, max_bytes: usize) -> bool {
    let Some((kind, subtype)) = value.split_once('/') else {
        return false;
    };
    value.len() <= max_bytes
        && !kind.is_empty()
        && !subtype.is_empty()
        && kind.bytes().all(is_media_token)
        && subtype.bytes().all(is_media_token)
}

fn is_media_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
        )
}
