//! Newline-delimited framing for the MCP stdio transport.
//!
//! MCP stdio messages are UTF-8 JSON-RPC objects delimited by a single
//! `\n`; messages must not contain embedded newlines. This module owns
//! that byte-level boundary: it never interprets frame contents, it
//! bounds memory with a hard per-frame size cap, and every failure is a
//! typed error — no panics. This is the parser-facing seam designated
//! for fuzz coverage in T5.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Default per-frame size cap: 16 MiB.
///
/// Large tool results (base64 images and the like) fit comfortably;
/// anything larger is treated as a framing violation instead of being
/// buffered without bound.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

const READ_CHUNK_BYTES: usize = 8 * 1024;

/// Errors while reading frames.
#[derive(Debug, thiserror::Error)]
pub enum FrameReadError {
    /// A frame exceeded the size cap. The reader has already discarded
    /// the rest of the oversized line, so the stream is re-synchronized
    /// and the next [`FrameReader::read_frame`] call is valid.
    #[error("frame exceeds the {limit}-byte limit")]
    Oversized {
        /// The configured cap that was exceeded.
        limit: usize,
    },

    /// The underlying transport failed.
    #[error("transport i/o error")]
    Io(#[from] std::io::Error),
}

/// Errors while writing frames.
#[derive(Debug, thiserror::Error)]
pub enum FrameWriteError {
    /// The outbound frame contained an embedded newline, which would
    /// desynchronize the peer's framing.
    #[error("outbound frame contains an embedded newline")]
    EmbeddedNewline,

    /// The underlying transport failed.
    #[error("transport i/o error")]
    Io(#[from] std::io::Error),
}

/// Reads `\n`-delimited frames from an [`AsyncRead`] with a size cap.
pub struct FrameReader<R> {
    inner: R,
    /// Bytes read from the transport but not yet returned as frames.
    buf: Vec<u8>,
    max_frame: usize,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// Creates a reader enforcing `max_frame` as the per-frame byte cap.
    pub fn new(inner: R, max_frame: usize) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            max_frame,
        }
    }

