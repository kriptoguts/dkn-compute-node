use crate::p2p::AvailableNodes;
use crate::DRIA_COMPUTE_NODE_VERSION;
use libp2p::futures::StreamExt;
use libp2p::gossipsub::{
    Message, MessageAcceptance, MessageId, PublishError, SubscriptionError, TopicHash,
};
use libp2p::kad::{GetClosestPeersError, GetClosestPeersOk, QueryResult};
use libp2p::{
    autonat, gossipsub, identify, kad, multiaddr::Protocol, noise, swarm::SwarmEvent, tcp, yamux,
};
use libp2p::{Multiaddr, PeerId, Swarm, SwarmBuilder};
use libp2p_identity::Keypair;
use semver::Version;
use tokio::time::Duration;
use tokio::time::Instant;

use super::{DriaBehaviour, DriaBehaviourEvent};

/// Used as default for unparsable versions.
/// Will not match with any valid version.
const ZERO_VERSION: Version = Version::new(0, 0, 0);

/// Underlying libp2p client.
pub struct P2PClient {
    swarm: Swarm<DriaBehaviour>,
    /// Client version, parsed from the `Cargo.toml` version.
    version: Version,
    /// Peer count for (All, Mesh).
    peer_count: (usize, usize),
    /// Last time the peer count was refreshed.
    peer_last_refreshed: Instant,
}

/// Number of seconds before an idle connection is closed.
const IDLE_CONNECTION_TIMEOUT_SECS: u64 = 60;

/// Number of seconds between refreshing the Kademlia DHT.
const PEER_REFRESH_INTERVAL_SECS: u64 = 30;

impl P2PClient {
    /// Creates a new P2P client with the given keypair and listen address.
    pub fn new(
        keypair: Keypair,
        listen_addr: Multiaddr,
        available_nodes: &AvailableNodes,
    ) -> Result<Self, String> {
        // this is our peerId
        let node_peerid = keypair.public().to_peer_id();
        log::info!("Compute node peer address: {}", node_peerid);

        let mut swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )
            .map_err(|e| e.to_string())?
            .with_quic()
            .with_relay_client(noise::Config::new, yamux::Config::default)
            .map_err(|e| e.to_string())?
            .with_behaviour(|key, relay_behavior| Ok(DriaBehaviour::new(key, relay_behavior)))
            .map_err(|e| e.to_string())?
            .with_swarm_config(|c| {
                c.with_idle_connection_timeout(Duration::from_secs(IDLE_CONNECTION_TIMEOUT_SECS))
            })
            .build();

        // set mode to server so that RPC nodes add us to the DHT
        swarm
            .behaviour_mut()
            .kademlia
            .set_mode(Some(libp2p::kad::Mode::Server));

        // initiate bootstrap
        log::info!(
            "Initiating bootstrap: {:#?}",
            available_nodes.bootstrap_nodes
        );
        for addr in &available_nodes.bootstrap_nodes {
            if let Some(peer_id) = addr.iter().find_map(|p| match p {
                Protocol::P2p(peer_id) => Some(peer_id),
                _ => None,
            }) {
                log::info!("Dialling peer: {}", addr);
                swarm.dial(addr.clone()).map_err(|e| e.to_string())?;
                log::info!("Adding {} to Kademlia routing table", addr);
                swarm
                    .behaviour_mut()
                    .kademlia
                    .add_address(&peer_id, addr.clone());
            } else {
                log::warn!("Missing peerID in address: {}", addr);
            }
        }

        // do a random-walk on the DHT with a random peer
        log::info!("Searching for random peers.");
        let random_peer = PeerId::random();
        swarm
            .behaviour_mut()
            .kademlia
            .get_closest_peers(random_peer);
        swarm
            .behaviour_mut()
            .kademlia
            .bootstrap()
            .map_err(|e| e.to_string())?;

        // listen on all interfaces for incoming connections
        log::info!("Listening p2p network on: {}", listen_addr);
        swarm.listen_on(listen_addr).map_err(|e| e.to_string())?;

        log::info!(
            "Listening to relay nodes: {:#?}",
            available_nodes.relay_nodes
        );
        for addr in &available_nodes.relay_nodes {
            swarm
                .listen_on(addr.clone().with(Protocol::P2pCircuit))
                .map_err(|e| e.to_string())?;
        }

