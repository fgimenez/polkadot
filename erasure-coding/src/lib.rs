// Copyright 2018 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! As part of Polkadot's availability system, certain pieces of data
//! for each block are required to be kept available.
//!
//! The way we accomplish this is by erasure coding the data into n pieces
//! and constructing a merkle root of the data.
//!
//! Each of n validators stores their piece of data. We assume n=3f+k, k < 3.
//! f is the maximum number of faulty vaildators in the system.
//! The data is coded so any f+1 chunks can be used to reconstruct the full data.

extern crate polkadot_primitives as primitives;
extern crate reed_solomon_erasure as reed_solomon;
extern crate parity_codec as codec;
extern crate substrate_primitives;
extern crate substrate_trie as trie;

use codec::{Encode, Decode};
use reed_solomon::ReedSolomon;
use primitives::{Hash as H256, BlakeTwo256, HashT};
use primitives::parachain::{BlockData, Extrinsic};
use substrate_primitives::Blake2Hasher;
use trie::{MemoryDB, Trie, TrieMut, TrieDB, TrieDBMut};

// unfortunate requirement due to use of GF(256) in the reed-solomon
// implementation.
const MAX_VALIDATORS: usize = 256;

/// Errors in erasure coding.
#[derive(Debug, Clone)]
pub enum Error {
	/// Returned when there are too many validators.
	TooManyValidators,
	/// Cannot encode something for no validators
	EmptyValidators,
	/// Cannot reconstruct: wrong number of validators.
	WrongValidatorCount,
	/// Not enough chunks present.
	NotEnoughChunks,
	/// Too many chunks present.
	TooManyChunks,
	/// Chunks not of uniform length or the chunks are empty.
	NonUniformChunks,
	/// Chunk index out of bounds.
	ChunkIndexOutOfBounds(usize, usize),
	/// Bad payload in reconstructed bytes.
	BadPayload,
}

struct CodeParams {
	data_shards: usize,
	parity_shards: usize,
}

impl CodeParams {
	// the shard length needed for a payload with initial size `base_len`.
	fn shard_len(&self, base_len: usize) -> usize {
		(base_len / self.data_shards) + (base_len % self.data_shards)
	}

	fn make_shards_for(&self, payload: &[u8]) -> Vec<Box<[u8]>> {
		let shard_len = self.shard_len(payload.len());
		let mut shards = reed_solomon::make_blank_shards(
			shard_len,
			self.data_shards + self.parity_shards,
		);
		for (data_chunk, blank_shard) in payload.chunks(shard_len).zip(&mut shards) {
			let len = ::std::cmp::min(data_chunk.len(), blank_shard.len());

			// fill the empty shards with the corresponding piece of the payload,
			// zero-padded to fit in the shards.
			blank_shard[..len].copy_from_slice(&data_chunk[..len]);
		}

		shards
	}

	// make a reed-solomon instance.
	fn make_encoder(&self) -> ReedSolomon {
		ReedSolomon::new(self.data_shards, self.parity_shards)
			.expect("this struct is not created with invalid shard number; qed")
	}
}

fn code_params(n_validators: usize) -> Result<CodeParams, Error> {
	if n_validators > MAX_VALIDATORS { return Err(Error::TooManyValidators) }
	if n_validators == 0 { return Err(Error::EmptyValidators) }

	let n_faulty = n_validators.saturating_sub(1) / 3;
	let n_good = n_validators - n_faulty;

	Ok(CodeParams {
		data_shards: n_faulty + 1,
		parity_shards: n_good - 1,
	})
}

/// Obtain erasure-coded chunks, one for each validator.
///
/// Works only up to 256 validators, and `n_validators` must be non-zero.
pub fn obtain_chunks(n_validators: usize, block_data: &BlockData, extrinsic: &Extrinsic)
	-> Result<Vec<Box<[u8]>>, Error>
{
	let params  = code_params(n_validators)?;
	let encoded = (block_data, extrinsic).encode();

	if encoded.is_empty() {
		return Err(Error::BadPayload);
	}

	let mut shards = params.make_shards_for(&encoded[..]);
	params.make_encoder().encode_shards(&mut shards[..])
		.expect("Payload non-empty, shard sizes are uniform, and validator numbers checked; qed");

	Ok(shards)
}

