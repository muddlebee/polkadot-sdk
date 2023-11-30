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

//! `NetworkBackend` implementation for `litep2p`.

#![allow(unused)]

use crate::{
	config::{
		FullNetworkConfiguration, IncomingRequest, NodeKeyConfig, NotificationHandshake, Params,
		SetConfig, TransportConfig,
	},
	error::Error,
	event::{DhtEvent, Event},
	litep2p::{
		discovery::{Discovery, DiscoveryEvent},
		peerstore::{peerstore_handle, Peerstore},
		service::{Litep2pNetworkService, NetworkServiceCommand},
		shim::{
			bitswap::BitswapServer,
			notification::{
				config::{NotificationProtocolConfig, ProtocolControlHandle},
				peerset::PeersetCommand,
			},
			request_response::{RequestResponseConfig, RequestResponseProtocol},
		},
	},
	multiaddr::Protocol,
	peer_store::PeerStoreProvider,
	protocol,
	service::{
		ensure_addresses_consistent_with_transport,
		metrics::{register_without_sources, MetricSources, NotificationMetrics},
		out_events,
		traits::{NetworkBackend, NetworkService},
	},
	IfDisconnected, NetworkStatus, NotificationService, ProtocolName, RequestFailure,
};

use codec::Encode;
use futures::{channel::oneshot, StreamExt};
use libp2p::{kad::RecordKey, Multiaddr};
use litep2p::{
	config::{Litep2pConfig, Litep2pConfigBuilder},
	crypto::ed25519::{Keypair, SecretKey},
	protocol::{
		libp2p::{
			bitswap::Config as BitswapConfig, identify::Config as IdentifyConfig,
			kademlia::QueryId, ping::ConfigBuilder as PingConfigBuilder,
		},
		request_response::{DialOptions, RequestResponseHandle},
	},
	transport::{
		tcp::config::TransportConfig as TcpTransportConfig,
		websocket::config::TransportConfig as WebSocketTransportConfig,
	},
	types::RequestId,
	Litep2p, Litep2pEvent,
};
use parking_lot::Mutex;
use prometheus_endpoint::Registry;
use tokio_stream::StreamMap;

use sc_client_api::BlockBackend;
use sc_network_common::{role::Roles, ExHashT};
use sc_network_types::PeerId;
use sc_utils::mpsc::{tracing_unbounded, TracingUnboundedReceiver, TracingUnboundedSender};
use sp_runtime::traits::Block as BlockT;

use std::{
	cmp,
	collections::{hash_map::Entry, HashMap, HashSet},
	fs,
	future::Future,
	io, iter,
	pin::Pin,
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	},
	time::Duration,
};

mod discovery;
mod peerstore;
mod service;
mod shim;

// TODO: metrics
// TODO: bandwidth sink
// TODO: add support for specifying external addresses

/// Logging target for the file.
const LOG_TARGET: &str = "sub-libp2p";

/// Networking backend for `litep2p`.
pub struct Litep2pNetworkBackend {
	/// `NetworkService` implementation for `Litep2pNetworkBackend`.
	network_service: Arc<dyn NetworkService>,

	/// RX channel for receiving commands from `Litep2pNetworkService`.
	cmd_rx: TracingUnboundedReceiver<NetworkServiceCommand>,

	/// Listen addresses. Do **NOT** include a trailing `/p2p/` with our `PeerId`.
	listen_addresses: Arc<Mutex<HashSet<Multiaddr>>>,

	/// `Peerset` handles to notification protocols.
	notif_protocols: HashMap<ProtocolName, ProtocolControlHandle>,

	/// Block announce protocol name.
	block_announce_protocol: ProtocolName,

	/// `litep2p` configuration.
	config: Litep2pConfig,

	/// Discovery.
	discovery: Discovery,

	/// Peerstore.
	peer_store_handle: Arc<dyn PeerStoreProvider>,
}

