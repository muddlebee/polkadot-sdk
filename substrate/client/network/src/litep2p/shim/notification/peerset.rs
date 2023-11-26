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

//! `Peerset` implementation for `litep2p`.
//!
//! `Peerset` is a separate but related component running alongside the notification protocol,
//! responsible for maintaining connectivity to remote peers. `Peerset` has an imperfect view of the
//! network as the notification protocol is behind an asynchronous channel. Based on this imperfect
//! view, it tries to connect to remote peers and disconnect peers that should be disconnected from.
//!
//! It is these state transitions that are

//! `Peerset` deals with two types of peers:
//!  - regular pers
//!  - reserved peers
//!
//! Each protocol

use crate::{
	litep2p::peerstore::PeerstoreHandle,
	service::traits::{self, ValidationResult},
	ProtocolName,
};

use futures::{channel::oneshot, future::BoxFuture, stream::FuturesUnordered, Stream, StreamExt};
use futures_timer::Delay;

use litep2p::protocol::notification::NotificationError;
use sc_network_types::PeerId;
use sc_utils::mpsc::{tracing_unbounded, TracingUnboundedReceiver, TracingUnboundedSender};

use std::{
	collections::{hash_map::Entry, HashMap, HashSet},
	pin::Pin,
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	},
	task::{Context, Poll, Waker},
	time::Duration,
};

// TODO: should reserved set updates be atomic?
// TODO: store disconnected peers in a separate LRU map so they're faster to deal with

/// Logging target for the file.
const LOG_TARGET: &str = "sub-libp2p::peerset";

/// Default backoff for connection re-attempts.
const DEFAULT_BACKOFF: Duration = Duration::from_secs(30);

/// Reputation adjustment when a peer gets disconnected.
///
/// Lessens the likelyhood of the peer getting selected for an outbound connection soon.
const DISCONNECT_ADJUSTMENT: i32 = -256;

/// Reputation adjustment when a substream fails to open.
///
/// Lessens the likelyhood of the peer getting selected for an outbound connection soon.
const OPEN_FAILURE_ADJUSTMENT: i32 = -256;

/// Is the peer reserved?
#[derive(Debug, Copy, Clone)]
enum Reserved {
	Yes,
	No,
}

impl From<bool> for Reserved {
	fn from(value: bool) -> Reserved {
		match value {
			true => Reserved::Yes,
			false => Reserved::No,
		}
	}
}

impl From<Reserved> for bool {
	fn from(value: Reserved) -> bool {
		std::matches!(value, Reserved::Yes)
	}
}

#[derive(Debug, Copy, Clone)]
enum Direction {
	/// Inbound substream.
	Inbound(Reserved),

	/// Outbound substream.
	Outbound(Reserved),
}

/// Commands emitted by other subsystems of the blockchain to [`Peerset`].
#[derive(Debug)]
pub enum PeersetCommand {
	/// Set current reserved peer set.
	///
	/// This command removes all reserved peers that are not in `peers`.
	SetReservedPeers {
		/// New seserved peer set.
		peers: HashSet<PeerId>,
	},

	/// Add one or more reserved peers.
	///
	/// This command doesn't remove any reserved peers but only add new peers.
	AddReservedPeers {
		/// Reserved peers to add.
		peers: HashSet<PeerId>,
	},

	/// Remove reserved peers.
	RemoveReservedPeers {
		/// Reserved peers to remove.
		peers: HashSet<PeerId>,
	},

	/// Set reserved-only mode to true/false.
	SetReservedOnly {
		/// Should the protocol only accept/establish connections to reserved peers.
		reserved_only: bool,
	},

	/// Disconnect peer.
	DisconnectPeer {
		/// Peer ID.
		peer: PeerId,
	},

	/// Get reserved peers.
	GetReservedPeers {
		/// `oneshot::Sender` for sending the current set of reserved peers.
		tx: oneshot::Sender<Vec<PeerId>>,
	},
}

/// Commands emitted by [`Peerset`] to the notification protocol.
#[derive(Debug)]
pub enum PeersetNotificationCommand {
	/// Open substreams to one or more peers.
	OpenSubstream {
		/// Peer IDs.
		peers: Vec<PeerId>,
	},

	/// Close substream to one or more peers.
	CloseSubstream {
		/// Peer IDs.
		peers: Vec<PeerId>,
	},
}

