//! Message batching and the raw-deflate framing scheme used on `/socket`.
//! See `../PROTOCOL.md` §1 for the exact wire format this reproduces
//! (byte-for-byte, since old and new clients share the same JS inflate
//! code).

use std::io::Write;

use flate2::write::DeflateEncoder;
use flate2::Compression;

use crate::protocol::server::ServerMessage;

/// Accumulates outgoing messages for one connection and flushes them as a
/// single `{"msgs":[...]}` batch, matching
/// `CrawlWebSocket.queue_message`/`flush_messages` exactly (including that
/// the inner array elements are spliced pre-serialized JSON text, not a
/// `Vec<Value>` re-encoded).
#[derive(Debug, Default)]
pub struct MessageBatcher {
    queue: Vec<String>,
}

impl MessageBatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a typed message without flushing.
    pub fn queue(&mut self, msg: &ServerMessage) -> serde_json::Result<()> {
        self.queue.push(msg.to_json()?);
        Ok(())
    }

    /// Queue an already-serialized JSON object (used for messages forwarded
    /// verbatim from the DCSS process, which must not be re-parsed/re-encoded
    /// per the performance requirements in ARCHITECTURE.md).
    pub fn queue_raw(&mut self, json_object_text: impl Into<String>) {
        self.queue.push(json_object_text.into());
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Produce the batch frame body (uncompressed JSON text) and clear the
    /// queue. Returns `None` if nothing was queued (mirrors
    /// `flush_messages` returning `False`/no-op on an empty queue).
    pub fn flush(&mut self) -> Option<String> {
        if self.queue.is_empty() {
            return None;
        }
        let body = format!("{{\"msgs\":[{}]}}", self.queue.join(","));
        self.queue.clear();
        Some(body)
    }
}

/// Per-connection raw-deflate compressor, matching Python's
/// `self._compressobj = zlib.compressobj(Z_DEFAULT_COMPRESSION, DEFLATED, -MAX_WBITS)`:
/// a *persistent* compression context is kept for the lifetime of a
/// WebSocket connection, so later frames can reference the dictionary
/// built up by earlier ones (better compression than resetting per
/// message). Each call performs a `Z_SYNC_FLUSH`-equivalent flush and
/// strips the trailing 4-byte sync marker (`00 00 FF FF`), matching:
/// ```python
/// compressed = compressobj.compress(data) + compressobj.flush(zlib.Z_SYNC_FLUSH)
/// compressed = compressed[:-4]
/// ```
/// The result is **not** a valid standalone zlib/deflate stream on its own
/// (no header, no final block) — the browser's per-connection inflate
/// context (which mirrors this same persistent-state design) appends the
/// same 4 bytes back before feeding each frame to its inflator.
pub struct FrameCompressor {
    encoder: DeflateEncoder<Vec<u8>>,
}

impl Default for FrameCompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameCompressor {
    pub fn new() -> Self {
        Self {
            encoder: DeflateEncoder::new(Vec::new(), Compression::default()),
        }
    }

    /// Compress one frame body, keeping compressor state for subsequent
    /// calls.
    pub fn compress_frame(&mut self, data: &[u8]) -> std::io::Result<Vec<u8>> {
        self.encoder.write_all(data)?;
        self.encoder.flush()?; // Z_SYNC_FLUSH-equivalent; does not end the stream
        let accumulated = std::mem::take(self.encoder.get_mut());
        let trim = accumulated.len().saturating_sub(4);
        Ok(accumulated[..trim].to_vec())
    }
}

/// Inverse of [`FrameCompressor`], for tests (and any future non-browser
/// Rust client); the production browser client does its own inflate in
/// JS. Also stateful, mirroring the compressor.
pub struct FrameDecompressor {
    decoder: flate2::write::DeflateDecoder<Vec<u8>>,
}

impl Default for FrameDecompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecompressor {
    pub fn new() -> Self {
        Self {
            decoder: flate2::write::DeflateDecoder::new(Vec::new()),
        }
    }

    pub fn decompress_frame(&mut self, compressed: &[u8]) -> std::io::Result<Vec<u8>> {
        self.decoder.write_all(compressed)?;
        self.decoder.write_all(&[0x00, 0x00, 0xff, 0xff])?;
        self.decoder.flush()?;
        Ok(std::mem::take(self.decoder.get_mut()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::server::ServerMessage;

    #[test]
    fn batcher_produces_pythons_exact_frame_shape() {
        let mut batcher = MessageBatcher::new();
        batcher.queue(&ServerMessage::Ping).unwrap();
        batcher
            .queue(&ServerMessage::GameStarted)
            .unwrap();
        let frame = batcher.flush().unwrap();
        assert_eq!(frame, r#"{"msgs":[{"msg":"ping"},{"msg":"game_started"}]}"#);
        assert!(batcher.is_empty());
    }

    #[test]
    fn flush_on_empty_queue_returns_none() {
        let mut batcher = MessageBatcher::new();
        assert!(batcher.flush().is_none());
    }

    #[test]
    fn queue_raw_is_spliced_without_reparsing() {
        let mut batcher = MessageBatcher::new();
        batcher.queue_raw(r#"{"msg":"map","cells":[[1,2,3]]}"#);
        let frame = batcher.flush().unwrap();
        assert_eq!(frame, r#"{"msgs":[{"msg":"map","cells":[[1,2,3]]}]}"#);
    }

    #[test]
    fn deflate_round_trips_through_inflate() {
        let original = br#"{"msgs":[{"msg":"ping"},{"msg":"chat","content":"hello world"}]}"#;
        let mut compressor = FrameCompressor::new();
        let compressed = compressor.compress_frame(original).unwrap();
        // sanity: the raw stream must NOT contain the standard zlib header
        assert_ne!(&compressed[..2], &[0x78, 0x9c][..]);
        let mut decompressor = FrameDecompressor::new();
        let decompressed = decompressor.decompress_frame(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn deflate_handles_larger_payloads() {
        let original = "x".repeat(200_000);
        let mut compressor = FrameCompressor::new();
        let compressed = compressor.compress_frame(original.as_bytes()).unwrap();
        let mut decompressor = FrameDecompressor::new();
        let decompressed = decompressor.decompress_frame(&compressed).unwrap();
        assert_eq!(decompressed, original.as_bytes());
    }

    #[test]
    fn compressor_state_persists_across_multiple_frames_like_python() {
        // Python keeps one `compressobj` per connection for its whole
        // lifetime; a second message benefits from (and must remain
        // decodable given) the dictionary built by the first.
        let mut compressor = FrameCompressor::new();
        let mut decompressor = FrameDecompressor::new();

        let frame1 = compressor.compress_frame(b"repeated repeated repeated").unwrap();
        let frame2 = compressor
            .compress_frame(b"repeated repeated repeated again")
            .unwrap();

        assert_eq!(
            decompressor.decompress_frame(&frame1).unwrap(),
            b"repeated repeated repeated"
        );
        assert_eq!(
            decompressor.decompress_frame(&frame2).unwrap(),
            b"repeated repeated repeated again"
        );
    }
}