impl Litep2pNetworkBackend {
	/// Get `litep2p` keypair from `NodeKeyConfig`.
	fn get_keypair(node_key: &NodeKeyConfig) -> Result<(Keypair, litep2p::PeerId), Error> {
		let secret = libp2p::identity::Keypair::try_into_ed25519(node_key.clone().into_keypair()?)
			.map_err(|error| {
				log::error!(target: LOG_TARGET, "failed to convert to ed25519: {error:?}");
				Error::Io(io::ErrorKind::InvalidInput.into())
			})?
			.secret();

		// TODO: zzz
		let mut secret = secret.as_ref().iter().cloned().collect::<Vec<_>>();
		let secret = SecretKey::from_bytes(&mut secret)
			.map_err(|_| Error::Io(io::ErrorKind::InvalidInput.into()))?;
		let local_identity = Keypair::from(secret);
		let local_public = local_identity.public();
		let local_peer_id = local_public.to_peer_id();

		Ok((local_identity, local_peer_id))
	}

	/// Configure transport protocols for `Litep2pNetworkBackend`.
	fn configure_transport<B: BlockT + 'static, H: ExHashT>(
		config: &FullNetworkConfiguration<B, H, Self>,
		builder: Litep2pConfigBuilder,
	) -> Litep2pConfigBuilder {
		let config_mem = match config.network_config.transport {
			TransportConfig::MemoryOnly => panic!("memory transport not supported"),
			TransportConfig::Normal { .. } => false,
		};

		// The yamux buffer size limit is configured to be equal to the maximum frame size
		// of all protocols. 10 bytes are added to each limit for the length prefix that
		// is not included in the upper layer protocols limit but is still present in the
		// yamux buffer. These 10 bytes correspond to the maximum size required to encode
		// a variable-length-encoding 64bits number. In other words, we make the
		// assumption that no notification larger than 2^64 will ever be sent.
		// TODO: make this a function of `NetworkConfiguration`?
		let yamux_maximum_buffer_size = {
			let requests_max = config
				.request_response_protocols
				.iter()
				.map(|cfg| usize::try_from(cfg.max_request_size).unwrap_or(usize::MAX));
			let responses_max = config
				.request_response_protocols
				.iter()
				.map(|cfg| usize::try_from(cfg.max_response_size).unwrap_or(usize::MAX));
			let notifs_max = config
				.notification_protocols
				.iter()
				.map(|cfg| usize::try_from(cfg.max_notification_size()).unwrap_or(usize::MAX));

			// A "default" max is added to cover all the other protocols: ping, identify,
			// kademlia, block announces, and transactions.
			let default_max = cmp::max(
				1024 * 1024,
				usize::try_from(protocol::BLOCK_ANNOUNCES_TRANSACTIONS_SUBSTREAM_SIZE)
					.unwrap_or(usize::MAX),
			);

			iter::once(default_max)
				.chain(requests_max)
				.chain(responses_max)
				.chain(notifs_max)
				.max()
				.expect("iterator known to always yield at least one element; qed")
				.saturating_add(10)
		};

		let multiplexing_config = {
			let mut yamux_config = litep2p::yamux::Config::default();
			// Enable proper flow-control: window updates are only sent when
			// buffered data has been consumed.
			yamux_config.set_window_update_mode(litep2p::yamux::WindowUpdateMode::OnRead);
			yamux_config.set_max_buffer_size(yamux_maximum_buffer_size);

			if let Some(yamux_window_size) = config.network_config.yamux_window_size {
				yamux_config.set_receive_window(yamux_window_size);
			}

			yamux_config
		};

		log::error!(target: LOG_TARGET, "listen addresses: {:#?}", config.network_config.listen_addresses);

		let (tcp, websocket): (Vec<Option<_>>, Vec<Option<_>>) = config
			.network_config
			.listen_addresses
			.iter()
			.filter_map(|address| {
				let mut iter = address.iter();

				match iter.next() {
					Some(Protocol::Ip4(_) | Protocol::Ip6(_)) => {},
					protocol => {
						log::error!(
							target: LOG_TARGET,
							"unknown protocol {protocol:?}, ignoring {address:?}",
						);

						return None
					},
				}

				match iter.next() {
					Some(Protocol::Tcp(_)) => match iter.next() {
						Some(Protocol::Ws(_) | Protocol::Wss(_)) =>
							Some((None, Some(address.clone()))),
						Some(Protocol::P2p(_)) | None => Some((Some(address.clone()), None)),
						protocol => {
							log::error!(
								target: LOG_TARGET,
								"unknown protocol {protocol:?}, ignoring {address:?}",
							);
							None
						},
					},
					protocol => {
						log::error!(
							target: LOG_TARGET,
							"unknown protocol {protocol:?}, ignoring {address:?}",
						);
						None
					},
				}
			})
			.unzip();

		builder
			// .with_websocket(WebSocketTransportConfig {
			// 	listen_addresses: websocket
			// 		.into_iter()
			// 		.filter_map(|address| address)
			// 		.collect::<Vec<_>>(),
			// 	..Default::default()
			// })
			.with_tcp(TcpTransportConfig {
				listen_addresses: tcp.into_iter().filter_map(|address| address).collect::<Vec<_>>(),
				..Default::default()
			})
	}

	/// Verify that given addresses match with the selected transport(s).
	fn sanity_check_addresses<B: BlockT + 'static, H: ExHashT>(
		config: &FullNetworkConfiguration<B, H, Self>,
	) -> Result<(), Error> {
		// Ensure the listen addresses are consistent with the transport.
		ensure_addresses_consistent_with_transport(
			config.network_config.listen_addresses.iter(),
			&config.network_config.transport,
		)?;
		ensure_addresses_consistent_with_transport(
			config.network_config.boot_nodes.iter().map(|x| &x.multiaddr),
			&config.network_config.transport,
		)?;
		ensure_addresses_consistent_with_transport(
			config
				.network_config
				.default_peers_set
				.reserved_nodes
				.iter()
				.map(|x| &x.multiaddr),
			&config.network_config.transport,
		)?;

		for notification_protocol in &config.notification_protocols {
			ensure_addresses_consistent_with_transport(
				notification_protocol.set_config().reserved_nodes.iter().map(|x| &x.multiaddr),
				&config.network_config.transport,
			)?;
		}
		ensure_addresses_consistent_with_transport(
			config.network_config.public_addresses.iter(),
			&config.network_config.transport,
		)?;

		Ok(())
	}
}

