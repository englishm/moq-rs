// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{cmp, io};

use bytes::{Buf, Bytes, BytesMut};

use crate::coding::{Decode, DecodeError};

use super::{SessionError, SessionId};

pub struct Reader {
    session_id: SessionId,
    stream: web_transport::RecvStream,
    buffer: BytesMut,
}

impl Reader {
    pub fn new(session_id: SessionId, stream: web_transport::RecvStream) -> Self {
        Self {
            session_id,
            stream,
            buffer: Default::default(),
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub async fn decode<T: Decode>(&mut self) -> Result<T, SessionError> {
        tracing::trace!(
            "[READER] decode: attempting to decode {} (buffer_len={})",
            std::any::type_name::<T>(),
            self.buffer.len()
        );

        loop {
            let mut cursor = io::Cursor::new(&self.buffer);

            // Try to decode with the current buffer.
            let required = match T::decode(&mut cursor) {
                Ok(msg) => {
                    let consumed = cursor.position() as usize;
                    self.buffer.advance(consumed);
                    tracing::trace!(
                        "[READER] decode: successfully decoded {} (consumed={} bytes, buffer_remaining={})",
                        std::any::type_name::<T>(),
                        consumed,
                        self.buffer.len()
                    );
                    return Ok(msg);
                }
                Err(DecodeError::More(required)) => {
                    let total_needed = self.buffer.len() + required;
                    tracing::trace!(
                        "[READER] decode: need more data for {} (current={} bytes, need={} more, total_required={})",
                        std::any::type_name::<T>(),
                        self.buffer.len(),
                        required,
                        total_needed
                    );
                    total_needed
                }
                Err(err) => {
                    tracing::error!(
                        session_id = %self.session_id,
                        "[READER] decode: ERROR decoding {} - {:?} (buffer_len={})",
                        std::any::type_name::<T>(),
                        err,
                        self.buffer.len()
                    );
                    return Err(err.into());
                }
            };

            // Read in more data until we reach the requested amount.
            // We always read at least once to avoid an infinite loop if some dingus puts remain=0
            loop {
                let before_read = self.buffer.len();
                if self.stream.read_buf(&mut self.buffer).await?.is_none() {
                    tracing::warn!(
                        session_id = %self.session_id,
                        "[READER] decode: stream ended while waiting for data (have={} bytes, need={})",
                        self.buffer.len(),
                        required
                    );
                    return Err(DecodeError::More(required - self.buffer.len()).into());
                };

                let read_amount = self.buffer.len() - before_read;
                tracing::trace!(
                    "[READER] decode: read {} bytes from stream (buffer_len={})",
                    read_amount,
                    self.buffer.len()
                );

                if self.buffer.len() >= required {
                    tracing::trace!(
                        "[READER] decode: have enough data now (buffer_len={}), retrying decode",
                        self.buffer.len()
                    );
                    break;
                }
            }
        }
    }

    pub async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>, SessionError> {
        tracing::trace!(
            "[READER] read_chunk: requested max={} bytes (buffer_len={})",
            max,
            self.buffer.len()
        );

        if !self.buffer.is_empty() {
            let size = cmp::min(max, self.buffer.len());
            let data = self.buffer.split_to(size).freeze();
            tracing::trace!(
                "[READER] read_chunk: returned {} bytes from buffer (buffer_remaining={})",
                data.len(),
                self.buffer.len()
            );
            return Ok(Some(data));
        }

        let chunk = self.stream.read(max).await?;
        if let Some(ref data) = chunk {
            tracing::trace!("[READER] read_chunk: read {} bytes from stream", data.len());
        } else {
            tracing::trace!("[READER] read_chunk: stream returned None");
        }
        Ok(chunk)
    }

    pub async fn done(&mut self) -> Result<bool, SessionError> {
        if !self.buffer.is_empty() {
            return Ok(false);
        }

        Ok(self.stream.read_buf(&mut self.buffer).await?.is_none())
    }

    pub(super) fn stop(&mut self, code: u32) {
        self.stream.stop(code);
    }
}