/// Peer state.
///
/// Peer can be in 4 different state:
///  - disconnected
///  - connected
///  - connection is opening
///  - connection is closing
///  - connection is backed-off
///
/// Opening and closing are separate states as litep2p guarantees to report when the substream is
/// either fully open or fully closed and the slot allocation for opening a substream is tied to a
/// state transition which moves the peer to [`PeerState::Opening`]. This is because it allows
/// reserving a slot for peer to prevent infinite outbound substreams. If the substream is opened
/// successfully, peer is moved to state [`PeerState::Open`] but there is no modificatoin to the
/// slot count as an outbound slot was already allocated for the peer. If the substream fails to
/// open, the event is reported by litep2p and [`Peerset::report_substream_open_failure()`] is
/// called which will decrease the outbound slot count. Similarly for inbound streams, the slot is
/// allocated in [`Peerset::report_inbound_substream()`] which will prevent `Peerset` from accepting
/// infinite inbound substreams. If the inbound substream fails to open and since [`Peerset`] was
/// notified of it, litep2p will report the open failure and the inbound slot count is once again
/// decreased in [`Peerset::report_substream_open_failure()`]. If the substream is opened
/// successfully, the slot count is not modified.
///
/// Since closing a substream is not instantaneous, there is a separate [`PeersState::Closing`]
/// state which indicates that the substream is being closed but hasn't been closed by litep2p yet.
/// This state is used to prevent invalid state transitions where, for example, [`Peerset`] would
/// close a substream and then try to reopen it immediately.
///
/// Irrespective of which side closed the substream (local/remote), the substream is chilled for a
/// small amount of time ([`DEFAULT_BACKOFF`]) and during this time no inbound or outbound
/// substreams are accepted/established. Inbound subtreams are completely rejected and outbound
/// substream requests are put on hold. Once the backoff expires and if the local node is still
/// interested in opening an outbound substream (the request hasn't been canceled), outbound
/// substream request is made to litep2p.
///
/// Disconnections and open failures will contribute negatively to the peer score to prevent it from
/// being selected for another outbound substream request soon after the failure/disconnection. The
/// reputation decays towards zero over time and eventually the peer will be as likely to be
/// selected for an outbound substream as any other freshly added peer.
// TODO(aaro): actually implement what is specified here
// TODO(aaro): add lots of tests for different state transitions.
#[derive(Debug)]
enum PeerState {
	/// No active connection to peer.
	Disconnected,

	/// Substream to peer was recently closed and the peer is currently backed off.
	///
	/// Backoff only applies to outbound substreams. Inbound substream will not experience any sort
	/// of "banning" even if the peer is backed off and an inbound substream for the peer is
	/// received.
	Backoff,

	/// Connection to peer is pending.
	Opening {
		/// Direction of the connection.
		direction: Direction,
	},

	// Connected to peer.
	Connected {
		/// Is the peer inbound or outbound.
		direction: Direction,
	},

	/// Substream was opened and while it was opening (no response had been heard from litep2p),
	/// the substream was canceled by either calling `disconnect_peer()` or by removing peer
	/// from the reserved set.
	///
	/// After the opened substream is acknowledged by litep2p (open success/failure), the peer is
	/// moved to [`PeerState::Backoff`] from which it will then be moved to
	/// [`PeerState::Disconnected`].
	Canceled {
		/// Is the peer inbound or outbound.
		direction: Direction,
	},

	/// Connection to peer is closing.
	///
	/// State implies that the substream was asked to be closed by the local node and litep2p is
	/// closing the substream. No command modifying the connection state is accepted until the
	/// state has been set to [`PeerState::Disconnected`].
	Closing {
		/// Is the peer inbound or outbound.
		direction: Direction,
	},
}

/// Peer context.
#[derive(Debug)]
struct PeerContext {
	/// Is the peer a reserved peer.
	is_reserved: bool,

	/// Peer state.
	state: PeerState,
}

/// `Peerset` implementation.
///
/// `Peerset` allows other subsystems of the blockchain to modify the connection state
/// of the notification protocol by adding and removing reserved peers.
///
/// `Peerset` is also responsible for maintaining the desired amount of peers the protocol is
/// connected to by establishing outbound connections and accepting/rejecting inbound connections.
#[derive(Debug)]
pub struct Peerset {
	/// Protocol name.
	protocol: ProtocolName,

	/// RX channel for receiving commands.
	cmd_rx: TracingUnboundedReceiver<PeersetCommand>,

	/// Maximum number of outbound peers.
	max_out: usize,

	/// Current number of outbound peers.
	num_out: usize,

	/// Maximum number of inbound peers.
	max_in: usize,

	/// Current number of inbound peers.
	num_in: usize,

	/// Only connect to/accept connections from reserved peers.
	reserved_only: bool,

	/// Current reserved peer set.
	reserved_peers: HashSet<PeerId>,

	/// Handle to `Peerstore`.
	peerstore_handle: PeerstoreHandle,