        Ok(Self {
            swarm,
            version: Version::parse(DRIA_COMPUTE_NODE_VERSION).unwrap(),
            peer_count: (0, 0),
            peer_last_refreshed: Instant::now(),
        })
    }

    /// Subscribe to a topic.
    pub fn subscribe(&mut self, topic_name: &str) -> Result<bool, SubscriptionError> {
        log::debug!("Subscribing to {}", topic_name);

        let topic = gossipsub::IdentTopic::new(topic_name);
        self.swarm.behaviour_mut().gossipsub.subscribe(&topic)
    }

    /// Unsubscribe from a topic.
    pub fn unsubscribe(&mut self, topic_name: &str) -> Result<bool, PublishError> {
        log::debug!("Unsubscribing from {}", topic_name);

        let topic = gossipsub::IdentTopic::new(topic_name);
        self.swarm.behaviour_mut().gossipsub.unsubscribe(&topic)
    }

    /// Publish a message to a topic.
    pub fn publish(
        &mut self,
        topic_name: &str,
        message_bytes: Vec<u8>,
    ) -> Result<MessageId, PublishError> {
        log::debug!("Publishing message to topic: {}", topic_name);

        let topic = gossipsub::IdentTopic::new(topic_name);
        let message_id = self
            .swarm
            .behaviour_mut()
            .gossipsub
            .publish(topic, message_bytes)?;

        Ok(message_id)
    }

    /// Validates a GossipSub message for propagation.
    ///
    /// - `Accept`: Accept the message and propagate it.
    /// - `Reject`: Reject the message and do not propagate it, with penalty to `propagation_source`.
    /// - `Ignore`: Ignore the message and do not propagate it, without any penalties.
    ///
    /// See [`validate_messages`](https://docs.rs/libp2p-gossipsub/latest/libp2p_gossipsub/struct.Config.html#method.validate_messages)
    /// and [`report_message_validation_result`](https://docs.rs/libp2p-gossipsub/latest/libp2p_gossipsub/struct.Behaviour.html#method.report_message_validation_result) for more details.
    pub fn validate_message(
        &mut self,
        msg_id: &MessageId,
        propagation_source: &PeerId,
        acceptance: MessageAcceptance,
    ) -> Result<(), PublishError> {
        log::debug!("Validating message ({}): {:?}", msg_id, acceptance);

        let msg_was_in_cache = self
            .swarm
            .behaviour_mut()
            .gossipsub
            .report_message_validation_result(msg_id, propagation_source, acceptance)?;

        if !msg_was_in_cache {
            log::debug!("Validated message was not in cache.");
        }
        Ok(())
    }

    /// Returns the list of connected peers within Gossipsub, with a list of subscribed topic hashes by each peer.
    pub fn peers(&self) -> Vec<(&PeerId, Vec<&TopicHash>)> {
        self.swarm
            .behaviour()
            .gossipsub
            .all_peers()
            .collect::<Vec<_>>()
    }

    /// Listens to the Swarm for incoming messages.
    ///
    /// This method should be called in a loop to keep the client running.
    /// When a GossipSub message is received, it will be returned.
    pub async fn process_events(&mut self) -> (PeerId, MessageId, Message) {
        loop {
            // refresh peers
            self.refresh_peer_counts().await;

            // wait for next event
            match self.swarm.select_next_some().await {
                SwarmEvent::Behaviour(DriaBehaviourEvent::Kademlia(
                    kad::Event::OutboundQueryProgressed {
                        result: QueryResult::GetClosestPeers(result),
                        ..
                    },
                )) => self.handle_closest_peers_result(result),
                SwarmEvent::Behaviour(DriaBehaviourEvent::Identify(
                    identify::Event::Received { peer_id, info, .. },
                )) => self.handle_identify_event(peer_id, info),
                SwarmEvent::Behaviour(DriaBehaviourEvent::Gossipsub(
                    gossipsub::Event::Message {
                        propagation_source: peer_id,
                        message_id,
                        message,
                    },
                )) => {
                    return (peer_id, message_id, message);
                }
                SwarmEvent::Behaviour(DriaBehaviourEvent::Autonat(
                    autonat::Event::StatusChanged { old, new },
                )) => {
                    log::warn!("AutoNAT status changed from {:?} to {:?}", old, new);
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    log::warn!("Local node is listening on {}", address);
                }
                SwarmEvent::ExternalAddrConfirmed { address } => {
                    log::warn!("External address confirmed: {}", address);
                }
                event => log::trace!("Unhandled Swarm Event: {:?}", event),
            }
        }
    }

    /// Handles identify events.
    ///
    /// At the top level, we check the protocol string.
    ///
    /// - For Kademlia, we check the kademlia protocol and then add the address to the Kademlia routing table.
    fn handle_identify_event(&mut self, peer_id: PeerId, info: identify::Info) {
        // we only care about the observed address, although there may be other addresses at `info.listen_addrs`
        let addr = info.observed_addr;

        // check protocol string
        // TODO: take the prefixes from a shared const
        let protocol_ok = self.check_version_with_prefix(&info.protocol_version, "dria/");
        if !protocol_ok {
            log::warn!(
                "Identify: Peer {} has different Identify protocol: (have {}, want {})",
                peer_id,
                info.protocol_version,
                self.version
            );
            return;
        }

        // check kademlia protocol
        if let Some(kad_protocol) = info
            .protocols
            .iter()
            .find(|p| p.to_string().starts_with("/dria/kad/"))
        {
            let protocol_ok = self.check_version_with_prefix(kad_protocol.as_ref(), "/dria/kad/");

            // if it matches our protocol, add it to the Kademlia routing table
            if protocol_ok {
                log::info!(
                    "Identify: {} peer {} identified at {}",
                    kad_protocol,
                    peer_id,
                    addr
                );

                self.swarm
                    .behaviour_mut()
                    .kademlia
                    .add_address(&peer_id, addr);
            } else {
                log::warn!(
                    "Identify: Peer {} has different Kademlia version: (have {}, want {})",
                    peer_id,
                    kad_protocol,
                    self.version
                );
            }
        }
    }

    /// Handles the results of a Kademlia closest peers search, simply logs it.
    fn handle_closest_peers_result(
        &mut self,
        result: Result<GetClosestPeersOk, GetClosestPeersError>,
    ) {
        match result {
            Ok(GetClosestPeersOk { peers, .. }) => {
                log::info!(
                    "Kademlia: Query finished with {} closest peers.",
                    peers.len()
                );
            }
            Err(GetClosestPeersError::Timeout { peers, .. }) => {
                log::info!(
                    "Kademlia: Query timed out with {} closest peers.",
                    peers.len()
                );
            }
        }
    }

    /// Does a random-walk over DHT and updates peer counts as needed.
    /// Keeps track of the last time the peer count was refreshed.
    ///
    /// Should be called in a loop.
    ///
    /// Returns: (All Peer Count, Mesh Peer Count)
    pub async fn refresh_peer_counts(&mut self) {
        if self.peer_last_refreshed.elapsed() > Duration::from_secs(PEER_REFRESH_INTERVAL_SECS) {
            let random_peer = PeerId::random();
            self.swarm
                .behaviour_mut()
                .kademlia
                .get_closest_peers(random_peer);
            self.peer_last_refreshed = Instant::now();

            // get peer count
            let gossipsub = &self.swarm.behaviour().gossipsub;
            let num_peers = gossipsub.all_peers().count();
            let num_mesh_peers = gossipsub.all_mesh_peers().count();

            // print peer counts
            log::info!("Peer Count (mesh/all): {} / {}", num_mesh_peers, num_peers);

            // print peers themselves if the count has changed
            if num_peers != self.peer_count.0 || num_mesh_peers != self.peer_count.1 {
                self.peer_count = (num_peers, num_mesh_peers);

                log::debug!(
                    "All Peers:\n{}",
                    gossipsub
                        .all_peers()
                        .enumerate()
                        .map(|(i, (p, _))| format!("{:#3}: {}", i, p))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                log::debug!(
                    "Mesh Peers:\n{}",
                    gossipsub
                        .all_mesh_peers()
                        .enumerate()
                        .map(|(i, p)| format!("{:#3}: {}", i, p))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
        }
    }

    /// Generic function to split a string such as `prefix || version` and check that the major & minor versions are the same.
    ///    
    /// Some examples:
    /// - `self.check_version_with_prefix("dria/")` for identity
    /// - `self.check_version_with_prefix("dria/kad/")` for Kademlia
    ///
    /// Returns whether the version is ok.
    fn check_version_with_prefix(&self, p: &str, prefix: &str) -> bool {
        let parsed_version = p
            .strip_prefix(prefix)
            .and_then(|v| Version::parse(v).ok())
            .unwrap_or(ZERO_VERSION);

        p.starts_with(prefix)
            && parsed_version.major == self.version.major
            && parsed_version.minor == self.version.minor
    }
}
