use std::{
    convert::Infallible,
    io::{self, Write},
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use serde::Serialize;

use crate::core::GatewayError;

use super::{
    multipart::MultipartRequest,
    types::{self, MAX_CHUNK_BYTES},
};

pub(crate) struct EncodedBody {
    content_type: String,
    chunks: Vec<Bytes>,
    index: usize,
    remaining: usize,
    sensitive: bool,
}

impl EncodedBody {
    pub(crate) fn json<T: Serialize>(value: &T, budget: usize) -> Result<Self, GatewayError> {
        Self::json_with_sensitivity(value, budget, false)
    }

    pub(crate) fn sensitive_json<T: Serialize>(
        value: &T,
        budget: usize,
    ) -> Result<Self, GatewayError> {
        Self::json_with_sensitivity(value, budget, true)
    }

    fn json_with_sensitivity<T: Serialize>(
        value: &T,
        budget: usize,
        sensitive: bool,
    ) -> Result<Self, GatewayError> {
        if budget == 0 {
            return Err(types::internal_error());
        }
        let mut writer = BoundedWriter::new(budget);
        serde_json::to_writer(&mut writer, value).map_err(|_| types::internal_error())?;
        Self::from_chunks(
            "application/json".to_owned(),
            vec![Bytes::from(writer.into_inner())],
            sensitive,
            budget,
        )
    }

    pub(crate) fn form(fields: &[(String, String)], budget: usize) -> Result<Self, GatewayError> {
        if budget == 0 {
            return Err(types::internal_error());
        }
        let mut writer = BoundedWriter::new(budget);
        for (index, (name, value)) in fields.iter().enumerate() {
            if index != 0 {
                writer
                    .write_all(b"&")
                    .map_err(|_| types::internal_error())?;
            }
            write_form_component(&mut writer, name)?;
            writer
                .write_all(b"=")
                .map_err(|_| types::internal_error())?;
            write_form_component(&mut writer, value)?;
        }
        Self::from_chunks(
            "application/x-www-form-urlencoded".to_owned(),
            vec![Bytes::from(writer.into_inner())],
            true,
            budget,
        )
    }

    pub(crate) fn form_encoded(value: &str, budget: usize) -> Result<Self, GatewayError> {
        if budget == 0 || !valid_form_encoded(value.as_bytes()) {
            return Err(types::internal_error());
        }
        Self::from_chunks(
            "application/x-www-form-urlencoded".to_owned(),
            vec![Bytes::copy_from_slice(value.as_bytes())],
            true,
            budget,
        )
    }

    pub(crate) fn multipart(
        request: MultipartRequest,
        budget: usize,
    ) -> Result<Self, GatewayError> {
        super::multipart::encode(request, budget)
    }

    pub(crate) fn content_type(&self) -> &str {
        &self.content_type
    }

    pub(crate) fn len(&self) -> usize {
        self.remaining
    }

    pub(crate) fn is_sensitive(&self) -> bool {
        self.sensitive
    }

    fn from_chunks(
        content_type: String,
        chunks: Vec<Bytes>,
        sensitive: bool,
        budget: usize,
    ) -> Result<Self, GatewayError> {
        let mut total = 0usize;
        for chunk in &chunks {
            total = total
                .checked_add(chunk.len())
                .ok_or_else(types::internal_error)?;
        }
        if total > budget {
            return Err(types::internal_error());
        }
        Ok(Self {
            content_type,
            chunks,
            index: 0,
            remaining: total,
            sensitive,
        })
    }

    pub(super) fn from_multipart_chunks(
        content_type: String,
        chunks: Vec<Bytes>,
        total: usize,
        budget: usize,
    ) -> Result<Self, GatewayError> {
        if total > budget {
            return Err(types::internal_error());
        }
        Ok(Self {
            content_type,
            chunks,
            index: 0,
            remaining: total,
            sensitive: false,
        })
    }
}

impl Body for EncodedBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.as_mut().get_mut();
        while this.index < this.chunks.len() {
            if this.chunks[this.index].is_empty() {
                this.index += 1;
                continue;
            }
            let take = this.chunks[this.index].len().min(MAX_CHUNK_BYTES);
            let data = this.chunks[this.index].split_to(take);
            this.remaining -= data.len();
            return Poll::Ready(Some(Ok(Frame::data(data))));
        }
        Poll::Ready(None)
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining as u64)
    }
}

struct BoundedWriter {
    bytes: Vec<u8>,
    budget: usize,
}

impl BoundedWriter {
    fn new(budget: usize) -> Self {
        Self {
            bytes: Vec::new(),
            budget,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next) = self.bytes.len().checked_add(bytes.len()) else {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "bounded body"));
        };
        if next > self.budget {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "bounded body"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn write_form_component(writer: &mut BoundedWriter, value: &str) -> Result<(), GatewayError> {
    for byte in value.bytes() {
        match byte {
            b' ' => writer
                .write_all(b"+")
                .map_err(|_| types::internal_error())?,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => writer
                .write_all(&[byte])
                .map_err(|_| types::internal_error())?,
            _ => {
                let hex = [
                    b"0123456789ABCDEF"[(byte >> 4) as usize],
                    b"0123456789ABCDEF"[(byte & 0x0f) as usize],
                ];
                writer
                    .write_all(&[b'%', hex[0], hex[1]])
                    .map_err(|_| types::internal_error())?;
            }
        }
    }
    Ok(())
}

fn valid_form_encoded(value: &[u8]) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut index = 0;
    while index < value.len() {
        match value[index] {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'*'
            | b'+'
            | b'&'
            | b'=' => index += 1,
            b'%' if index + 2 < value.len()
                && value[index + 1].is_ascii_hexdigit()
                && value[index + 2].is_ascii_hexdigit() =>
            {
                index += 3;
            }
            _ => return false,
        }
    }
    true
}