	/// Peers.
	peers: HashMap<PeerId, PeerState>,

	/// Counter connected peers.
	connected_peers: Arc<AtomicUsize>,

	/// Pending backoffs for peers who recently disconnected.
	pending_backoffs: FuturesUnordered<BoxFuture<'static, PeerId>>,

	waker: Option<Waker>,

	iteration: usize,
}

macro_rules! adjust_or_warn {
    ($slot:expr, $protocol:expr, $peer:expr, $direction:expr) => {{
		match $slot.checked_sub(1) {
			Some(value) => {
				$slot = value;
			}
			None => {
				log::warn!(
					target: LOG_TARGET,
					"{}: state mismatch, {:?} is not counted as part of {:?} slots",
					$protocol, $peer, $direction
				);
			}
		}
    }};
}

impl Peerset {
	/// Create new [`Peerset`].
	pub fn new(
		protocol: ProtocolName,
		max_out: usize,
		max_in: usize,
		reserved_only: bool,
		reserved_peers: HashSet<PeerId>,
		connected_peers: Arc<AtomicUsize>,
		mut peerstore_handle: PeerstoreHandle,
	) -> (Self, TracingUnboundedSender<PeersetCommand>) {
		let (cmd_tx, cmd_rx) = tracing_unbounded("mpsc-peerset-protocol", 100_000);
		let peers = reserved_peers
			.iter()
			.map(|peer| (*peer, PeerState::Disconnected))
			.collect::<HashMap<_, _>>();

		// register protocol's commad channel to `Peerstore` so it can issue disconnect commands
		// if some connected peer gets banned.
		peerstore_handle.register_protocol(cmd_tx.clone());

		(
			Self {
				protocol,
				max_out,
				num_out: 0usize,
				max_in,
				num_in: 0usize,
				reserved_peers,
				cmd_rx,
				peerstore_handle,
				reserved_only,
				peers,
				connected_peers,
				waker: None,
				pending_backoffs: FuturesUnordered::new(),
				iteration: 0usize,
			},
			cmd_tx,
		)
	}

	/// Report to [`Peerset`] that a substream was opened.
	///
	/// Slot for the stream was "preallocated" when the it was initiated (outbound) or accepted
	/// (inbound) by the local node which is why this function doesn't allocate a slot for the peer.
	///
	/// Returns `true` if the substream should be kept open and `false` if the substream had been
	/// canceled while it was opening and litep2p should close the substream.
	pub fn report_substream_opened(&mut self, peer: PeerId, direction: traits::Direction) -> bool {
		log::trace!(
			target: LOG_TARGET,
			"{}: substream opened to {peer:?}, direction {direction:?}, reserved peer {}",
			self.protocol,
			self.reserved_peers.contains(&peer),
		);

		let Some(state) = self.peers.get_mut(&peer) else {
			log::warn!(target: LOG_TARGET, "{}: substream opened for unknown peer {peer:?}", self.protocol);
			debug_assert!(false);
			return false
		};

		// litep2p doesn't support the ability to cancel an opening substream to
		match state {
			PeerState::Opening { direction } => {
				*state = PeerState::Connected { direction: *direction };
				self.connected_peers.fetch_add(1usize, Ordering::Relaxed);
				true
			},
			PeerState::Canceled { direction } => {
				log::trace!(
					target: LOG_TARGET,
					"{}: substream to {peer:?} is canceled, issue disconnection request",
					self.protocol,
				);

				self.connected_peers.fetch_add(1usize, Ordering::Relaxed);
				*state = PeerState::Closing { direction: *direction };
				false
			},
			state => {
				panic!("{}: invalid state for open substream {peer:?} {state:?}", self.protocol);
			},
		}
	}