#[async_trait::async_trait]
impl<B: BlockT + 'static, H: ExHashT> NetworkBackend<B, H> for Litep2pNetworkBackend {
	type NotificationProtocolConfig = NotificationProtocolConfig;
	type RequestResponseProtocolConfig = RequestResponseConfig;
	type NetworkService<Block, Hash> = Arc<Litep2pNetworkService>;
	type PeerStore = Peerstore;
	type BitswapConfig = BitswapConfig;

	/// Create new `NetworkBackend`.
	// TODO(aaro): clean up this function
	fn new(mut params: Params<B, H, Self>) -> Result<Self, Error>
	where
		Self: Sized,
	{
		// get local keypair and local peer id
		let (keypair, local_peer_id) =
			Self::get_keypair(&params.network_config.network_config.node_key)?;
		let (cmd_tx, cmd_rx) = tracing_unbounded("mpsc_network_worker", 100_000);

		params.network_config.network_config.boot_nodes = params
			.network_config
			.network_config
			.boot_nodes
			.into_iter()
			.filter(|boot_node| boot_node.peer_id != local_peer_id.into())
			.collect();
		params.network_config.network_config.default_peers_set.reserved_nodes = params
			.network_config
			.network_config
			.default_peers_set
			.reserved_nodes
			.into_iter()
			.filter(|reserved_node| {
				if reserved_node.peer_id == local_peer_id.into() {
					log::warn!(
						target: LOG_TARGET,
						"Local peer ID used in reserved node, ignoring: {reserved_node}",
					);
					false
				} else {
					true
				}
			})
			.collect();

		Self::sanity_check_addresses(&params.network_config)?;

		if let Some(path) = &params.network_config.network_config.net_config_path {
			fs::create_dir_all(path)?;
		}

		log::info!(
			target: LOG_TARGET,
			"🏷  Local node identity is: {local_peer_id}",
		);

		let mut config_builder = Litep2pConfigBuilder::new();
		let mut config_builder = Self::configure_transport(&params.network_config, config_builder);

		let known_addresses = {
			// Collect all reserved nodes and bootnodes addresses.
			let mut addresses: Vec<_> = params
				.network_config
				.network_config
				.default_peers_set
				.reserved_nodes
				.iter()
				.map(|reserved| (reserved.peer_id, reserved.multiaddr.clone()))
				.chain(params.network_config.notification_protocols.iter().flat_map(|protocol| {
					protocol
						.set_config()
						.reserved_nodes
						.iter()
						.map(|reserved| (reserved.peer_id, reserved.multiaddr.clone()))
				}))
				.chain(
					params
						.network_config
						.network_config
						.boot_nodes
						.iter()
						.map(|bootnode| (bootnode.peer_id, bootnode.multiaddr.clone())),
				)
				.collect();

			// Remove possible duplicates.
			addresses.sort();
			addresses.dedup();

			addresses
		};

		// Check for duplicate bootnodes.
		params
			.network_config
			.network_config
			.boot_nodes
			.iter()
			.try_for_each(|bootnode| {
				if let Some(other) = params
					.network_config
					.network_config
					.boot_nodes
					.iter()
					.filter(|o| o.multiaddr == bootnode.multiaddr)
					.find(|o| o.peer_id != bootnode.peer_id)
				{
					Err(Error::DuplicateBootnode {
						address: bootnode.multiaddr.clone(),
						first_id: bootnode.peer_id.into(),
						second_id: other.peer_id.into(),
					})
				} else {
					Ok(())
				}
			})?;

		// List of bootnode multiaddresses.
		let mut boot_node_ids = HashMap::<PeerId, Vec<Multiaddr>>::new();

		for bootnode in params.network_config.network_config.boot_nodes.iter() {
			boot_node_ids
				.entry(bootnode.peer_id.into())
				.or_default()
				.push(bootnode.multiaddr.clone());
		}

		let boot_node_ids = Arc::new(boot_node_ids);
		let num_connected = Arc::new(AtomicUsize::new(0));
		// let external_addresses = Arc::new(Mutex::new(HashSet::new()));

		let FullNetworkConfiguration {
			notification_protocols,
			request_response_protocols,
			mut network_config,
		} = params.network_config;

		// initialize notification protocols
		//
		// pass the protocol configuration to `litep2pconfigurationbuilder` and save the tx channel
		// to the protocol's `peerset` together with the protocol name to allow other subsystems
		// polkadot sdk to control the connectivity behavior of the notification protocol
		let block_announce_protocol = params.block_announce_config.protocol_name().clone();
		let mut notif_protocols = HashMap::new();

		notif_protocols.insert(
			params.block_announce_config.protocol_name().clone(),
			params.block_announce_config.handle,
		);
		config_builder =
			config_builder.with_notification_protocol(params.block_announce_config.config);

		for mut config in notification_protocols {
			log::error!(target: LOG_TARGET, "enable {:?}", config.protocol_name);

			config.config.set_handshake(Roles::from(&params.role).encode());
			config_builder = config_builder.with_notification_protocol(config.config);
			notif_protocols.insert(config.protocol_name, config.handle);
		}

		// Initialize the metrics.
		let metrics = match &params.metrics_registry {
			Some(registry) => Some(register_without_sources(registry)?),
			None => None,
		};

		// initialize request-response protocols
		//
		// TODO: explanation
		let mut request_response_tx = HashMap::new();

		for config in request_response_protocols {
			let (protocol_config, handle) =
				litep2p::protocol::request_response::ConfigBuilder::new(
					litep2p::ProtocolName::from(config.protocol_name.clone()),
				)
				.with_max_size(
					std::cmp::max(config.max_request_size, config.max_response_size) as usize
				)
				.with_fallback_names(config.fallback_names.into_iter().map(From::from).collect())
				.with_timeout(config.request_timeout)
				.build();

			// TODO: register metrics from registry and then pass to `RequestResponseProtocol`
			config_builder = config_builder.with_request_response_protocol(protocol_config);
			let (protocol, tx) = RequestResponseProtocol::new(
				config.protocol_name.clone(),
				handle,
				peerstore_handle(),
				config.inbound_queue.expect("inbound queue to exist"),
				metrics.clone(),
			);
			params.spawn_handle.run(Box::pin(async move {
				protocol.run().await;
			}));
			request_response_tx.insert(config.protocol_name, tx);
		}

		// TODO: clean up this code
		let mut tmp: HashMap<litep2p::PeerId, Vec<Multiaddr>> = HashMap::new();
		let peer_store_handle = params.peer_store.clone();

		// add known addresses
		for (peer, address) in known_addresses {
			let last = address.iter().last();

			if std::matches!(
				last,
				// Some(crate::multiaddr::Protocol::Ws(_) | crate::multiaddr::Protocol::Wss(_))
				Some(crate::multiaddr::Protocol::Tcp(_))
			) {
				let new_address = address.with(crate::multiaddr::Protocol::P2p(peer.into()));
				match tmp.get_mut(&peer.into()) {
					Some(ref mut addrs) => {
						addrs.push(new_address);
					},
					None => {
						tmp.insert(peer.into(), vec![new_address]);
						peer_store_handle.add_known_peer(peer);
					},
				}
			}
		}
		config_builder = config_builder.with_known_addresses(tmp.clone().into_iter());

		// enable ipfs ping, identify and kademlia, and potentially mdns if user enabled it
		let (discovery, ping_config, identify_config, kademlia_config, maybe_mdns_config) =
			Discovery::new(
				&network_config,
				params.genesis_hash,
				params.fork_id.as_deref(),
				&params.protocol_id,
				tmp,
				peerstore_handle(),
			);

		config_builder = config_builder
			.with_libp2p_ping(ping_config)
			.with_libp2p_identify(identify_config)
			.with_libp2p_kademlia(kademlia_config)
			.with_executor(params.spawn_handle);

		if let Some(config) = maybe_mdns_config {
			config_builder = config_builder.with_mdns(config);
		}

		if let Some(config) = params.bitswap_config {
			config_builder = config_builder.with_libp2p_bitswap(config);
		}

		let listen_addresses = Arc::new(Mutex::new(HashSet::new()));
		let network_service = Arc::new(Litep2pNetworkService::new(
			local_peer_id,
			keypair.clone(),
			cmd_tx,
			params.peer_store.clone(),
			notif_protocols.clone(),
			block_announce_protocol.clone(),
			request_response_tx,
		));

		Ok(Self {
			network_service,
			cmd_rx,
			peer_store_handle,
			listen_addresses,
			config: config_builder.build(),
			notif_protocols,
			discovery,
			block_announce_protocol: block_announce_protocol.clone(),
		})
	}

	/// Get handle to `NetworkService` of the `NetworkBackend`.
	fn network_service(&self) -> Arc<dyn NetworkService> {
		Arc::clone(&self.network_service)
	}

	/// Create `PeerStore`.
	fn peer_store(bootnodes: Vec<PeerId>) -> Self::PeerStore {
		Peerstore::new(bootnodes)
	}

	fn register_notification_metrics(registry: Option<&Registry>) -> NotificationMetrics {
		NotificationMetrics::new(registry)
	}

	/// Create Bitswap server.
	fn bitswap_server(
		client: Arc<dyn BlockBackend<B> + Send + Sync>,
	) -> (Pin<Box<dyn Future<Output = ()> + Send>>, Self::BitswapConfig) {
		BitswapServer::new(client)
	}

	/// Create notification protocol configuration for `protocol`.
	fn notification_config(
		protocol_name: ProtocolName,
		fallback_names: Vec<ProtocolName>,
		max_notification_size: u64,
		handshake: Option<NotificationHandshake>,
		set_config: SetConfig,
		metrics: NotificationMetrics,
	) -> (Self::NotificationProtocolConfig, Box<dyn NotificationService>) {
		Self::NotificationProtocolConfig::new(
			protocol_name,
			fallback_names,
			max_notification_size as usize,
			handshake,
			set_config,
			metrics,
		)
	}

	/// Create request-response protocol configuration.
	fn request_response_config(
		protocol_name: ProtocolName,
		fallback_names: Vec<ProtocolName>,
		max_request_size: u64,
		max_response_size: u64,
		request_timeout: Duration,
		inbound_queue: Option<async_channel::Sender<IncomingRequest>>,
	) -> Self::RequestResponseProtocolConfig {
		Self::RequestResponseProtocolConfig::new(
			protocol_name,
			fallback_names,
			max_request_size,
			max_response_size,
			request_timeout,
			inbound_queue,
		)
	}

	/// Create [`Litep2pBackend`] object and start running its event loop.
	///
	/// Creating a separate inner litep2p runner is needed because `NetworkBackend::new()` is not
	/// async so `Litep2p` cannot be initialized using it. This needs to fixed but requires deeper
	/// refactoring in `builder.rs` to allow calling asynchronous functions.
	async fn run(mut self) {
		let mut litep2p_backend = Litep2pBackend {
			network_service: self.network_service,
			cmd_rx: self.cmd_rx,
			// peers: HashMap::new(),
			listen_addresses: self.listen_addresses,
			peerset_handles: self.notif_protocols,
			discovery: self.discovery,
			pending_put_values: HashMap::new(),
			pending_get_values: HashMap::new(),
			peerstore_handle: self.peer_store_handle,
			block_announce_protocol: self.block_announce_protocol,
			event_streams: out_events::OutChannels::new(None).unwrap(), // TODO: no unwraps
			litep2p: Litep2p::new(self.config).await.expect("to succeed"),
		};

		litep2p_backend.run().await;
	}
}