/// Reconstruct the block data from a set of chunks.
///
/// Provide an iterator containing chunk data and the corresponding index.
/// The indices of the present chunks must be indicated. If too few chunks
/// are provided, recovery is not possible.
///
/// Works only up to 256 validators, and `n_validators` must be non-zero.
pub fn reconstruct<'a, I: 'a>(n_validators: usize, chunks: I)
	-> Result<(BlockData, Extrinsic), Error>
	where I: IntoIterator<Item=(&'a [u8], usize)>
{
	let params = code_params(n_validators)?;
	let mut shards: Vec<Option<Box<[u8]>>> = vec![None; n_validators];
	let mut shard_len = None;
	for (chunk_data, chunk_idx) in chunks.into_iter().take(n_validators) {
		if chunk_idx >= n_validators {
			return Err(Error::ChunkIndexOutOfBounds(chunk_idx, n_validators));
		}

		let shard_len = shard_len.get_or_insert_with(|| chunk_data.len());

		if *shard_len != chunk_data.len() || *shard_len == 0 {
			return Err(Error::NonUniformChunks);
		}

		shards[chunk_idx] = Some(chunk_data.to_vec().into_boxed_slice());
	}

	if let Err(e) = params.make_encoder().reconstruct_shards(&mut shards[..]) {
		match e {
			reed_solomon::Error::TooFewShardsPresent => Err(Error::NotEnoughChunks)?,
			reed_solomon::Error::InvalidShardFlags => Err(Error::WrongValidatorCount)?,
			reed_solomon::Error::TooManyShards => Err(Error::TooManyChunks)?,
			reed_solomon::Error::EmptyShard => panic!("chunks are all non-empty; this is checked above; qed"),
			reed_solomon::Error::IncorrectShardSize => panic!("chunks are all same len; this is checked above; qed"),
			_ => panic!("reed_solomon encoder returns no more variants for this function; qed"),
		}
	}

	Decode::decode(&mut ShardInput {
		shards,
		data_shards: params.data_shards,
		cursor: (0, 0),
	}).ok_or_else(|| Error::BadPayload)
}

/// An iterator that yields merkle branches and chunk data for all chunks to
/// be sent to other validators.
pub struct Branches<'a> {
	trie_storage: MemoryDB<Blake2Hasher>,
	root: H256,
	chunks: Vec<&'a [u8]>,
	current_pos: usize,
}

impl<'a> Branches<'a> {
	/// Get the trie root.
	pub fn root(&self) -> H256 { self.root.clone() }
}

impl<'a> Iterator for Branches<'a> {
	type Item = (Vec<Vec<u8>>, &'a [u8]);

	fn next(&mut self) -> Option<Self::Item> {
		use trie::Recorder;

		let trie = TrieDB::new(&self.trie_storage, &self.root)
			.expect("`Branches` is only created with a valid memorydb that contains all nodes for the trie with given root; qed");

		let mut recorder = Recorder::new();
		let res = (self.current_pos as u32).using_encoded(|s|
			trie.get_with(s, &mut recorder)
		);

		match res.expect("all nodes in trie present; qed") {
			Some(_) => {
				let nodes = recorder.drain().into_iter().map(|r| r.data).collect();
				let chunk = &self.chunks.get(self.current_pos)
					.expect("there is a one-to-one mapping of chunks to valid merkle branches; qed");

				self.current_pos += 1;
				Some((nodes, chunk))
			}
			None => None,
		}
	}
}

/// Construct a trie from chunks of an erasure-coded value. This returns the root hash and an
/// iterator of merkle proofs, one for each validator.
pub fn branches<'a>(chunks: Vec<&'a [u8]>) -> Branches<'a> {
	let mut trie_storage: MemoryDB<Blake2Hasher> = MemoryDB::default();
	let mut root = H256::default();

	// construct trie mapping each chunk's index to its hash.
	{
		let mut trie = TrieDBMut::new(&mut trie_storage, &mut root);
		for (i, &chunk) in chunks.iter().enumerate() {
			(i as u32).using_encoded(|encoded_index| {
				let chunk_hash = BlakeTwo256::hash(chunk);
				trie.insert(encoded_index, chunk_hash.as_ref())
					.expect("a fresh trie stored in memory cannot have errors loading nodes; qed");
			})
		}
	}

	Branches {
		trie_storage,
		root,
		chunks,
		current_pos: 0,
	}
}

// input for `parity_codec` which draws data from the data shards
struct ShardInput {
	shards: Vec<Option<Box<[u8]>>>,
	data_shards: usize,
	cursor: (usize, usize), // shard, in_shard
}

impl codec::Input for ShardInput {
	fn read(&mut self, into: &mut [u8]) -> usize {
		let &mut (ref mut shard_idx, ref mut in_shard) = &mut self.cursor;
		let mut read_bytes = 0;

		while *shard_idx < self.data_shards {
			if read_bytes == into.len() { break }

			let active_shard = self.shards[*shard_idx]
				.as_ref()
				.expect("data shards have been recovered; qed");

			if *in_shard >= active_shard.len() {
				*shard_idx += 1;
				*in_shard = 0;
				continue;
			}

			let remaining_len_out = into.len() - read_bytes;
			let remaining_len_shard = active_shard.len() - *in_shard;

			let write_len = std::cmp::min(remaining_len_out, remaining_len_shard);
			into[read_bytes..][..write_len]
				.copy_from_slice(&active_shard[*in_shard..][..write_len]);

			*in_shard += write_len;
			read_bytes += write_len;
		}

		read_bytes
	}
}

#[cfg(test)]
mod tests {
	use super::*;

    #[test]
	fn round_trip_block_data() {
		let block_data = BlockData(vec![1; 256]);
		let chunks = obtain_chunks(10, &block_data, &Extrinsic).unwrap();

		assert_eq!(chunks.len(), 10);

		// any 4 chunks should work.
		let reconstructed = reconstruct(
			10,
			[
				(&*chunks[1], 1),
				(&*chunks[4], 4),
				(&*chunks[6], 6),
				(&*chunks[9], 9),
			].iter().cloned(),
		).unwrap();



		assert_eq!(reconstructed, (block_data, Extrinsic));
	}
}
