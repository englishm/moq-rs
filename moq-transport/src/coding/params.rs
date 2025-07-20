use std::collections::HashMap;
use std::io::Cursor;

use crate::coding::{Decode, DecodeError, Encode, EncodeError, VarInt};

#[derive(Default, Debug, Clone)]
pub struct Params(pub HashMap<u64, Vec<u8>>);

impl Decode for Params {
    fn decode<R: bytes::Buf>(mut r: &mut R) -> Result<Self, DecodeError> {
        let mut params = HashMap::new();

        // I hate this encoding so much; let me encode my role and get on with my life.
        let count = u64::decode(r)?;
        for _ in 0..count {
            let kind = u64::decode(r)?;

            // Duplicate parameters are not allowed unless they are explicitly said to be (e.g. Auth parameters)
            // https://www.ietf.org/archive/id/draft-ietf-moq-transport-12.html#section-8.2-2
            // https://www.ietf.org/archive/id/draft-ietf-moq-transport-12.html#section-8.2.1.1-2
            if params.contains_key(&kind) && kind != 0x3 {
                return Err(DecodeError::DupliateParameter);
            }

            if kind % 2 == 0 {
                // Even types are VarInts without length prefix
                let value = VarInt::decode(r)?;

                // Re-encode value into a Vec<u8> for storage
                let mut buf = Vec::new();
                value.encode(&mut buf)?;
                params.insert(kind, buf);
            } else {
                // Odd types are length-prefixed byte arrays
                let size = usize::decode(&mut r)?;
                Self::decode_remaining(r, size)?;

                const MAX_PARAM_SIZE: usize = 8192; // TODO: Make this configurable
                if size > MAX_PARAM_SIZE {
                    return Err(DecodeError::InvalidParameter);
                }
                let mut buf = vec![0; size];
                r.copy_to_slice(&mut buf);
                params.insert(kind, buf);
            }
        }

        Ok(Params(params))
    }
}

impl Encode for Params {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.0.len().encode(w)?;

        for (kind, value) in self.0.iter() {
            kind.encode(w)?;

            if kind % 2 == 0 {
                // Even types are VarInts with no length prefix
                // https://www.ietf.org/archive/id/draft-ietf-moq-transport-12.html#section-1.3.2
                let varint = VarInt::decode(&mut &value[..])
                    .map_err(|e| EncodeError::Decode(e.to_string()))?;

                varint.encode(w)?;
            } else {
                // Odd types are length-prefixed byte arrays
                (value.len() as u64).encode(w)?;
                w.put_slice(value);
            }
        }

        Ok(())
    }
}

impl Params {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set<P: Encode>(&mut self, kind: u64, p: P) -> Result<(), EncodeError> {
        let mut value = Vec::new();
        p.encode(&mut value)?;
        self.0.insert(kind, value);

        Ok(())
    }

    pub fn has(&self, kind: u64) -> bool {
        self.0.contains_key(&kind)
    }

    pub fn get<P: Decode>(&mut self, kind: u64) -> Result<Option<P>, DecodeError> {
        if let Some(value) = self.0.remove(&kind) {
            if kind % 2 == 0 {
                // Even types are VarInts with no length prefix, decode directly
                Ok(Some(P::decode(&mut &value[..])?))
            } else {
                // Odd types are length-prefixed byte arrays
                let mut cursor = Cursor::new(value);
                Ok(Some(P::decode(&mut cursor)?))
            }
        } else {
            Ok(None)
        }
    }
}
