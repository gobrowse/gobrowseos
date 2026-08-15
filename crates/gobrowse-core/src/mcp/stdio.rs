//! Bounded line framing for caller-provided MCP stdio pipes.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::wire::{
    MAX_FRAME_BYTES, ValidatedMessage, ValidatedNotification, ValidatedRequest, WireError, decode,
    encode,
};

/// A bounded JSON-RPC line transport over caller-provided asynchronous pipes.
pub struct McpStdioTransport<R, W> {
    reader: R,
    writer: W,
}

/// Errors produced by [`McpStdioTransport`].
#[derive(Debug, thiserror::Error)]
pub enum McpStdioError {
    #[error("stdio transport I/O error")]
    Io(#[from] io::Error),
    #[error("stdio stream ended before the next frame")]
    CleanEof,
    #[error("stdio stream ended during a frame")]
    PartialEof,
    #[error("stdio frame exceeds the configured limit")]
    FrameTooLarge,
    #[error("invalid JSON-RPC frame")]
    Wire(#[from] WireError),
}

impl<R, W> McpStdioTransport<R, W> {
    /// Wrap caller-provided reader and writer pipes without opening or spawning anything.
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }
}

impl<R, W> McpStdioTransport<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Encode, write, and flush a validated JSON-RPC request as one LF-terminated frame.
    pub async fn send_request(&mut self, request: ValidatedRequest) -> Result<(), McpStdioError> {
        self.send(ValidatedMessage::Request(request)).await
    }

    /// Encode, write, and flush a validated JSON-RPC notification as one LF-terminated frame.
    pub async fn send_notification(
        &mut self,
        notification: ValidatedNotification,
    ) -> Result<(), McpStdioError> {
        self.send(ValidatedMessage::Notification(notification))
            .await
    }

    /// Receive and validate one bounded LF- or CRLF-terminated JSON-RPC frame.
    pub async fn recv(&mut self) -> Result<ValidatedMessage, McpStdioError> {
        let mut payload = Vec::new();
        let mut pending_cr = false;
        let mut byte = [0_u8; 1];

        loop {
            if self.reader.read(&mut byte).await? == 0 {
                return if pending_cr && payload.len() == MAX_FRAME_BYTES {
                    Err(McpStdioError::FrameTooLarge)
                } else if payload.is_empty() && !pending_cr {
                    Err(McpStdioError::CleanEof)
                } else {
                    Err(McpStdioError::PartialEof)
                };
            }

            let byte = byte[0];
            if pending_cr {
                if byte == b'\n' {
                    return decode(&payload).map_err(McpStdioError::Wire);
                }
                push_payload_byte(&mut payload, b'\r')?;
                pending_cr = false;
            }

            match byte {
                b'\n' => return decode(&payload).map_err(McpStdioError::Wire),
                b'\r' => pending_cr = true,
                byte => push_payload_byte(&mut payload, byte)?,
            }
        }
    }

    async fn send(&mut self, message: ValidatedMessage) -> Result<(), McpStdioError> {
        let payload = encode(&message).map_err(McpStdioError::Wire)?;
        self.writer.write_all(&payload).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;
        Ok(())
    }
}

