use anyhow::{anyhow, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MessageFraming {
    JsonLine,
    ContentLength,
}

pub struct StdioTransport<R, W> {
    reader: BufReader<R>,
    writer: BufWriter<W>,
    read_buffer: Vec<u8>,
}

impl<R, W> StdioTransport<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            read_buffer: Vec::with_capacity(8192),
        }
    }

    pub async fn read_message(&mut self) -> Result<Option<(String, MessageFraming)>> {
        loop {
            if let Some(message) = extract_message(&mut self.read_buffer)? {
                return Ok(Some(message));
            }

            let bytes_read = self.reader.read_buf(&mut self.read_buffer).await?;
            if bytes_read == 0 {
                return extract_message_at_eof(&mut self.read_buffer);
            }
        }
    }

    pub async fn write_message(&mut self, message: &str, framing: MessageFraming) -> Result<()> {
        match framing {
            MessageFraming::JsonLine => {
                self.writer.write_all(message.as_bytes()).await?;
                self.writer.write_all(b"\n").await?;
            }
            MessageFraming::ContentLength => {
                let header = format!("Content-Length: {}\r\n\r\n", message.len());
                self.writer.write_all(header.as_bytes()).await?;
                self.writer.write_all(message.as_bytes()).await?;
            }
        }

        self.writer.flush().await?;
        Ok(())
    }
}

fn extract_message(buffer: &mut Vec<u8>) -> Result<Option<(String, MessageFraming)>> {
    trim_leading_whitespace(buffer);
    if buffer.is_empty() {
        return Ok(None);
    }

    if starts_with_content_length(buffer) {
        if let Some(message) = try_extract_content_length_message(buffer)? {
            return Ok(Some((message, MessageFraming::ContentLength)));
        }
        return Ok(None);
    }

    if let Some(message) = try_extract_ndjson_message(buffer)? {
        return Ok(Some((message, MessageFraming::JsonLine)));
    }

    Ok(None)
}

fn extract_message_at_eof(buffer: &mut Vec<u8>) -> Result<Option<(String, MessageFraming)>> {
    if let Some(message) = extract_message(buffer)? {
        return Ok(Some(message));
    }

    trim_leading_whitespace(buffer);
    if buffer.is_empty() {
        return Ok(None);
    }

    if starts_with_content_length(buffer) {
        return Err(anyhow!(
            "Unexpected EOF while reading Content-Length framed message"
        ));
    }

    let trailing = std::str::from_utf8(buffer)?.trim().to_string();
    buffer.clear();

    if trailing.is_empty() {
        return Ok(None);
    }

    Ok(Some((trailing, MessageFraming::JsonLine)))
}

fn try_extract_content_length_message(buffer: &mut Vec<u8>) -> Result<Option<String>> {
    let Some((header_end, delimiter_len)) = find_header_end(buffer) else {
        return Ok(None);
    };

    let headers = &buffer[..header_end];
    let Some(content_length) = parse_content_length(headers)? else {
        return Err(anyhow!("Missing Content-Length header"));
    };

    let body_start = header_end + delimiter_len;
    let body_end = body_start + content_length;
    if buffer.len() < body_end {
        return Ok(None);
    }

    let message = String::from_utf8(buffer[body_start..body_end].to_vec())?;
    buffer.drain(..body_end);
    Ok(Some(message))
}

fn try_extract_ndjson_message(buffer: &mut Vec<u8>) -> Result<Option<String>> {
    loop {
        let Some(newline_pos) = buffer.iter().position(|byte| *byte == b'\n') else {
            return Ok(None);
        };

        let mut line = buffer[..newline_pos].to_vec();
        buffer.drain(..=newline_pos);

        if let Some(b'\r') = line.last().copied() {
            line.pop();
        }

        let text = String::from_utf8(line)?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }

        return Ok(Some(trimmed.to_string()));
    }
}

