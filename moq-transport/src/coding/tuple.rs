//
use super::{Decode, DecodeError, Encode, EncodeError};

/// Tuple Field
#[derive(Clone, Debug, Default)]
pub struct TupleField {
	pub value: Vec<u8>,
}

impl Decode for TupleField {
	fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
		let size = usize::decode(r)?;
		Self::decode_remaining(r, size)?;
		let mut buf = vec![0; size];
		r.copy_to_slice(&mut buf);
		Ok(Self { value: buf })
	}
}

impl Encode for TupleField {
	fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
		self.value.len().encode(w)?;
		Self::encode_remaining(w, self.value.len())?;
		w.put_slice(&self.value);
		Ok(())
	}
}

impl TupleField {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn set<P: Encode>(&mut self, p: P) -> Result<(), EncodeError> {
		let mut value = Vec::new();
		p.encode(&mut value)?;
		self.value = value;
		Ok(())
	}

	pub fn get<P: Decode>(&self) -> Result<P, DecodeError> {
		P::decode(&mut bytes::Bytes::from(self.value.clone()))
	}
}

/// Tuple
#[derive(Clone, Debug, Default)]
pub struct Tuple {
	pub fields: Vec<TupleField>,
}

impl Decode for Tuple {
	fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
		let count = u64::decode(r)? as usize;
		let mut fields = Vec::new();
		for _ in 0..count {
			fields.push(TupleField::decode(r)?);
		}
		Ok(Self { fields })
	}
}

impl Encode for Tuple {
	fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
		self.fields.len().encode(w)?;
		for field in &self.fields {
			field.encode(w)?;
		}
		Ok(())
	}
}

impl Tuple {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn add(&mut self, field: TupleField) {
		self.fields.push(field);
	}

	pub fn get(&self, index: usize) -> Result<TupleField, DecodeError> {
		self.fields[index].get()
	}

	pub fn set(&mut self, index: usize, f: TupleField) -> Result<(), EncodeError> {
		self.fields[index].set(f)
	}

	pub fn clear(&mut self) {
		self.fields.clear();
	}
}
