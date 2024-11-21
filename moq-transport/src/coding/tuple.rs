//
use super::{Decode, DecodeError, Encode, EncodeError};
use core::hash::{Hash, Hasher};
/// Tuple Field
#[derive(Clone, Debug, Default)]
pub struct TupleField {
	pub value: Vec<u8>,
}

impl Eq for TupleField {}

impl PartialEq for TupleField {
	fn eq(&self, other: &Self) -> bool {
		self.value.eq(&other.value)
	}
}

impl Hash for TupleField {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.value.hash(state);
	}
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

	pub fn from_utf8(path: &str) -> Self {
		let mut field = TupleField::new();
		field.value = path.as_bytes().to_vec();
		field
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

impl Hash for Tuple {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.fields.hash(state);
	}
}

impl Eq for Tuple {}

impl PartialEq for Tuple {
	fn eq(&self, other: &Self) -> bool {
		self.fields.eq(&other.fields)
	}
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

	pub fn from_utf8_path(path: &str) -> Self {
		let mut tuple = Tuple::new();
		for part in path.split('/') {
			tuple.add(TupleField::from_utf8(part));
		}
		tuple
	}

	pub fn to_utf8_path(&self) -> String {
		let mut path = String::new();
		for field in &self.fields {
			path.push('/');
			path.push_str(&String::from_utf8_lossy(&field.value));
		}
		path
	}
}
