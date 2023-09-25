// This file is part of Substrate.
//
// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.
//
// If you read this, you are very thorough, congratulations.

//! Signature-related code

// TODO(aaro): remove
#![allow(unused)]
#![allow(missing_docs)]

use sc_network_types::PeerId;

pub use libp2p::identity::SigningError;

pub enum PublicKey {
	Libp2p(libp2p::identity::PublicKey),
	Litep2p(litep2p::crypto::PublicKey),
}

impl PublicKey {
	pub fn encode_protobuf(&self) -> Vec<u8> {
		match self {
			Self::Libp2p(public) => public.encode_protobuf(),
			Self::Litep2p(public) => public.to_protobuf_encoding(),
		}
	}

	// pub fn to_peer_id(&self) -> PeerId {
	// 	match self {
	// 		Self::Libp2p(public) => public.to_peer_id().into(),
	// 		Self::Litep2p(public) => public.to_peer_id().into(),
	// 	}
	// }

	// pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
	// 	match self {
	// 		Self::Libp2p(public) => public.verify(message, signature),
	// 		Self::Litep2p(public) => public.verify(message, signature),
	// 	}
	// }
}

/// A result of signing a message with a network identity. Since `PeerId` is potentially a hash of a
/// `PublicKey`, you need to reveal the `PublicKey` next to the signature, so the verifier can check
/// if the signature was made by the entity that controls a given `PeerId`.
pub struct Signature {
	/// The public key derived from the network identity that signed the message.
	pub public_key: PublicKey,

	/// The actual signature made for the message signed.
	pub bytes: Vec<u8>,
}

impl Signature {
	pub fn new(public_key: PublicKey, bytes: Vec<u8>) -> Self {
		Self { public_key, bytes }
	}

	/// Create a signature for a message with a given network identity.
	pub(crate) fn sign_message(
		message: impl AsRef<[u8]>,
		keypair: &libp2p::identity::Keypair,
	) -> Result<Self, SigningError> {
		let public_key = keypair.public();
		let bytes = keypair.sign(message.as_ref())?;
		todo!();
		// Ok(Self { public_key, bytes })
	}

	// /// Verify whether the signature was made for the given message by the entity that controls
	// the /// given `PeerId`.
	// pub fn verify(&self, message: impl AsRef<[u8]>, peer_id: &PeerId) -> bool {
	// 	*peer_id == self.public_key.to_peer_id() &&
	// 		self.public_key.verify(message.as_ref(), &self.bytes)
	// }
}
