// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::{Publisher, SessionError};
use crate::coding::{KeyValuePairs, ReasonPhrase};
use crate::message;
use crate::message::RequestOk;
use crate::serve;

pub struct TrackStatusRequested {
    publisher: Publisher,
    pub request_msg: message::TrackStatus,
}

impl TrackStatusRequested {
    pub fn new(publisher: Publisher, request_msg: message::TrackStatus) -> Self {
        Self {
            publisher,
            request_msg,
        }
    }

    /// Reject the TRACK_STATUS request with REQUEST_ERROR (draft-16 §9.8).
    pub fn respond_error(
        &mut self,
        error_code: u64,
        error_message: &str,
    ) -> Result<(), SessionError> {
        self.publisher.send_request_error(
            "track_status",
            message::RequestError {
                id: self.request_msg.id,
                error_code,
                retry_interval: 0,
                reason: ReasonPhrase::new(error_message),
            },
        );
        Ok(())
    }

    /// Accept the TRACK_STATUS request with REQUEST_OK (draft-16 §9.7).
    ///
    /// The response includes LARGEST_OBJECT when objects have been published.
    /// No Track Alias is included — draft-16 §9.19 does not use one for
    /// TRACK_STATUS responses.
    pub fn respond_ok(mut self, track: &serve::TrackReader) -> Result<(), SessionError> {
        let mut params = KeyValuePairs::default();

        if let Some(largest) = track.largest_location() {
            params
                .set_largest_object(largest)
                .map_err(|_| SessionError::Internal)?;
        }

        self.publisher.send_request_ok(
            "track_status",
            RequestOk {
                id: self.request_msg.id,
                params,
            },
        );

        Ok(())
    }
}
