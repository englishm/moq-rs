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
        log::trace!(
            "[WRITER] encode: encoding {} to buffer",
            std::any::type_name::<T>()
        );

        msg.encode(&mut self.buffer)?;
        let encoded_len = self.buffer.len();
        log::debug!(
            "[WRITER] encode: encoded {} ({} bytes), sending to stream",
            std::any::type_name::<T>(),
            encoded_len
        );

        let mut total_written = 0;
        while !self.buffer.is_empty() {
            let written = self.stream.write_buf(&mut self.buffer).await?;
            total_written += written;
            log::trace!(
                "[WRITER] encode: wrote {} bytes to stream (total={}/{}, remaining={})",
                written,
                total_written,
                encoded_len,
                self.buffer.len()
            );
        }

        log::debug!(
            "[WRITER] encode: finished sending {} ({} bytes total)",
            std::any::type_name::<T>(),
            total_written
        );

        Ok(())
    }

    pub async fn write(&mut self, buf: &[u8]) -> Result<(), SessionError> {
        log::trace!("[WRITER] write: writing {} bytes to stream", buf.len());

        let mut cursor = io::Cursor::new(buf);
        let total_len = buf.len();
        let mut total_written = 0;

        while cursor.has_remaining() {
            let size = self.stream.write_buf(&mut cursor).await?;
            if size == 0 {
                log::error!(
                    "[WRITER] write: ERROR - wrote 0 bytes with {} bytes remaining",
                    cursor.remaining()
                );
                return Err(EncodeError::More(cursor.remaining()).into());
            }
            total_written += size;
            log::trace!(
                "[WRITER] write: wrote {} bytes (total={}/{}, remaining={})",
                size,
                total_written,
                total_len,
                cursor.remaining()
            );
        }

        log::debug!("[WRITER] write: finished writing {} bytes", total_written);

        Ok(())
    }
}