/// Litep2p backend.
struct Litep2pBackend {
	/// Main `litep2p` object.
	litep2p: Litep2p,

	/// `NetworkService` implementation for `Litep2pNetworkBackend`.
	network_service: Arc<dyn NetworkService>,

	/// RX channel for receiving commands from `Litep2pNetworkService`.
	cmd_rx: TracingUnboundedReceiver<NetworkServiceCommand>,

	/// Listen addresses. Do **NOT** include a trailing `/p2p/` with our `PeerId`.
	listen_addresses: Arc<Mutex<HashSet<Multiaddr>>>,

	/// `Peerset` handles to notification protocols.
	peerset_handles: HashMap<ProtocolName, ProtocolControlHandle>,

	/// Pending `GET_VALUE` queries.
	pending_get_values: HashMap<QueryId, RecordKey>,

	/// Pending `PUT_VALUE` queries.
	pending_put_values: HashMap<QueryId, RecordKey>,

	/// Discovery.
	discovery: Discovery,

	// /// Connected peers.
	// peers: HashMap<litep2p::PeerId, Endpoint>,
	/// Peerstore.
	peerstore_handle: Arc<dyn PeerStoreProvider>,

	/// Block announce protocol name.
	block_announce_protocol: ProtocolName,

	/// Sender for DHT events.
	event_streams: out_events::OutChannels,
}