	/// Report to [`Peerset`] that a substream was closed.
	///
	/// If the peer was not a reserved peer, the inbound/outbound slot count is adjusted to account
	/// for the disconnected peer. After the connection is closed, the peer is chilled for a
	/// duration of [`DEFAULT_BACKOFF`] which prevens [`Peerset`] from establishing/accepting new
	/// connections for that time period.
	///
	/// Reserved peers cannot be disconnected using this method and they can be disconnected only if
	/// they're banned.
	pub fn report_substream_closed(&mut self, peer: PeerId) {
		log::trace!(target: LOG_TARGET, "{}: substream closed to {peer:?}", self.protocol);

		let Some(state) = self.peers.get_mut(&peer) else {
			log::warn!(target: LOG_TARGET, "{}: substream closed for unknown peer {peer:?}", self.protocol);
			debug_assert!(false);
			return
		};

		match &state {
			// close was initiated either by remote ([`PeerState::Connected`]) or local node
			// ([`PeerState::Closing`]) and it was a non-reserved peer
			PeerState::Connected { direction: Direction::Inbound(Reserved::No) } |
			PeerState::Closing { direction: Direction::Inbound(Reserved::No) } => {
				adjust_or_warn!(self.num_in, peer, self.protocol, Direction::Inbound(Reserved::No));
			},
			// close was initiated either by remote ([`PeerState::Connected`]) or local node
			// ([`PeerState::Closing`]) and it was a non-reserved peer
			PeerState::Connected { direction: Direction::Outbound(Reserved::No) } |
			PeerState::Closing { direction: Direction::Outbound(Reserved::No) } => {
				adjust_or_warn!(
					self.num_out,
					peer,
					self.protocol,
					Direction::Outbound(Reserved::No)
				);
			},
			// reserved peers don't require adjustments to slot counts
			PeerState::Closing { .. } | PeerState::Connected { .. } => {
				log::debug!(target: LOG_TARGET, "{}: reserved peer {peer:?} disconnected", self.protocol);
			},
			state => {
				log::warn!(target: LOG_TARGET, "{}: invalid state for disconnected peer {peer:?}: {state:?}", self.protocol);
				debug_assert!(false);
			},
		}
		*state = PeerState::Backoff;

		self.connected_peers.fetch_sub(1usize, Ordering::Relaxed);
		self.peerstore_handle.report_peer(peer, DISCONNECT_ADJUSTMENT);
		self.pending_backoffs.push(Box::pin(async move {
			Delay::new(DEFAULT_BACKOFF).await;
			peer
		}));
		self.waker.take().map(|waker| waker.wake());
	}

	/// Report to [`Peerset`] that an inbound substream was opened and that it should validate it.
	pub fn report_inbound_substream(&mut self, peer: PeerId) -> ValidationResult {
		log::trace!(target: LOG_TARGET, "{}: inbound substream from {peer:?}", self.protocol);

		let state = self.peers.entry(peer).or_insert(PeerState::Disconnected);
		let reserved_peer = self.reserved_peers.contains(&peer);

		match state {
			// disconnected peers proceed directly to inbound slot allocation
			PeerState::Disconnected => {},
			// backed-off peers are ignored (TODO: should they be ignored?)
			PeerState::Backoff => {
				log::trace!(target: LOG_TARGET, "{}: ({peer:?}) is backed-off, reject inbound substream", self.protocol);
				return ValidationResult::Reject
			},
			// `Peerset` had initiated an outbound substream but litep2p had received an inbound
			// substream before the command was received. As litep2p is the source of truth as far
			// as substream states go, `Peerset` must update its own state and attempt to accept the
			// peer as inbound if there are enough slots.
			PeerState::Opening { direction: Direction::Outbound(reserved) } => {
				log::trace!(
					target: LOG_TARGET,
					"{}: inbound substream received for {peer:?} that was marked outbound",
					self.protocol,
				);

				match (reserved_peer, &reserved) {
					(true, &Reserved::Yes) => {},
					(false, &Reserved::No) => {},
					_ => panic!(
						"{}: state mismatch for {peer:?}, {reserved_peer} {reserved:?}",
						self.protocol
					),
				}

				// since the peer was not a reserved peer when the outbound substream was opened and
				// there are no free inbound slots, adjust outbound slot count and reject the peer
				if std::matches!(reserved, Reserved::No) && self.num_in >= self.max_in {
					log::debug!(
						target: LOG_TARGET,
						"{}: inbound substream for {peer:?} cannot be accepted because there aren't any free slots",
						self.protocol,
					);

					self.num_out -= 1;
					*state = PeerState::Disconnected;
					return ValidationResult::Reject
				}
			},
			PeerState::Canceled { direction } => {
				match direction {
					Direction::Outbound(reserved) => match reserved {
						Reserved::Yes => {},
						Reserved::No => adjust_or_warn!(
							self.num_out,
							self.protocol,
							peer,
							Direction::Outbound(Reserved::No)
						),
					},
					direction => {
						panic!(
							"{}: invalid direction for canceled inbound substream {peer:?}",
							self.protocol
						);
					},
					_ => {},
				}

				*state = PeerState::Disconnected;
				return ValidationResult::Reject
			},
			state => {
				log::warn!(
					target: LOG_TARGET,
					"{}: invalid state ({state:?}) for inbound substream, peer {peer:?}",
					self.protocol
				);
				debug_assert!(false);
				return ValidationResult::Reject
			},
		}

		if reserved_peer {
			*state = PeerState::Opening { direction: Direction::Inbound(reserved_peer.into()) };
			return ValidationResult::Accept
		}

		if self.num_in < self.max_in {
			self.num_in += 1;
			debug_assert!(self.num_in <= self.max_in);

			*state = PeerState::Opening { direction: Direction::Inbound(reserved_peer.into()) };
			return ValidationResult::Accept
		}

		log::trace!(
			target: LOG_TARGET,
			"{}: reject {peer:?}, not a reserved peer and no free inbound slots",
			self.protocol,
		);

		*state = PeerState::Disconnected;
		return ValidationResult::Reject
	}