fn parse_content_length(headers: &[u8]) -> Result<Option<usize>> {
    for raw_line in headers.split(|byte| *byte == b'\n') {
        let line = trim_trailing_cr(raw_line);
        if line.is_empty() {
            continue;
        }

        let Some((name, value)) = split_header(line) else {
            continue;
        };

        if name.eq_ignore_ascii_case("Content-Length") {
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|err| anyhow!("Invalid Content-Length value: {err}"))?;
            return Ok(Some(parsed));
        }
    }

    Ok(None)
}

fn split_header(line: &[u8]) -> Option<(&str, &str)> {
    let colon = line.iter().position(|byte| *byte == b':')?;
    let name = std::str::from_utf8(&line[..colon]).ok()?;
    let value = std::str::from_utf8(&line[colon + 1..]).ok()?;
    Some((name, value))
}

fn trim_trailing_cr(line: &[u8]) -> &[u8] {
    if line.ends_with(b"\r") {
        &line[..line.len() - 1]
    } else {
        line
    }
}

fn trim_leading_whitespace(buffer: &mut Vec<u8>) {
    let count = buffer
        .iter()
        .take_while(|byte| byte.is_ascii_whitespace())
        .count();
    if count > 0 {
        buffer.drain(..count);
    }
}

fn starts_with_content_length(buffer: &[u8]) -> bool {
    const PREFIX: &[u8] = b"content-length:";
    buffer.len() >= PREFIX.len()
        && buffer[..PREFIX.len()]
            .iter()
            .zip(PREFIX.iter())
            .all(|(left, right)| left.to_ascii_lowercase() == *right)
}

fn find_header_end(buffer: &[u8]) -> Option<(usize, usize)> {
    find_subsequence(buffer, b"\r\n\r\n")
        .map(|index| (index, 4))
        .or_else(|| find_subsequence(buffer, b"\n\n").map(|index| (index, 2)))
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }

    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::{extract_message, extract_message_at_eof, MessageFraming};

    #[test]
    fn test_extract_ndjson_message() {
        let mut buffer = br#"{"jsonrpc":"2.0","id":1}"#.to_vec();
        buffer.push(b'\n');

        let message = extract_message(&mut buffer)
            .expect("parse failed")
            .expect("message missing");

        assert_eq!(message.1, MessageFraming::JsonLine);
        assert_eq!(message.0, r#"{"jsonrpc":"2.0","id":1}"#);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_extract_content_length_message() {
        let body = r#"{"jsonrpc":"2.0","id":1}"#;
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut buffer = frame.into_bytes();

        let message = extract_message(&mut buffer)
            .expect("parse failed")
            .expect("message missing");

        assert_eq!(message.1, MessageFraming::ContentLength);
        assert_eq!(message.0, body);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_extract_multiple_content_length_messages() {
        let first = r#"{"id":1}"#;
        let second = r#"{"id":2}"#;
        let frame = format!(
            "Content-Length: {}\r\n\r\n{}Content-Length: {}\r\n\r\n{}",
            first.len(),
            first,
            second.len(),
            second
        );
        let mut buffer = frame.into_bytes();

        let first_message = extract_message(&mut buffer)
            .expect("first parse failed")
            .expect("first message missing");
        let second_message = extract_message(&mut buffer)
            .expect("second parse failed")
            .expect("second message missing");

        assert_eq!(first_message.0, first);
        assert_eq!(second_message.0, second);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_extract_message_at_eof_for_ndjson_without_newline() {
        let mut buffer = br#"{"jsonrpc":"2.0","id":42}"#.to_vec();
        let message = extract_message_at_eof(&mut buffer)
            .expect("parse failed")
            .expect("message missing");

        assert_eq!(message.1, MessageFraming::JsonLine);
        assert_eq!(message.0, r#"{"jsonrpc":"2.0","id":42}"#);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_extract_message_returns_none_for_partial_content_length() {
        let body = r#"{"jsonrpc":"2.0","id":1}"#;
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len() + 10, body);
        let mut buffer = frame.into_bytes();

        let message = extract_message(&mut buffer).expect("parse failed");
        assert!(message.is_none());
    }
}