    /// Reads the next frame, without its trailing `\n`.
    ///
    /// Returns `Ok(None)` at end of stream. A final unterminated frame
    /// (data followed by EOF instead of `\n`) is returned as a frame,
    /// mirroring `BufRead::lines` tolerance.
    pub async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, FrameReadError> {
        let mut searched = 0;
        loop {
            if let Some(offset) = find_newline(&self.buf[searched..]) {
                let newline = searched + offset;
                let mut frame: Vec<u8> = self.buf.drain(..=newline).collect();
                frame.pop();
                if frame.len() > self.max_frame {
                    return Err(FrameReadError::Oversized {
                        limit: self.max_frame,
                    });
                }
                return Ok(Some(frame));
            }
            if self.buf.len() > self.max_frame {
                self.discard_until_newline().await?;
                return Err(FrameReadError::Oversized {
                    limit: self.max_frame,
                });
            }
            searched = self.buf.len();
            let mut chunk = [0u8; READ_CHUNK_BYTES];
            let n = self.inner.read(&mut chunk).await?;
            if n == 0 {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                let frame = std::mem::take(&mut self.buf);
                return Ok(Some(frame));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Drops buffered bytes of an oversized frame and consumes the
    /// transport until the next `\n` (or EOF), bounding memory use while
    /// re-synchronizing the stream.
    async fn discard_until_newline(&mut self) -> Result<(), FrameReadError> {
        loop {
            if let Some(pos) = find_newline(&self.buf) {
                self.buf.drain(..=pos);
                return Ok(());
            }
            self.buf.clear();
            let mut chunk = [0u8; READ_CHUNK_BYTES];
            let n = self.inner.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

fn find_newline(haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|&b| b == b'\n')
}

/// Writes one frame followed by the `\n` delimiter, then flushes.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &[u8],
) -> Result<(), FrameWriteError> {
    if frame.contains(&b'\n') {
        return Err(FrameWriteError::EmbeddedNewline);
    }
    writer.write_all(frame).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    async fn reader_over(bytes: &[u8], max: usize) -> FrameReader<std::io::Cursor<Vec<u8>>> {
        FrameReader::new(std::io::Cursor::new(bytes.to_vec()), max)
    }

    #[tokio::test]
    async fn splits_multiple_frames_in_one_chunk() {
        let mut r = reader_over(b"one\ntwo\nthree\n", 1024).await;
        assert_eq!(r.read_frame().await.unwrap(), Some(b"one".to_vec()));
        assert_eq!(r.read_frame().await.unwrap(), Some(b"two".to_vec()));
        assert_eq!(r.read_frame().await.unwrap(), Some(b"three".to_vec()));
        assert_eq!(r.read_frame().await.unwrap(), None);
    }

    #[tokio::test]
    async fn returns_final_unterminated_frame() {
        let mut r = reader_over(b"first\nlast-no-newline", 1024).await;
        assert_eq!(r.read_frame().await.unwrap(), Some(b"first".to_vec()));
        assert_eq!(
            r.read_frame().await.unwrap(),
            Some(b"last-no-newline".to_vec())
        );
        assert_eq!(r.read_frame().await.unwrap(), None);
    }

    #[tokio::test]
    async fn empty_lines_are_empty_frames() {
        let mut r = reader_over(b"\n\nx\n", 1024).await;
        assert_eq!(r.read_frame().await.unwrap(), Some(Vec::new()));
        assert_eq!(r.read_frame().await.unwrap(), Some(Vec::new()));
        assert_eq!(r.read_frame().await.unwrap(), Some(b"x".to_vec()));
    }

    #[tokio::test]
    async fn oversized_frame_is_a_typed_error_and_stream_resyncs() {
        let mut input = vec![b'a'; 5000];
        input.push(b'\n');
        input.extend_from_slice(b"ok\n");
        let mut r = reader_over(&input, 1024).await;
        match r.read_frame().await {
            Err(FrameReadError::Oversized { limit }) => assert_eq!(limit, 1024),
            other => panic!("expected Oversized, got {other:?}"),
        }
        assert_eq!(r.read_frame().await.unwrap(), Some(b"ok".to_vec()));
        assert_eq!(r.read_frame().await.unwrap(), None);
    }

    #[tokio::test]
    async fn oversized_frame_spanning_read_chunks_resyncs_to_next_frame() {
        // The oversized line exceeds READ_CHUNK_BYTES, so its
        // terminating newline arrives in a *later* read while the
        // reader is inside discard_until_newline — the mid-stream
        // resync path. The frame after it must survive intact.
        let mut input = vec![b'a'; READ_CHUNK_BYTES + 800];
        input.push(b'\n');
        input.extend_from_slice(b"ok\n");
        let mut r = reader_over(&input, 1024).await;
        assert!(matches!(
            r.read_frame().await,
            Err(FrameReadError::Oversized { limit: 1024 })
        ));
        assert_eq!(r.read_frame().await.unwrap(), Some(b"ok".to_vec()));
        assert_eq!(r.read_frame().await.unwrap(), None);
    }

    #[tokio::test]
    async fn back_to_back_oversized_frames_each_error_then_resync() {
        let mut input = vec![b'a'; 2000];
        input.push(b'\n');
        input.extend(vec![b'b'; 2000]);
        input.push(b'\n');
        input.extend_from_slice(b"ok\n");
        let mut r = reader_over(&input, 1024).await;
        assert!(matches!(
            r.read_frame().await,
            Err(FrameReadError::Oversized { .. })
        ));
        assert!(matches!(
            r.read_frame().await,
            Err(FrameReadError::Oversized { .. })
        ));
        assert_eq!(r.read_frame().await.unwrap(), Some(b"ok".to_vec()));
    }

    #[tokio::test]
    async fn oversized_frame_at_eof_without_newline_resyncs_to_eof() {
        let input = vec![b'a'; 5000];
        let mut r = reader_over(&input, 1024).await;
        assert!(matches!(
            r.read_frame().await,
            Err(FrameReadError::Oversized { .. })
        ));
        assert_eq!(r.read_frame().await.unwrap(), None);
    }

    #[tokio::test]
    async fn frame_exactly_at_limit_is_accepted() {
        let mut input = vec![b'a'; 1024];
        input.push(b'\n');
        let mut r = reader_over(&input, 1024).await;
        assert_eq!(r.read_frame().await.unwrap(), Some(vec![b'a'; 1024]));
    }

    #[tokio::test]
    async fn write_frame_appends_newline() {
        let mut out = Vec::new();
        write_frame(&mut out, b"{}").await.unwrap();
        assert_eq!(out, b"{}\n");
    }

    #[tokio::test]
    async fn write_frame_rejects_embedded_newline() {
        let mut out = Vec::new();
        let err = write_frame(&mut out, b"a\nb").await.unwrap_err();
        assert!(matches!(err, FrameWriteError::EmbeddedNewline));
        assert!(out.is_empty());
    }
}
