// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::io;

use crate::coding::{Encode, EncodeError};

use super::SessionError;
use bytes::Buf;

pub struct Writer {
    stream: web_transport::SendStream,
    buffer: bytes::BytesMut,
}

impl Writer {
    pub fn new(stream: web_transport::SendStream) -> Self {
        Self {
            stream,
            buffer: Default::default(),
        }
    }

    pub async fn encode<T: Encode>(&mut self, msg: &T) -> Result<(), SessionError> {
        self.buffer.clear();
        tracing::trace!(
            "[WRITER] encode: encoding {} to buffer",
            std::any::type_name::<T>()
        );

        msg.encode(&mut self.buffer)?;
        let encoded_len = self.buffer.len();
        tracing::debug!(
            "[WRITER] encode: encoded {} ({} bytes), sending to stream",
            std::any::type_name::<T>(),
            encoded_len
        );

        let mut total_written = 0;
        while !self.buffer.is_empty() {
            let written = self.stream.write_buf(&mut self.buffer).await?;
            total_written += written;
            tracing::trace!(
                "[WRITER] encode: wrote {} bytes to stream (total={}/{}, remaining={})",
                written,
                total_written,
                encoded_len,
                self.buffer.len()
            );
        }

        tracing::debug!(
            "[WRITER] encode: finished sending {} ({} bytes total)",
            std::any::type_name::<T>(),
            total_written
        );

        Ok(())
    }

    pub async fn write(&mut self, buf: &[u8]) -> Result<(), SessionError> {
        tracing::trace!("[WRITER] write: writing {} bytes to stream", buf.len());

        let mut cursor = io::Cursor::new(buf);
        let total_len = buf.len();
        let mut total_written = 0;

        while cursor.has_remaining() {
            let size = self.stream.write_buf(&mut cursor).await?;
            if size == 0 {
                tracing::error!(
                    "[WRITER] write: ERROR - wrote 0 bytes with {} bytes remaining",
                    cursor.remaining()
                );
                return Err(EncodeError::More(cursor.remaining()).into());
            }
            total_written += size;
            tracing::trace!(
                "[WRITER] write: wrote {} bytes (total={}/{}, remaining={})",
                size,
                total_written,
                total_len,
                cursor.remaining()
            );
        }

        tracing::debug!("[WRITER] write: finished writing {} bytes", total_written);

        Ok(())
    }

    /// Cleanly close the stream with a FIN at the current offset.
    ///
    /// For data streams this asserts that everything the receiver was promised
    /// has been written, so callers must only use it at a message boundary. See
    /// [`crate::data::DataStreamResetCode`] for why a mid-object FIN is a
    /// protocol violation rather than a cosmetic issue.
    pub(super) fn finish(&mut self) -> Result<(), SessionError> {
        self.stream.finish()?;
        Ok(())
    }

    /// Abort the stream with a `RESET_STREAM` carrying `code`.
    pub(super) fn reset(&mut self, code: u32) {
        self.stream.reset(code);
    }
}
