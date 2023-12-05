// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use crate::{
	litep2p::{
		peerstore::{peerstore_handle_test, Peerstore},
		shim::notification::peerset::{
			Direction, PeerState, Peerset, PeersetCommand, PeersetNotificationCommand, Reserved,
		},
	},
	peer_store::PeerStoreProvider,
	protocol_controller::IncomingIndex,
	service::traits::{PeerStore, ValidationResult},
	ProtocolName, ReputationChange,
};

use futures::prelude::*;
use litep2p::protocol::notification::NotificationError;
use rand::{
	distributions::{Distribution, Uniform, WeightedIndex},
	seq::IteratorRandom,
};

use sc_network_types::PeerId;
use sc_utils::mpsc::tracing_unbounded;

use std::{
	collections::{HashMap, HashSet},
	time::Instant,
};

// outbound substream was initiated for a peer but an inbound substream from that same peer
// was receied while the `Peerset` was waiting for the outbound substream to be opened
//
// verify that the peer state is updated correctly
#[tokio::test]
async fn inbound_substream_for_outbound_peer() {
	let peerstore_handle = peerstore_handle_test();
	let peers = vec![PeerId::random(), PeerId::random(), PeerId::random()];
	let inbound_peer = peers.iter().next().unwrap().clone();
	let peerstore = Peerstore::from_handle(peerstore_handle.clone(), peers.clone());

	let (mut peerset, to_peerset) = Peerset::new(
		ProtocolName::from("/notif/1"),
		25,
		25,
		false,
		Default::default(),
		Default::default(),
		peerstore_handle,
	);
	assert_eq!(peerset.num_in(), 0usize);
	assert_eq!(peerset.num_out(), 0usize);

	match peerset.next().await {
		Some(PeersetNotificationCommand::OpenSubstream { peers: out_peers }) => {
			assert_eq!(peerset.num_in(), 0usize);
			assert_eq!(peerset.num_out(), 3usize);
			assert_eq!(
				peerset.peers().get(&inbound_peer),
				Some(&PeerState::Opening { direction: Direction::Outbound(Reserved::No) })
			);
		},
		event => panic!("invalid event: {event:?}"),
	}

	// inbound substream was received from peer who was marked outbound
	//
	// verify that the peer state and inbound/outbound counts are updated correctly
	assert_eq!(peerset.report_inbound_substream(inbound_peer), ValidationResult::Accept);
	assert_eq!(peerset.num_in(), 1usize);
	assert_eq!(peerset.num_out(), 2usize);
	assert_eq!(
		peerset.peers().get(&inbound_peer),
		Some(&PeerState::Opening { direction: Direction::Inbound(Reserved::No) })
	);
}