	/// Report to [`Peerset`] that an inbound substream was opened and that it should validate it.
	pub fn report_substream_open_failure(&mut self, peer: PeerId, error: NotificationError) {
		log::trace!(
			target: LOG_TARGET,
			"{}: failed to open substream to {peer:?}: {error:?}",
			self.protocol,
		);

		match self.peers.get(&peer) {
			Some(PeerState::Opening { direction: Direction::Outbound(Reserved::No) }) => {
				adjust_or_warn!(
					self.num_out,
					self.protocol,
					peer,
					Direction::Outbound(Reserved::No)
				);
			},
			Some(PeerState::Opening { direction: Direction::Inbound(Reserved::No) }) => {
				adjust_or_warn!(self.num_in, self.protocol, peer, Direction::Inbound(Reserved::No));
			},
			Some(PeerState::Canceled { direction }) => match direction {
				Direction::Inbound(Reserved::No) => {
					adjust_or_warn!(
						self.num_in,
						self.protocol,
						peer,
						Direction::Inbound(Reserved::No)
					);
				},
				Direction::Outbound(Reserved::No) => {
					adjust_or_warn!(
						self.num_out,
						self.protocol,
						peer,
						Direction::Outbound(Reserved::No)
					);
				},
				_ => {},
			},
			// reserved
			Some(PeerState::Opening { direction: Direction::Inbound(Reserved::Yes) }) |
			Some(PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) }) => {
				log::debug!(
					target: LOG_TARGET,
					"{}: substream open failure for reserved peer {peer:?}",
					self.protocol,
				);
			},
			state => {
				panic!(
					"{}: unexpected state for substream open failure: {peer:?} {state:?}",
					self.protocol
				);
			},
		}

		self.peers.insert(peer, PeerState::Backoff);
		self.peerstore_handle.report_peer(peer, OPEN_FAILURE_ADJUSTMENT);
		self.pending_backoffs.push(Box::pin(async move {
			Delay::new(DEFAULT_BACKOFF).await;
			peer
		}));
		self.waker.take().map(|waker| waker.wake());
	}
}

impl Stream for Peerset {
	type Item = PeersetNotificationCommand;

	fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		self.waker = Some(cx.waker().clone());

		// self.iteration += 1;
		// if (self.iteration % 500) == 0 {
		// 	let connected_reserved = self.peers.iter().fold(0usize, |acc, (peer, state)| {
		// 		if self.reserved_peers.contains(peer) &&
		// 			std::matches!(state, PeerState::Connected { .. })
		// 		{
		// 			acc + 1
		// 		} else {
		// 			acc
		// 		}
		// 	});
		// 	let open = self.peers.iter().fold(0usize, |acc, (peer, state)| {
		// 		if std::matches!(state, PeerState::Opening { .. }) {
		// 			acc + 1
		// 		} else {
		// 			acc
		// 		}
		// 	});
		// 	let cancel = self.peers.iter().fold(0usize, |acc, (peer, state)| {
		// 		if std::matches!(state, PeerState::Canceled { .. }) {
		// 			log::debug!(target: LOG_TARGET, "{}: {peer:?} is canceled", self.protocol);
		// 			acc + 1
		// 		} else {
		// 			acc
		// 		}
		// 	});
		// 	let close = self.peers.iter().fold(0usize, |acc, (peer, state)| {
		// 		if std::matches!(state, PeerState::Closing { .. }) {
		// 			log::trace!("{}: peer that's getting closed {peer:?}", self.protocol);
		// 			acc + 1
		// 		} else {
		// 			acc
		// 		}
		// 	});

		// 	let total_peers = self.num_in + self.num_out;
		// 	let total = self.peers.iter().fold(0usize, |acc, (peer, state)| {
		// 		if std::matches!(
		// 			state,
		// 			PeerState::Closing { .. } |
		// 				PeerState::Canceled { .. } |
		// 				PeerState::Opening { .. } |
		// 				PeerState::Connected { .. }
		// 		) {
		// 			acc + 1
		// 		} else {
		// 			acc
		// 		}
		// 	});

		// 	if total_peers != 0 && total_peers != total {
		// 		panic!("{}: state mismatch {total} vs {total_peers}", self.protocol);
		// 	}

