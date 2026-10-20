#![allow(dead_code)]
//! HTTP/1.1 request framing for loopback upstream mocks.

use std::{
    collections::BTreeMap,
    io::{Error, ErrorKind, Result},
};

use tokio::io::{AsyncRead, AsyncReadExt};

#[derive(Clone, Debug)]
pub struct RequestRecord {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// Reads one `content-length` framed request; header names are lowercased.
pub async fn read_request<S: AsyncRead + Unpin>(socket: &mut S) -> Result<RequestRecord> {
    let invalid = |what: &str| Error::new(ErrorKind::InvalidData, what.to_owned());
    let mut bytes = Vec::new();
    let head_end = loop {
        let mut chunk = [0_u8; 1024];
        let count = socket.read(&mut chunk).await?;
        if count == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "headers"));
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() > 64 * 1024 {
            return Err(invalid("headers"));
        }
    };
    let head = std::str::from_utf8(&bytes[..head_end - 4]).map_err(|_| invalid("headers"))?;
    let mut lines = head.split("\r\n");
    let mut request_line = lines
        .next()
        .ok_or_else(|| invalid("request line"))?
        .split_whitespace();
    let method = request_line
        .next()
        .ok_or_else(|| invalid("method"))?
        .to_owned();
    let path = request_line
        .next()
        .ok_or_else(|| invalid("path"))?
        .to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or_else(|| invalid("header"))?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers
        .get("content-length")
        .ok_or_else(|| invalid("length"))?
        .parse::<usize>()
        .map_err(|_| invalid("length"))?;
    let mut body = bytes.split_off(head_end);
    if body.len() > length {
        return Err(invalid("body"));
    }
    let already_read = body.len();
    body.resize(length, 0);
    if already_read < length {
        socket.read_exact(&mut body[already_read..]).await?;
    }
    Ok(RequestRecord {
        method,
        path,
        headers,
        body,
    })
}

/// Reads until the peer closes `socket` or the read fails.
pub async fn wait_for_eof<S: AsyncRead + Unpin>(socket: &mut S) {
    let mut buffer = [0_u8; 1024];
    while socket.read(&mut buffer).await.unwrap_or(0) != 0 {}
}
