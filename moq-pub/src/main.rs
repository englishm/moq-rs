use anyhow;
use std::io::{self, Cursor, Read};

fn main() -> anyhow::Result<()> {
	// let atom = read_atom(io::stdin().by_ref());
	//dbg!(&atom);
	let ftyp = read_atom(io::stdin().by_ref())?;
	anyhow::ensure!(&ftyp[4..8] == b"ftyp", "expected ftyp atom");

	let moov = read_atom(io::stdin().by_ref())?;
	anyhow::ensure!(&moov[4..8] == b"moov", "expected moov atom");

	dbg!(&ftyp, &moov);
	let mut c_ftyp = Cursor::new(ftyp);
	let parsed_ftyp = mp4::BoxHeader::read(&mut c_ftyp);
	let mut c_moov = Cursor::new(moov);
	let parsed_moov = mp4::BoxHeader::read(&mut c_moov);
	dbg!(&parsed_ftyp, &parsed_moov);

	Ok(())
}

// Read a full MP4 atom into a vector.
fn read_atom<R: Read>(reader: &mut R) -> anyhow::Result<Vec<u8>> {
	// Read the 8 bytes for the size + type
	let mut buf = [0u8; 8];
	reader.read_exact(&mut buf)?;

	// Convert the first 4 bytes into the size.
	let size = u32::from_be_bytes(buf[0..4].try_into()?) as u64;
	//let typ = &buf[4..8].try_into().ok().unwrap();

	let mut raw = buf.to_vec();

	let mut limit = match size {
		// Runs until the end of the file.
		0 => reader.take(u64::MAX),

		// The next 8 bytes are the extended size to be used instead.
		1 => {
			reader.read_exact(&mut buf)?;
			let size_large = u64::from_be_bytes(buf);
			anyhow::ensure!(size_large >= 16, "impossible extended box size: {}", size_large);

			reader.take(size_large - 16)
		}

		2..=7 => {
			anyhow::bail!("impossible box size: {}", size)
		}

		// Otherwise read based on the size.
		size => reader.take(size - 8),
	};

	// Append to the vector and return it.
	limit.read_to_end(&mut raw)?;

	Ok(raw)
}