impl Litep2pBackend {
	/// From an iterator of multiaddress(es), parse and group all addresses of peers
	/// so that litep2p can consume the information easily.
	// TODO: add tests for this function
	fn parse_addresses(
		addresses: impl Iterator<Item = Multiaddr>,
	) -> HashMap<PeerId, Vec<Multiaddr>> {
		addresses
			.into_iter()
			.filter_map(|address| match address.iter().next() {
				Some(
					Protocol::Dns(_) |
					Protocol::Dns4(_) |
					Protocol::Dns6(_) |
					Protocol::Ip6(_) |
					Protocol::Ip4(_),
				) => match address.iter().find(|protocol| std::matches!(protocol, Protocol::P2p(_)))
				{
					Some(Protocol::P2p(multihash)) => PeerId::from_multihash(multihash)
						.map_or(None, |peer| Some((peer, Some(address)))),
					_ => None,
				},
				Some(Protocol::P2p(multihash)) =>
					PeerId::from_multihash(multihash).map_or(None, |peer| Some((peer, None))),
				_ => None,
			})
			.fold(HashMap::new(), |mut acc, (peer, maybe_address)| {
				let entry = acc.entry(peer).or_default();
				maybe_address.map(|address| entry.push(address));

				acc
			})
	}

	/// Add new known addresses to `litep2p` and returned the parsed peer IDs.
	fn add_addresses(&self, peers: impl Iterator<Item = Multiaddr>) -> HashSet<PeerId> {
		Self::parse_addresses(peers.into_iter())
			.into_iter()
			.filter_map(|(peer, addresses)| {
				// `peers` contained multiaddress in the form `/p2p/<peer ID>`
				if addresses.is_empty() {
					return Some(peer)
				}

				// if self.litep2p.add_known_address(peer.into(), addresses.into_iter()) == 0usize {
				// 	log::warn!(
				// 		target: LOG_TARGET,
				// 		"couldn't add any addresses for peer {peer:?}, peer won't be added as reserved
				// peer" 	);
				// 	return None
				// }

				self.peerstore_handle.add_known_peer(peer);
				Some(peer)
			})
			.collect::<HashSet<_>>()
	}

