use super::{Versions};
use crate::coding::{Decode, DecodeError, Encode, EncodeError, KeyValuePairs, VarInt};

/// Sent by the client to setup the session.
/// This CLIENT_SETUP message is used by moq-transport draft versions 11 and later.
/// Id = 0x20 vs 0x40 for versions <= 10.
#[derive(Debug)]
pub struct Client {
    /// The list of supported versions in preferred order.
    pub versions: Versions,

    /// Setup Parameters, ie: PATH, MAX_REQUEST_ID,
    /// MAX_AUTH_TOKEN_CACHE_SIZE, AUTHORIZATION_TOKEN, etc.
    pub params: KeyValuePairs,
}

impl Decode for Client {
    /// Decode a client setup message.
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let typ = VarInt::decode(r)?;
        if typ.into_inner() != 0x20 {   // CLIENT_SETUP message ID for draft versions 11 and later
            return Err(DecodeError::InvalidMessage(typ.into_inner()));
        }

        let _len = u16::decode(r)?;
        // TODO: Check the length of the message.

        let versions = Versions::decode(r)?;
        let params = KeyValuePairs::decode(r)?;

        Ok(Self {
            versions,
            params,
        })
    }
}

impl Encode for Client {
    /// Encode a server setup message.
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        VarInt::from_u32(0x20).encode(w)?; // CLIENT_SETUP message ID for draft versions 11 and later

        // Find out the length of the message
        // by encoding it into a buffer and then encoding the length.
        // This is a bit wasteful, but it's the only way to know the length.
        // TODO SLG - perhaps we can store the position of the Length field in the BufMut and
        //       write the length later, to avoid the copy of the message bytes?
        let mut buf = Vec::new();

        self.versions.encode(&mut buf).unwrap();
        self.params.encode(&mut buf).unwrap();

        // Make sure buf.len() <= u16::MAX
        if buf.len() > u16::MAX as usize {
            return Err(EncodeError::MsgBoundsExceeded);
        }
        (buf.len() as u16).encode(w)?;

        // At least don't encode the message twice.
        // Instead, write the buffer directly to the writer.
        w.put_slice(&buf);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::Version;
    use bytes::BytesMut;

    #[test]
    fn encode_decode() {
        let mut buf = BytesMut::new();

        let mut params = KeyValuePairs::default();
        params.set_bytesvalue(0x01 /* Path */, "testpath".as_bytes().to_vec());

        let client = Client {
            versions: [Version::DRAFT_13].into(),
            params,
        };
        client.encode(&mut buf).unwrap();
        assert_eq!(
            buf.to_vec(),
            vec![
                0x20,       // Type
                0x00, 0x14, // Length
                0x01,       // 1 Version
                0xC0, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x0D,   // Version DRAFT_13 (0xff00000D)
                0x01,       // 1 Param
                0x01, 0x08, 0x74, 0x65, 0x73, 0x74, 0x70, 0x61, 0x74, 0x68, // Key=1 (Path), Value="testpath"
            ]
        );
        let decoded = Client::decode(&mut buf).unwrap();
        assert_eq!(decoded.versions, client.versions);
        assert_eq!(decoded.params, client.params);
    }
}