		// 	log::info!(
		// 		target: LOG_TARGET,
		// 		"{}: num_in/max_in {}/{}, num_in/max_in {}/{}, connected reserved peers {}, open {}
		// cancel {} close {}", 		self.protocol,
		// 		self.num_in,
		// 		self.max_in,
		// 		self.num_out,
		// 		self.max_out,
		// 		connected_reserved,
		// 		open,cancel,close,
		// 	)
		// }

		// check if any pending backoffs have expired
		while let Poll::Ready(Some(peer)) = self.pending_backoffs.poll_next_unpin(cx) {
			log::trace!(target: LOG_TARGET, "{}: backoff expired for {peer:?}", self.protocol);
			self.peers.insert(peer, PeerState::Disconnected);
		}

		// TODO(aaro): coalesce the results into one call to `litep2p`
		if let Poll::Ready(Some(action)) = Pin::new(&mut self.cmd_rx).poll_next(cx) {
			match action {
				PeersetCommand::DisconnectPeer { peer } => match self.peers.remove(&peer) {
					Some(PeerState::Connected { direction }) => {
						log::trace!(
							target: LOG_TARGET,
							"{}: close connection to {peer:?}, direction {direction:?}",
							self.protocol,
						);

						self.peers.insert(peer, PeerState::Closing { direction });
						return Poll::Ready(Some(PeersetNotificationCommand::CloseSubstream {
							peers: vec![peer],
						}))
					},
					Some(PeerState::Backoff) => {
						log::trace!(
							target: LOG_TARGET,
							"{}: cannot disconnect {peer:?}, already backed-off",
							self.protocol,
						);

						self.peers.insert(peer, PeerState::Backoff);
					},
					Some(PeerState::Opening { direction }) => {
						self.peers.insert(peer, PeerState::Canceled { direction });
					},
					// protocol had issued two disconnection requests in rapid succession and the
					// substream hadn't closed before the second disconnection request was received,
					// this is harmless and can be ignored.
					Some(state @ PeerState::Closing { .. }) => {
						log::trace!(
							target: LOG_TARGET,
							"{}: cannot disconnect {peer:?}, already closing ({state:?})",
							self.protocol,
						);

						self.peers.insert(peer, state);
					},
					// if peer is banned, e.g. due to genesis mismatch, `Peerstore` will issue a
					// global disconnection request to all protocols, irrespective of the
					// connectivity state. Peer isn't necessarily connected to all protocols at all
					// times so this is a harmless state to be in if a disconnection request is
					// received.
					Some(state @ PeerState::Disconnected) => {
						self.peers.insert(peer, state);
					},
					// peer had an opening substream earlier which was canceled and then,
					// e.g., the peer was banned which caused it to be disconnected again
					// TODO(aaro): add test
					Some(state @ PeerState::Canceled { .. }) => {
						log::debug!(
							target: LOG_TARGET,
							"{}: cannot disconnect {peer:?}, already canceled ({state:?})",
							self.protocol,
						);

						self.peers.insert(peer, state);
					},
					Some(state) => {
						log::warn!(
							target: LOG_TARGET,
							"{}: cannot disconnect {peer:?}, invalid state: {state:?}",
							self.protocol,
						);

						self.peers.insert(peer, state);
						debug_assert!(false);
					},
					// peer doesn't exist
					//
					// this can happen, for example, when peer connects over `/block-announces/1`
					// and it has wrong genesis hash which initiates a ban for that peer. Since the
					// ban is reported to all protocols but the peer mightn't have been registered
					// to GRANDPA or transactions yet, the peer doesn't exist in their `Peerset`s
					// and the error can just be ignored.
					None => {
						log::debug!(target: LOG_TARGET, "{}: {peer:?} doesn't exist", self.protocol);
					},
				},
				PeersetCommand::SetReservedPeers { peers } => {
					log::debug!(target: LOG_TARGET, "{}: set reserved peers {peers:?}", self.protocol);

					if !peers.is_empty() {
						// collect reserved peers which are not in new reserved set
						let peers_to_disconnect = self
							.reserved_peers
							.iter()
							.filter_map(|peer| (!peers.contains(peer)).then_some(*peer))
							.collect::<Vec<_>>();

						// set `reserved_peers` to the new set and issue wake-up for the future so
						// `Peerset` will open substreams for the new reserved peers
						cx.waker().wake_by_ref();
						self.reserved_peers = peers;

						if !peers_to_disconnect.is_empty() {
							return Poll::Ready(Some(PeersetNotificationCommand::CloseSubstream {
								peers: peers_to_disconnect,
							}))
						}
					}
				},
				PeersetCommand::AddReservedPeers { peers } => {
					log::debug!(target: LOG_TARGET, "{}: add reserved peers {peers:?}", self.protocol);

					return Poll::Ready(Some(PeersetNotificationCommand::OpenSubstream {
						peers: peers
							.iter()
							.filter_map(|peer| {
								if !self.reserved_peers.insert(*peer) {
									log::warn!(
										target: LOG_TARGET,
										"{}: {peer:?} is already a reserved peer",
										self.protocol,
									);
									return None
								}

								std::matches!(
									self.peers.get_mut(peer),
									None | Some(PeerState::Disconnected)
								)
								.then(|| {
									self.peers.insert(
										*peer,
										PeerState::Opening {
											direction: Direction::Outbound(Reserved::Yes),
										},
									);
									*peer
								})
							})
							.collect(),
					}))
				},
				PeersetCommand::RemoveReservedPeers { peers } => {
					log::debug!(target: LOG_TARGET, "{}: remove reserved peers {peers:?}", self.protocol);

					return Poll::Ready(Some(PeersetNotificationCommand::CloseSubstream {
						peers: peers
							.iter()
							.filter_map(|peer| {
								if !self.reserved_peers.remove(peer) {
									log::warn!(
										target: LOG_TARGET,
										"{}: {peer} is not a reserved peer",
										self.protocol,
									);
									return None
								}

								let protocol = self.protocol.clone();
								let peer_state = self.peers.get_mut(peer)?;

								match std::mem::replace(peer_state, PeerState::Disconnected) {
									PeerState::Connected { direction } => {
										*peer_state = PeerState::Closing { direction };
										return Some(*peer)
									},
									PeerState::Opening { direction } => {
										log::trace!(
											target: LOG_TARGET,
											"{}: {:?} removed from reserved peers but was not connected, canceling",
											protocol,
											peer,
										);

										match direction {
											Direction::Inbound(Reserved::Yes) |
											Direction::Outbound(Reserved::Yes) => {},
											state => {
												// TODO: explain this state
											},
										}

										*peer_state = PeerState::Canceled { direction };
										None
									},
									// peer might have already disconnected by the time request to
									// disconnect them was received and the peer was backed off but
									// it had no expired by the time the request to disconnect the
									// peer was received
									PeerState::Backoff => {
										log::trace!(
											target: LOG_TARGET,
											"{}: cannot disconnect removed reserved peer {:?}, already backed-off",
											protocol,
											peer,
										);

										*peer_state = PeerState::Backoff;
										None
									},
									// if there is a rapid change in substream state, the peer may
									// be canceled when the substream is asked to be closed.
									//
									// this can happen if substream is first opened and the very
									// soon after canceled. The substream may not have had time to
									// open yet and second open is ignored. If the substream is now
									// closed again before it has had time to open, it will be in
									// canceled state since `Peerset` is still waiting to hear
									// either success/failure on the original substream it tried to
									// cancel.
									PeerState::Canceled { direction } => {
										log::trace!(
											target: LOG_TARGET,
											"{}: cannot disconnect removed reserved peer {:?}, already canceled",
											protocol,
											peer,
										);
										*peer_state = PeerState::Canceled { direction };
										None
									},
									// substream to the peer might have failed to open which caused
									// the peer to be backed off
									//
									// the back-off might've expired by the time the peer was
									// disconnected at which point the peer is already disconnected
									// when the protocol asked the peer to be disconnected
									PeerState::Disconnected => {
										log::trace!(
											target: LOG_TARGET,
											"{}: cannot disconnect removed reserved peer {:?}, already disconnected",
											protocol,
											peer,
										);

										*peer_state = PeerState::Disconnected;
										None
									},
									// TODO(aaro): explain
									// TODO(aaro): add test
									PeerState::Closing { direction } => {
										log::trace!(
											target: LOG_TARGET,
											"{}: cannot disconnect removed reserved peer {:?}, already closing",
											protocol,
											peer,
										);

										*peer_state = PeerState::Closing { direction };
										None
									},
									state => {
										log::warn!(target: LOG_TARGET, "ignore state {state:?} for {peer:?}");
										*peer_state = state;
										debug_assert!(false);
										None
									},
								}
							})
							.collect(),
					}))
				},
				PeersetCommand::SetReservedOnly { reserved_only } => {
					log::debug!(target: LOG_TARGET, "{}: set reserved only mode to {reserved_only}", self.protocol);

					// update mode and if it's set to false, disconnect all non-reserved peers
					self.reserved_only = reserved_only;

					if reserved_only {
						let peers_to_remove = self
							.peers
							.iter()
							.filter_map(|(peer, state)| {
								(!self.reserved_peers.contains(peer) &&
									std::matches!(state, PeerState::Connected { .. }))
								.then_some(*peer)
							})
							.collect::<Vec<_>>();

						// set peers to correct states

						// peers who are who are connected are move to [`PeerState::Closing`]
						// and peers who are already opening are moved to [`PeerState::Canceled`]
						// and if the substream for them opens, it will be closed right after.
						self.peers.iter_mut().for_each(|(peer, state)| match state {
							PeerState::Connected { direction } => {
								*state = PeerState::Closing { direction: *direction };
							},
							// peer for whom a substream was opening are canceled and if the
							// substream opens successfully, it will be closed immediately
							PeerState::Opening { direction } => {
								*state = PeerState::Canceled { direction: *direction };
							},
							_ => {},
						});

						return Poll::Ready(Some(PeersetNotificationCommand::CloseSubstream {
							peers: peers_to_remove,
						}))
					}
				},
				PeersetCommand::GetReservedPeers { tx } => {
					let _ = tx.send(self.reserved_peers.iter().cloned().collect());
				},
			}
		}

