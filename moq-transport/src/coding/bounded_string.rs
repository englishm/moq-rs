// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::{Decode, DecodeError, Encode, EncodeError};

macro_rules! bounded_string {
    ($name:ident, $max_len:expr) => {
        #[derive(Clone, Debug, Default, Eq, PartialEq)]
        pub struct $name(pub String);

        impl $name {
            pub const MAX_LEN: usize = $max_len;

            /// Build from a string, truncating to [`MAX_LEN`](Self::MAX_LEN).
            ///
            /// [`Encode`] rejects anything longer, and an encode failure on
            /// the control stream tears down the whole session — so for a
            /// string the local application did not choose (an error's
            /// `Display`, a caller-supplied reason phrase) a shortened value
            /// is strictly better than the alternative. Prefer this over the
            /// tuple constructor anywhere the length is not known statically.
            ///
            /// Truncation lands on a UTF-8 character boundary, so the result
            /// is always a valid prefix of the input.
            pub fn new(value: impl Into<String>) -> Self {
                let mut value = value.into();

                if value.len() > Self::MAX_LEN {
                    // `is_char_boundary(0)` is always true, so this terminates
                    // and cannot underflow; UTF-8 scalars are at most 4 bytes,
                    // so it steps back at most 3 times.
                    let mut end = Self::MAX_LEN;
                    while !value.is_char_boundary(end) {
                        end -= 1;
                    }
                    value.truncate(end);
                }

                Self(value)
            }
        }

        impl Encode for $name {
            fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
                if self.0.len() > Self::MAX_LEN {
                    return Err(EncodeError::FieldBoundsExceeded(
                        stringify!($name).to_string(),
                    ));
                }
                self.0.len().encode(w)?;
                Self::encode_remaining(w, self.0.len())?;
                w.put(self.0.as_ref());
                Ok(())
            }
        }

        impl Decode for $name {
            fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
                let size = usize::decode(r)?;
                if size > Self::MAX_LEN {
                    return Err(DecodeError::FieldBoundsExceeded(
                        stringify!($name).to_string(),
                    ));
                }
                Self::decode_remaining(r, size)?;
                let mut buf = vec![0; size];
                r.copy_to_slice(&mut buf);
                Ok($name(String::from_utf8(buf)?))
            }
        }
    };
}

// Implementations of bounded strings
bounded_string!(ReasonPhrase, 1024);
bounded_string!(SessionUri, 8192);

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use bytes::BytesMut;

    #[test]
    fn encode_decode() {
        let mut buf = BytesMut::new();

        let r = ReasonPhrase("testreason".to_string());
        r.encode(&mut buf).unwrap();
        assert_eq!(
            buf.to_vec(),
            vec![
                0x0a, // Length of "testreason" is 10
                0x74, 0x65, 0x73, 0x74, 0x72, 0x65, 0x61, 0x73, 0x6f, 0x6e
            ]
        );
        let decoded = ReasonPhrase::decode(&mut buf).unwrap();
        assert_eq!(decoded, r);
    }

    #[test]
    fn encode_too_large() {
        let mut buf = BytesMut::new();

        let r = ReasonPhrase("x".repeat(1025));
        let encoded = r.encode(&mut buf);
        assert!(matches!(
            encoded.unwrap_err(),
            EncodeError::FieldBoundsExceeded(_)
        ));
    }

    #[test]
    fn decode_too_large() {
        let mut data: Vec<u8> = vec![0x00; 1025]; // Create a vector with 1025 bytes
                                                  // Set first 2 bytes as length of 1025 as a VarInt
        data[0] = 0x44;
        data[1] = 0x01;
        let mut buf: Bytes = data.into();
        let decoded = ReasonPhrase::decode(&mut buf);
        assert!(matches!(
            decoded.unwrap_err(),
            DecodeError::FieldBoundsExceeded(_)
        ));
    }

    #[test]
    fn new_leaves_a_string_within_the_bound_untouched() {
        assert_eq!(ReasonPhrase::new("unauthorized").0, "unauthorized");
        assert_eq!(ReasonPhrase::new("").0, "");

        let exact = "x".repeat(ReasonPhrase::MAX_LEN);
        assert_eq!(ReasonPhrase::new(exact.clone()).0, exact);
    }

    /// Truncation must land on a character boundary, whatever width the
    /// characters are and wherever the limit falls inside one.
    #[test]
    fn new_truncates_on_a_char_boundary() {
        // 1, 2, 3 and 4 byte scalars, each padded so the limit lands at every
        // offset within a character.
        for filler in ["a", "\u{80}", "\u{800}", "\u{10000}", "\u{10FFFF}"] {
            for pad in 0..8 {
                let mut input = "p".repeat(pad);
                while input.len() < ReasonPhrase::MAX_LEN * 2 {
                    input.push_str(filler);
                }

                let bounded = ReasonPhrase::new(input.clone());

                assert!(
                    bounded.0.len() <= ReasonPhrase::MAX_LEN,
                    "filler {filler:?} pad {pad}: {} bytes",
                    bounded.0.len()
                );
                assert!(
                    input.starts_with(&bounded.0),
                    "filler {filler:?} pad {pad}: not a prefix"
                );
                // Backtracking is bounded by the widest UTF-8 scalar.
                assert!(bounded.0.len() + 4 > ReasonPhrase::MAX_LEN);

                // The point of the bound: it always encodes.
                let mut buf = Vec::new();
                bounded.encode(&mut buf).expect("must encode");
            }
        }
    }

    /// The bound is in bytes, matching what `Encode` checks and what
    /// draft-16 §1.4.3 specifies.
    #[test]
    fn new_bounds_bytes_not_characters() {
        let wide = "\u{10FFFF}".repeat(ReasonPhrase::MAX_LEN);
        let bounded = ReasonPhrase::new(wide);

        assert!(bounded.0.len() <= ReasonPhrase::MAX_LEN);
        assert!(bounded.0.chars().count() < ReasonPhrase::MAX_LEN);
    }
}