fn push_payload_byte(payload: &mut Vec<u8>, byte: u8) -> Result<(), McpStdioError> {
    if payload.len() == MAX_FRAME_BYTES {
        return Err(McpStdioError::FrameTooLarge);
    }
    payload.push(byte);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{io, pin::Pin, task::Context, task::Poll};

    use tokio::io::{AsyncWrite, AsyncWriteExt, DuplexStream, Sink, duplex};

    use super::*;
    use crate::mcp::wire::{
        JSONRPC_VERSION, RequestId, decode, encode, try_notification, try_request,
    };

    #[derive(Default)]
    struct RecordingWriter {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl AsyncWrite for RecordingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.bytes.extend_from_slice(buffer);
            Poll::Ready(Ok(buffer.len()))
        }

        fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.flushes += 1;
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    async fn transport_with_frame(frame: &[u8]) -> McpStdioTransport<DuplexStream, Sink> {
        let (mut writer, reader) = duplex(frame.len().max(1));
        writer.write_all(frame).await.unwrap();
        drop(writer);
        McpStdioTransport::new(reader, tokio::io::sink())
    }

    #[tokio::test]
    async fn sends_validated_messages_with_one_lf_and_flush() {
        let request = try_request(RequestId::Number(7), "ping", None).unwrap();
        let expected_request = encode(&ValidatedMessage::Request(request.clone())).unwrap();
        let mut request_transport =
            McpStdioTransport::new(tokio::io::empty(), RecordingWriter::default());
        request_transport.send_request(request).await.unwrap();
        assert_eq!(
            request_transport.writer.bytes,
            [expected_request.as_slice(), b"\n"].concat()
        );
        assert_eq!(request_transport.writer.flushes, 1);

        let notification = try_notification("notifications/initialized", None).unwrap();
        let expected_notification =
            encode(&ValidatedMessage::Notification(notification.clone())).unwrap();
        let mut notification_transport =
            McpStdioTransport::new(tokio::io::empty(), RecordingWriter::default());
        notification_transport
            .send_notification(notification)
            .await
            .unwrap();
        assert_eq!(
            notification_transport.writer.bytes,
            [expected_notification.as_slice(), b"\n"].concat()
        );
        assert_eq!(notification_transport.writer.flushes, 1);
    }

    #[tokio::test]
    async fn decodes_equivalent_lf_and_crlf_frames_through_wire_decoder() {
        let payload = br#"{"jsonrpc":"2.0","method":"ping"}"#;
        let expected = decode(payload).unwrap();

        let mut lf_frame = payload.to_vec();
        lf_frame.push(b'\n');
        let mut crlf_frame = payload.to_vec();
        crlf_frame.extend_from_slice(b"\r\n");

        let mut lf_transport = transport_with_frame(&lf_frame).await;
        let mut crlf_transport = transport_with_frame(&crlf_frame).await;

        assert_eq!(lf_transport.recv().await.unwrap(), expected);
        assert_eq!(crlf_transport.recv().await.unwrap(), expected);
    }

    #[tokio::test]
    async fn maps_terminated_malformed_frame_to_exact_wire_error() {
        let mut transport = transport_with_frame(b"{\n").await;

        assert!(matches!(
            transport.recv().await,
            Err(McpStdioError::Wire(WireError::Malformed))
        ));
    }

    #[tokio::test]
    async fn accepts_limit_sized_payload_and_rejects_first_byte_beyond_it() {
        let prefix = br#"{"jsonrpc":"2.0","method":"x","params":""#;
        let suffix = br#""}"#;
        let mut at_limit = Vec::with_capacity(MAX_FRAME_BYTES + 1);
        at_limit.extend_from_slice(prefix);
        at_limit.extend(std::iter::repeat_n(
            b'x',
            MAX_FRAME_BYTES - prefix.len() - suffix.len(),
        ));
        at_limit.extend_from_slice(suffix);
        assert_eq!(at_limit.len(), MAX_FRAME_BYTES);

        let mut accepted_transport =
            transport_with_frame(&[at_limit.as_slice(), b"\n"].concat()).await;
        assert!(accepted_transport.recv().await.is_ok());

        let over_limit = vec![b'x'; MAX_FRAME_BYTES + 1];
        let mut rejected_transport = transport_with_frame(&over_limit).await;
        assert!(matches!(
            rejected_transport.recv().await,
            Err(McpStdioError::FrameTooLarge)
        ));
    }

    #[tokio::test]
    async fn distinguishes_clean_and_partial_eof() {
        let mut clean_transport = transport_with_frame(b"").await;
        assert!(matches!(
            clean_transport.recv().await,
            Err(McpStdioError::CleanEof)
        ));

        let mut partial_transport = transport_with_frame(b"{").await;
        assert!(matches!(
            partial_transport.recv().await,
            Err(McpStdioError::PartialEof)
        ));
    }

    #[tokio::test]
    async fn preserves_decoded_responses_without_correlation() {
        let payload =
            format!(r#"{{"jsonrpc":"{JSONRPC_VERSION}","id":9,"result":{{"answer":true}}}}"#);
        let mut transport = transport_with_frame(&[payload.as_bytes(), b"\n"].concat()).await;

        let response = transport.recv().await.unwrap();
        assert!(matches!(&response, ValidatedMessage::Response(_)));
        assert_eq!(encode(&response).unwrap(), payload.as_bytes());
    }
}