		// try to establish connection any reserved peer who is not currently connected
		//
		// TODO(aaro): optimize this by keeping the set of reserved peers who are `Disconnected` to
		// a separate may so they don't have to be iterated over during each call to `poll_next()`
		let reserved_peers = self
			.peers
			.iter()
			.filter_map(|(peer, state)| {
				(self.reserved_peers.contains(peer) &&
					std::matches!(state, PeerState::Disconnected))
				.then_some(*peer)
			})
			.collect::<Vec<_>>();

		if !reserved_peers.is_empty() {
			log::trace!(
				target: LOG_TARGET,
				"{}: start connecting to reserved peers {reserved_peers:?}",
				self.protocol,
			);

			reserved_peers.iter().for_each(|peer| {
				self.peers.insert(
					*peer,
					PeerState::Opening { direction: Direction::Outbound(Reserved::Yes) },
				);
			});

			return Poll::Ready(Some(PeersetNotificationCommand::OpenSubstream {
				peers: reserved_peers,
			}))
		}

		// if the number of outbound peers is lower than the desired amount of oubound peers,
		// query `PeerStore` and try to get a new outbound candidated.
		if self.num_out < self.max_out && !self.reserved_only {
			// TODO(aaro): continuously update the ignore list so it doesn't have to be recollected
			// every time
			let ignore: HashSet<&PeerId> = self
				.peers
				.iter()
				.filter_map(|(peer, state)| {
					std::matches!(
						state,
						PeerState::Closing { .. } |
							PeerState::Backoff | PeerState::Opening { .. } |
							PeerState::Connected { .. } | PeerState::Canceled { .. },
					)
					.then_some(peer)
				})
				.collect();

			let peers: Vec<_> = self
				.peerstore_handle
				.next_outbound_peers(&ignore, self.max_out - self.num_out)
				.collect();

			if peers.len() > 0 {
				log::trace!(target: LOG_TARGET, "{}: start connecting to peers {peers:?}", self.protocol);

				peers.iter().for_each(|peer| {
					self.peers.insert(
						*peer,
						PeerState::Opening { direction: Direction::Outbound(Reserved::No) },
					);
				});
				self.num_out += peers.len();

				return Poll::Ready(Some(PeersetNotificationCommand::OpenSubstream { peers }))
			}
		}