	/// Start [`Litep2pBackend`] event loop.
	async fn run(mut self) {
		log::debug!(target: LOG_TARGET, "staring litep2p network backend");

		loop {
			tokio::select! {
				command = self.cmd_rx.next() => match command {
					None => return,
					Some(command) => match command {
						NetworkServiceCommand::GetValue{ key } => {
							let query_id = self.discovery.get_value(key.clone()).await;
							self.pending_get_values.insert(query_id, key);
						}
						NetworkServiceCommand::PutValue { key, value } => {
							let query_id = self.discovery.put_value(key.clone(), value).await;
							self.pending_put_values.insert(query_id, key);
						}
						NetworkServiceCommand::EventStream { tx } => {
							self.event_streams.push(tx);
						}
						NetworkServiceCommand::Status { tx } => {
							tx.send(NetworkStatus {
								num_connected_peers: self
									.peerset_handles
									.get(&self.block_announce_protocol)
									.map_or(0usize, |handle| handle.connected_peers.load(Ordering::Relaxed)),
								total_bytes_inbound: self.litep2p.bandwidth_sink().inbound() as u64,
								total_bytes_outbound: self.litep2p.bandwidth_sink().outbound() as u64,
							});
						}
						NetworkServiceCommand::AddPeersToReservedSet {
							protocol,
							peers,
						} => {
							let Some(handle) = self.peerset_handles.get(&protocol) else {
								log::warn!(target: LOG_TARGET, "protocol {protocol} doens't exist");
								continue
							};

							let peers = self.add_addresses(peers.into_iter());
							let _ = handle.tx.unbounded_send(PeersetCommand::AddReservedPeers { peers });
						}
						NetworkServiceCommand::ReportPeer { peer, cost_benefit } => {
							log::trace!(target: LOG_TARGET, "report {peer:?}: {cost_benefit:?}");

							self.peerstore_handle.report_peer(peer, cost_benefit);
						},
						NetworkServiceCommand::AddKnownAddress { peer, mut address } => {
							if !address.iter().any(|protocol| std::matches!(protocol, Protocol::P2p(_))) {
								address.push(Protocol::P2p(peer.into()));
							}

							if self.litep2p.add_known_address(peer.into(), iter::once(address.clone())) == 0usize {
								log::warn!(
									target: LOG_TARGET,
									"couldn't add known address ({address}) for {peer:?}, unsupported transport"
								);
							}
						},
						NetworkServiceCommand::SetReservedPeers { protocol, peers } => {
							let Some(handle) = self.peerset_handles.get(&protocol) else {
								log::warn!(target: LOG_TARGET, "protocol {protocol} doens't exist");
								continue
							};

							log::trace!(target: "sub-libp2p::peerset", "set reserved peers ({peers:?}) for {protocol}");

							let peers = self.add_addresses(peers.into_iter());
							let _ = handle.tx.unbounded_send(PeersetCommand::SetReservedPeers { peers });
						},
						NetworkServiceCommand::DisconnectPeer {
							protocol,
							peer,
						} => {
							let Some(handle) = self.peerset_handles.get(&protocol) else {
								log::warn!(target: LOG_TARGET, "protocol {protocol} doens't exist");
								continue
							};

							let _ = handle.tx.unbounded_send(PeersetCommand::DisconnectPeer { peer });
						}
						NetworkServiceCommand::SetReservedOnly {
							protocol,
							reserved_only,
						} => {
							let Some(handle) = self.peerset_handles.get(&protocol) else {
								log::warn!(target: LOG_TARGET, "protocol {protocol} doens't exist");
								continue
							};

							let _ = handle.tx.unbounded_send(PeersetCommand::SetReservedOnly { reserved_only });
						}
						NetworkServiceCommand::RemoveReservedPeers {
							protocol,
							peers,
						} => {
							let Some(handle) = self.peerset_handles.get(&protocol) else {
								log::warn!(target: LOG_TARGET, "protocol {protocol} doens't exist");
								continue
							};

							let _ = handle.tx.unbounded_send(PeersetCommand::RemoveReservedPeers { peers });
						}
					}
				},
				event = self.discovery.next() => match event {
					None => return,
					Some(DiscoveryEvent::RoutingTableUpdate { peers }) => {
						for peer in peers {
							self.peerstore_handle.add_known_peer(peer.into());
						}
					}
					Some(DiscoveryEvent::GetRecordSuccess { query_id, record }) => {
						match self.pending_get_values.remove(&query_id) {
							None => log::warn!(
								target: LOG_TARGET,
								"`GET_VALUE` succeeded for a non-existent query",
							),
							Some(key) => {
								log::trace!(
									target: LOG_TARGET,
									"`GET_VALUE` for {:?} ({query_id:?}) succeeded",
									record.key,
								);

								self.event_streams.send(Event::Dht(
									DhtEvent::ValueFound(vec![
										(libp2p::kad::RecordKey::new(&record.key), record.value)
									])
								));
							}
						}
					}
					Some(DiscoveryEvent::PutRecordSuccess { query_id }) => {
						match self.pending_put_values.remove(&query_id) {
							None => {
								log::warn!(
									target: LOG_TARGET,
									"`PUT_VALUE` succeeded for a non-existent query",
								);
							}
							Some(key) => {
								log::trace!(
									target: LOG_TARGET,
									"`PUT_VALUE` for {key:?} ({query_id:?}) succeeded",
								);
							}
						}
					}
					Some(DiscoveryEvent::QueryFailed { query_id }) => {
						match self.pending_get_values.remove(&query_id) {
							None => match self.pending_put_values.remove(&query_id) {
								None => log::warn!(
									target: LOG_TARGET,
									"non-existent query failed ({query_id:?})",
								),
								Some(key) => {
									log::debug!(
										target: LOG_TARGET,
										"`PUT_VALUE` ({query_id:?}) failed for key {key:?}",
									);

									self.event_streams.send(Event::Dht(
										DhtEvent::ValuePutFailed(libp2p::kad::RecordKey::new(&key))
									));
								}
							}
							Some(key) => {
								log::debug!(
									target: LOG_TARGET,
									"`GET_VALUE` ({query_id:?}) failed for key {key:?}",
								);

								self.event_streams.send(Event::Dht(
									DhtEvent::ValueNotFound(libp2p::kad::RecordKey::new(&key))
								));
							}
						}
					}
					Some(DiscoveryEvent::Identified {
						peer,
						observed_address,
						supported_protocols,
					}) => {
						// log::debug!(target: LOG_TARGET, "peer {peer:?} identified, supported protocols {supported_protocols:?}");

						// if let Some(Endpoint::Listener { address }) = self.peers.get(&peer) {
						// 	log::info!(target: LOG_TARGET, "observed address confirmed: {observed_address:?}");
						// }
					}
					event => {
						// log::warn!(target: LOG_TARGET, "ignoring discovery event: {event:?}");
					}
				},
				event = self.litep2p.next_event() => match event {
					Some(Litep2pEvent::ConnectionEstablished { peer, endpoint }) => {}
					// 	let _is_none = self.peers.insert(peer, endpoint);
					// 	debug_assert!(_is_none.is_none());
					// }
					Some(Litep2pEvent::ConnectionClosed { peer }) => {
						// let _is_some = self.peers.remove(&peer);
						// debug_assert!(_is_some.is_some());
					}
					_ => {}
				},
			}
		}
	}
}
