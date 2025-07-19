use super::Versions;
use crate::coding::{Decode, DecodeError, Encode, EncodeError, Params};

/// Sent by the client to setup the session.
// NOTE: This is not a message type, but rather the control stream header.
// Proposal: https://github.com/moq-wg/moq-transport/issues/138
#[derive(Debug)]
pub struct Client {
    /// The list of supported versions in preferred order.
    pub versions: Versions,

    /// Unknown parameters.
    pub params: Params,
}

impl Decode for Client {
    /// Decode a client setup message.
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let typ = u64::decode(r)?;
        if typ != 0x20 {
            return Err(DecodeError::InvalidMessage(typ));
        }

        let _len = u64::decode(r)?;

        // TODO: Check the length of the message.

        let versions = Versions::decode(r)?;
        let params = Params::decode(r)?;

        // Make sure the PATH parameter isn't used
        // TODO: This assumes WebTransport support only
        if params.has(1) {
            return Err(DecodeError::InvalidParameter);
        }

        Ok(Self { versions, params })
    }
}

impl Encode for Client {
    /// Encode a server setup message.
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        0x20_u64.encode(w)?;

        // Find out the length of the message
        // by encoding it into a buffer and then encoding the length.
        // This is a bit wasteful, but it's the only way to know the length.
        let mut buf = Vec::new();

        self.versions.encode(&mut buf).unwrap();

        self.params.encode(&mut buf).unwrap();

        (buf.len() as u64).encode(w)?;

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
        let client = Client {
            versions: [Version::DRAFT_12].into(),
            params: Params::default(),
        };

        client.encode(&mut buf).unwrap();
        assert_eq!(
            buf.to_vec(),
            vec![
                0x20, 0x0A, 0x01, 0xC0, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x0C, 0x00
            ]
        );

        let decoded = Client::decode(&mut buf).unwrap();
        assert_eq!(decoded.versions, client.versions);
        //assert_eq!(decoded.params, client.params);
    }
}