		Poll::Pending
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::litep2p::peerstore::peerstore_handle;

	#[tokio::test]
	async fn test() {
		let mut peerstore_handle = peerstore_handle();
		let (mut peerset, mut tx) = Peerset::new(
			ProtocolName::from("/notif/1"),
			8usize,
			32usize,
			false,
			HashSet::new(),
			Default::default(),
			peerstore_handle.clone(),
		);

		peerset.report_substream_opened(PeerId::random(), traits::Direction::Inbound);
	}

	#[tokio::test]
	async fn set_reserved_peers() {
		let reserved_peer1 = PeerId::random();
		let reserved_peer2 = PeerId::random();
		let reserved_peer3 = PeerId::random();
		let mut peerstore_handle = peerstore_handle();

		let (mut peerset, mut tx) = Peerset::new(
			ProtocolName::from("/notif/1"),
			8usize,
			32usize,
			false,
			HashSet::from_iter([reserved_peer1, reserved_peer2, reserved_peer3]),
			Default::default(),
			peerstore_handle.clone(),
		);

		match peerset.next().await {
			state => panic!("didn't expect {state:?}"),
			Some(PeersetNotificationCommand::OpenSubstream { peers }) => {
				todo!();
			},
		}
	}

	#[tokio::test]
	async fn set_reserved_only() {}
}
