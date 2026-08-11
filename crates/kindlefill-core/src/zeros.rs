//! A synthetic byte source for filler objects.
//!
//! This is where we beat the download-a-ZIP-and-extract-it approach. That workflow
//! expands an archive into ~13 GB of real files on the host, then reads all of it
//! back to push over USB — 26 GB of local disk I/O, plus 13 GB of free space you
//! have to have spare. We generate the bytes in memory and stream them straight
//! into the upload: no temp files, no host free-space requirement.
//!
//! It does *not* make the USB leg faster. Nothing can: MTP has no compressed
//! transfer mode, and the device can't expand an archive without code execution on
//! it, which is precisely what you don't have before jailbreaking. The wire is the
//! floor.

use bytes::Bytes;
use futures::Stream;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

/// How much we hand the transport per poll. Large enough that per-chunk bookkeeping
/// is noise against USB transfer time, small enough to stay responsive to cancellation.
const CHUNK: usize = 1024 * 1024;

/// Yields `len` zero bytes, then ends.
///
/// One zero-filled block is allocated per stream and re-sliced for every chunk;
/// `Bytes::slice` is a refcount bump, not a copy, so a 13 GB fill allocates 1 MiB
/// per object rather than per chunk.
pub struct ZeroStream {
    remaining: u64,
    block: Bytes,
}

impl ZeroStream {
    #[must_use]
    pub fn new(len: u64) -> Self {
        let block = usize::try_from(len).unwrap_or(CHUNK).min(CHUNK);
        Self {
            remaining: len,
            block: Bytes::from(vec![0u8; block.max(1)]),
        }
    }

    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.remaining
    }
}

impl Stream for ZeroStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        // `block.len()` fits usize by construction, and `remaining` is clamped to it.
        let take = self.remaining.min(self.block.len() as u64) as usize;
        let chunk = self.block.slice(0..take);
        self.remaining -= take as u64;
        Poll::Ready(Some(Ok(chunk)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    async fn drain(len: u64) -> (u64, usize) {
        let mut s = ZeroStream::new(len);
        let (mut total, mut chunks) = (0u64, 0usize);
        while let Some(chunk) = s.next().await {
            let chunk = chunk.unwrap();
            assert!(chunk.iter().all(|&b| b == 0), "must be zero-filled");
            total += chunk.len() as u64;
            chunks += 1;
        }
        (total, chunks)
    }

    #[tokio::test]
    async fn yields_exactly_the_requested_length() {
        // The declared size must match the bytes sent, or the device rejects the object.
        for len in [0u64, 1, 4096, CHUNK as u64, CHUNK as u64 + 1, 5 * CHUNK as u64] {
            let (total, _) = drain(len).await;
            assert_eq!(total, len, "length mismatch for {len}");
        }
    }

    #[tokio::test]
    async fn chunks_the_stream_rather_than_buffering_it_whole() {
        let (_, chunks) = drain(5 * CHUNK as u64).await;
        assert_eq!(chunks, 5);
    }

    #[tokio::test]
    async fn small_streams_do_not_allocate_a_full_block() {
        let s = ZeroStream::new(512);
        assert_eq!(s.block.len(), 512);
    }
}
