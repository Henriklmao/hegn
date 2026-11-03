//! HTTP/1.1 framing for the Cloud Hypervisor Unix socket API.
//!
//! Responses are framed by `Content-Length` (or chunked encoding) so a VMM
//! that keeps the connection open cannot make hegn wait for EOF. Reads are
//! bounded so a stuck or malicious socket peer cannot make hegn buffer
//! unbounded data in memory.

use std::time::Duration;

use anyhow::Result;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;

/// API responses are small; bound reads so a stuck or malicious socket peer
/// cannot make hegn buffer unbounded data in memory.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub const API_PREFIX: &str = "/api/v1";

#[derive(Debug, Error)]
pub enum Error {
    #[error("cloud-hypervisor io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cloud-hypervisor returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("invalid cloud-hypervisor response: {0}")]
    Protocol(String),
    #[error("invalid cloud-hypervisor JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cloud-hypervisor request timed out after {0:?}")]
    Timeout(Duration),
}

#[derive(Debug)]
pub(crate) struct HttpResponse {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

pub(crate) fn check_status(status: u16, body: &[u8]) -> Result<()> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    Err(Error::Status {
        status,
        body: String::from_utf8_lossy(body).into_owned(),
    }
    .into())
}

/// Read one HTTP response from a persistent Cloud Hypervisor connection.
///
/// `read_to_end` is deliberately not used: the VMM may keep the socket open
/// even when the request asked for `Connection: close`. Framing the body with
/// `Content-Length` (or chunked encoding) lets the client return as soon as the
/// response is complete instead of waiting for EOF until the timeout.
pub(crate) async fn read_http_response(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut raw = Vec::new();
    loop {
        let mut buffer = [0u8; 8 * 1024];
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
        if raw.len() > MAX_RESPONSE_BYTES {
            return Err(
                Error::Protocol(format!("response exceeds {MAX_RESPONSE_BYTES} bytes")).into(),
            );
        }

        let Some(header_end) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&raw[..header_end])
            .map_err(|_| Error::Protocol("headers are not UTF-8".into()))?;
        let status = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or(0);
        // 204/304 and 1xx responses have no body and end at the header block.
        if status == 204 || status == 304 || (100..200).contains(&status) {
            return Ok(raw);
        }
        if let Some(length) =
            header_value(headers, "content-length").and_then(|value| value.parse::<usize>().ok())
        {
            let needed = header_end
                .checked_add(4)
                .and_then(|offset| offset.checked_add(length))
                .ok_or_else(|| Error::Protocol("content length overflow".into()))?;
            if raw.len() >= needed {
                return Ok(raw);
            }
        } else if header_value(headers, "transfer-encoding")
            .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
        {
            // Keep reading until the chunked body (including its terminator)
            // is complete. A transient decode error means "not yet complete".
            if decode_chunked(&raw[header_end + 4..]).is_ok() {
                return Ok(raw);
            }
        } else {
            // No framing: EOF is the only delimiter.
        }
    }
    if raw.is_empty() {
        return Err(Error::Protocol("empty response".into()).into());
    }
    Ok(raw)
}

fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

pub(crate) fn parse_http_response(raw: &[u8]) -> Result<HttpResponse> {
    if raw.len() > MAX_RESPONSE_BYTES {
        return Err(Error::Protocol(format!("response exceeds {MAX_RESPONSE_BYTES} bytes")).into());
    }
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| Error::Protocol("missing header terminator".into()))?;
    let headers = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| Error::Protocol("headers are not UTF-8".into()))?;
    let mut lines = headers.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| Error::Protocol("empty response".into()))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| Error::Protocol(format!("malformed status line: {status_line}")))?
        .parse::<u16>()
        .map_err(|_| Error::Protocol(format!("invalid status code in {status_line}")))?;

    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            } else if name == "content-length" {
                content_length = value.parse::<usize>().ok();
            }
        }
    }

    let remainder = &raw[header_end + 4..];
    let body = if chunked {
        decode_chunked(remainder)?
    } else if let Some(length) = content_length {
        if remainder.len() < length {
            return Err(Error::Protocol(format!(
                "body truncated: expected {length} bytes, got {}",
                remainder.len()
            ))
            .into());
        }
        remainder[..length].to_vec()
    } else {
        remainder.to_vec()
    };

    Ok(HttpResponse { status, body })
}

fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| Error::Protocol("chunk size missing CRLF".into()))?;
        let size_line = std::str::from_utf8(&data[..line_end])
            .map_err(|_| Error::Protocol("chunk size is not UTF-8".into()))?;
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| Error::Protocol(format!("invalid chunk size: {size_line}")))?;
        data = &data[line_end + 2..];
        if size == 0 {
            break;
        }
        let Some(needed) = size.checked_add(2) else {
            return Err(Error::Protocol(format!("chunk size overflow: {size_line}")).into());
        };
        if data.len() < needed {
            return Err(Error::Protocol("chunk truncated".into()).into());
        }
        out.extend_from_slice(&data[..size]);
        data = &data[needed..];
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_responses() {
        let mut raw = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n".to_vec();
        raw.resize(MAX_RESPONSE_BYTES + 1, b'x');
        let err = format!("{:#}", parse_http_response(&raw).unwrap_err());
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn decodes_chunked_bodies() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n4\r\n:1}\n\r\n0\r\n\r\n";
        let parsed = parse_http_response(raw).unwrap();
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.body, b"{\"a\":1}\n");
    }

    #[test]
    fn rejects_chunk_size_overflow() {
        let raw = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            usize::MAX
        );
        let err = format!("{:#}", parse_http_response(raw.as_bytes()).unwrap_err());
        assert!(err.contains("chunk size overflow"), "{err}");
    }

    #[test]
    fn rejects_truncated_responses() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort";
        assert!(parse_http_response(raw).is_err());
    }
}
