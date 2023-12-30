use std::{
    collections::{HashMap, HashSet, VecDeque},
    task::{Context, Poll},
    time::Duration,
};

use either::Either;
use futures::FutureExt;
use futures_timer::Delay;
use libp2p_core::{multiaddr::Protocol, transport::PortUse, Endpoint, Multiaddr};
use libp2p_identity::PeerId;
use libp2p_swarm::{
    behaviour::{ConnectionEstablished, ExternalAddrConfirmed},
    ConnectionClosed, ConnectionDenied, ConnectionHandler, ConnectionId, DialFailure, FromSwarm,
    NetworkBehaviour, NewExternalAddrCandidate, NotifyHandler, ToSwarm,
};
use rand::prelude::*;
use rand_core::OsRng;
use std::fmt::{Debug, Display, Formatter};

use crate::v2::client::handler::dial_request::InternalError;
use crate::v2::{global_only::IpExt, protocol::DialRequest};

use super::handler::{
    dial_back,
    dial_request::{self, InternalStatusUpdate},
    TestEnd,
};

#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// How many candidates we will test at most.
    pub(crate) max_candidates: usize,

    /// The interval at which we will attempt to confirm candidates as external addresses.
    pub(crate) probe_interval: Duration,
}

impl Config {
    pub fn with_max_candidates(self, max_candidates: usize) -> Self {
        Self {
            max_candidates,
            ..self
        }
    }

    pub fn with_probe_interval(self, probe_interval: Duration) -> Self {
        Self {
            probe_interval,
            ..self
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_candidates: 10,
            probe_interval: Duration::from_secs(5),
        }
    }
}

pub struct Behaviour<R = OsRng>
where
    R: RngCore + 'static,
{
    pending_nonces: HashMap<u64, NonceStatus>,
    rng: R,
    config: Config,
    pending_events: VecDeque<
        ToSwarm<
            <Self as NetworkBehaviour>::ToSwarm,
            <<Self as NetworkBehaviour>::ConnectionHandler as ConnectionHandler>::FromBehaviour,
        >,
    >,
    address_candidates: HashMap<Multiaddr, AddressInfo>,
    already_tested: HashSet<Multiaddr>,
    next_tick: Delay,
    peer_info: HashMap<ConnectionId, ConnectionInfo>,
}

impl<R> NetworkBehaviour for Behaviour<R>
where
    R: RngCore + 'static,
{
    type ConnectionHandler = Either<dial_request::Handler, dial_back::Handler>;

    type ToSwarm = Event;

    fn handle_established_inbound_connection(
        &mut self,
        _: ConnectionId,
        _: PeerId,
        _: &Multiaddr,
        _: &Multiaddr,
    ) -> Result<<Self as NetworkBehaviour>::ConnectionHandler, ConnectionDenied> {
        Ok(Either::Right(dial_back::Handler::new()))
    }

    fn handle_established_outbound_connection(
        &mut self,
        _: ConnectionId,
        peer_id: PeerId,
        _: &Multiaddr,
        _: Endpoint,
        _: PortUse,
    ) -> Result<<Self as NetworkBehaviour>::ConnectionHandler, ConnectionDenied> {
        Ok(Either::Left(dial_request::Handler::new(peer_id)))
    }

    fn on_swarm_event(&mut self, event: FromSwarm) {
        match event {
            FromSwarm::NewExternalAddrCandidate(NewExternalAddrCandidate { addr }) => {
                self.address_candidates
                    .entry(addr.clone())
                    .or_default()
                    .score += 1;
            }
            FromSwarm::ExternalAddrConfirmed(ExternalAddrConfirmed { addr }) => {
                if let Some(info) = self.address_candidates.get_mut(addr) {
                    info.is_tested = true;
                }
            }
            FromSwarm::ConnectionEstablished(ConnectionEstablished {
                peer_id,
                connection_id,
                endpoint,
                ..
            }) => {
                self.peer_info
                    .entry(connection_id)
                    .or_insert(ConnectionInfo {
                        peer_id,
                        supports_autonat: false,
                        is_local: addr_is_local(endpoint.get_remote_address()),
                    });
            }
            FromSwarm::ConnectionClosed(ConnectionClosed {
                peer_id,
                connection_id,
                ..
            }) => {
                self.handle_no_connection(peer_id, connection_id);
            }
            FromSwarm::DialFailure(DialFailure {
                peer_id: Some(peer_id),
                connection_id,
                ..
            }) => {
                self.handle_no_connection(peer_id, connection_id);
            }
            _ => {}
        }
    }

    fn on_connection_handler_event(
        &mut self,
        peer_id: PeerId,
        connection_id: ConnectionId,
        event: <Self::ConnectionHandler as ConnectionHandler>::ToBehaviour,
    ) {
        match event {
            Either::Right(nonce) => {
                let Some(status) = self.pending_nonces.get_mut(&nonce) else {
                    tracing::warn!(%peer_id, %nonce, "Received unexpected nonce");
                    return;
                };

                *status = NonceStatus::Received;
                tracing::debug!(%peer_id, %nonce, "Successful dial-back");
            }
            Either::Left(dial_request::ToBehaviour::PeerHasServerSupport) => {
                self.peer_info
                    .get_mut(&connection_id)
                    .expect("inconsistent state")
                    .supports_autonat = true;
            }
            Either::Left(dial_request::ToBehaviour::TestCompleted(InternalStatusUpdate {
                tested_addr,
                bytes_sent: data_amount,
                server,
                result,
                server_no_support,
            })) => {
                if server_no_support {
                    self.peer_info
                        .get_mut(&connection_id)
                        .expect("inconsistent state")
                        .supports_autonat = false;
                }

                match result {
                    Ok(TestEnd {
                        dial_request: DialRequest { nonce, .. },
                        ref reachable_addr,
                    }) => {
                        if !matches!(self.pending_nonces.get(&nonce), Some(NonceStatus::Received)) {
                            tracing::debug!(
                            "server reported reachbility, but didn't actually reached this node."
                        );
                        } else {
                            self.pending_events
                                .push_back(ToSwarm::ExternalAddrConfirmed(reachable_addr.clone()));
                        }
                    }
                    Err(ref err) => match &err.internal {
                        dial_request::InternalError::FailureDuringDialBack { addr: Some(addr) }
                        | dial_request::InternalError::UnableToConnectOnSelectedAddress {
                            addr: Some(addr),
                        } => {
                            if let Some(peer_info) = self.address_candidates.get_mut(addr) {
                                peer_info.is_tested = true;
                            }
                            tracing::debug!(addr = %addr, "Was unable to connect to the server on the selected address.")
                        }
                        dial_request::InternalError::InternalServer
                        | dial_request::InternalError::DataRequestTooLarge { .. }
                        | dial_request::InternalError::DataRequestTooSmall { .. }
                        | dial_request::InternalError::InvalidResponse
                        | dial_request::InternalError::ServerRejectedDialRequest
                        | dial_request::InternalError::InvalidReferencedAddress { .. }
                        | dial_request::InternalError::ServerChoseNotToDialAnyAddress => {
                            self.handle_no_connection(peer_id, connection_id);
                        }
                        _ => {
                            tracing::debug!("Test failed: {:?}", err);
                        }
                    },
                }
                let event = crate::v2::client::Event {
                    tested_addr,
                    bytes_sent: data_amount,
                    server: server.unwrap_or(peer_id),
                    result: result.map(|_| ()),
                };
                self.pending_events.push_back(ToSwarm::GenerateEvent(event));
            }
        }
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ToSwarm<Self::ToSwarm, <Self::ConnectionHandler as ConnectionHandler>::FromBehaviour>>
    {
        if let Some(event) = self.pending_events.pop_front() {
            return Poll::Ready(event);
        }
        if self.next_tick.poll_unpin(cx).is_ready() {
            self.inject_address_candiate_test();
            if let Some(event) = self.pending_events.pop_front() {
                return Poll::Ready(event);
            }
        }
        Poll::Pending
    }
}

impl<R> Behaviour<R>
where
    R: RngCore + 'static,
{
    pub fn new(rng: R, config: Config) -> Self {
        Self {
            pending_nonces: HashMap::new(),
            rng,
            next_tick: Delay::new(config.probe_interval),
            config,
            pending_events: VecDeque::new(),
            address_candidates: HashMap::new(),
            already_tested: HashSet::new(),
            peer_info: HashMap::new(),
        }
    }

    /// Inject an immediate test for all pending address candidates.
    fn inject_address_candiate_test(&mut self) {
        if self.peer_info.values().all(|info| !info.supports_autonat) {
            return;
        }
        if self.address_candidates.is_empty() {
            return;
        }
        if self.address_candidates.values().all(|info| info.is_tested) {
            return;
        }
        let mut entries = self
            .address_candidates
            .iter()
            .filter(|(_, info)| !info.is_tested)
            .filter(|(addr, _)| !self.already_tested.contains(addr))
            .map(|(addr, count)| (addr.clone(), *count))
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return;
        }
        entries.sort_unstable_by_key(|(_, count)| *count);
        let addrs = entries
            .iter()
            .rev()
            .map(|(addr, _)| addr)
            .take(self.config.max_candidates)
            .cloned()
            .collect();
        if let Some(ConnectionInfo { peer_id, .. }) = self
            .peer_info
            .values()
            .filter(|e| e.supports_autonat)
            .choose(&mut self.rng)
        {
            self.submit_req_for_peer(*peer_id, addrs);
        }
        self.next_tick.reset(self.config.probe_interval);
    }

    fn submit_req_for_peer(&mut self, peer: PeerId, addrs: Vec<Multiaddr>) {
        let nonce = self.rng.gen();
        let req = DialRequest { nonce, addrs };
        self.pending_nonces.insert(nonce, NonceStatus::Pending);
        if let Some(conn_id) = self
            .peer_info
            .iter()
            .filter(|(_, info)| info.supports_autonat)
            .find(|(_, info)| info.peer_id == peer)
            .map(|(id, _)| *id)
        {
            self.pending_events.push_back(ToSwarm::NotifyHandler {
                peer_id: peer,
                handler: NotifyHandler::One(conn_id),
                event: Either::Left(req),
            });
        }
    }

    fn handle_no_connection(&mut self, peer_id: PeerId, connection_id: ConnectionId) {
        let removeable_conn_ids = self
            .peer_info
            .iter()
            .filter(|(conn_id, info)| info.peer_id == peer_id && **conn_id == connection_id)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for conn_id in removeable_conn_ids {
            self.peer_info.remove(&conn_id);
        }
        let known_servers_n = self
            .peer_info
            .values()
            .filter(|info| info.supports_autonat)
            .count();
        let changed_n = self
            .peer_info
            .values_mut()
            .filter(|info| info.supports_autonat)
            .filter(|info| info.peer_id == peer_id)
            .map(|info| info.supports_autonat = false)
            .count();
        if known_servers_n != changed_n {
            tracing::trace!(server = %peer_id, "Removing potential Autonat server due to dial failure");
        }
    }

    pub fn validate_addr(&mut self, addr: &Multiaddr) {
        if let Some(info) = self.address_candidates.get_mut(addr) {
            info.is_tested = true;
        }
    }
}

impl Default for Behaviour<OsRng> {
    fn default() -> Self {
        Self::new(OsRng, Config::default())
    }
}

pub struct Error {
    pub(crate) internal: InternalError,
}

impl From<InternalError> for Error {
    fn from(internal: InternalError) -> Self {
        Self { internal }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.internal, f)
    }
}

impl Debug for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.internal, f)
    }
}

#[derive(Debug)]
pub struct Event {
    /// The address that was selected for testing.
    /// Is `None` in the case that the server respond with something unexpected.
    pub tested_addr: Option<Multiaddr>,
    /// The amount of data that was sent to the server.
    /// Is 0 if it wasn't necessary to send any data.
    /// Otherwise it's a number between 30.000 and 100.000.
    pub bytes_sent: usize,
    /// The peer id of the server that was selected for testing.
    pub server: PeerId,
    /// The result of the test. If the test was successful, this is `Ok(())`.
    /// Otherwise it's an error.
    pub result: Result<(), Error>,
}

fn addr_is_local(addr: &Multiaddr) -> bool {
    addr.iter().any(|c| match c {
        Protocol::Ip4(ip) => !IpExt::is_global(&ip),
        Protocol::Ip6(ip) => !IpExt::is_global(&ip),
        _ => false,
    })
}

enum NonceStatus {
    Pending,
    Received,
}

struct ConnectionInfo {
    peer_id: PeerId,
    supports_autonat: bool,
    is_local: bool,
}

#[derive(Copy, Clone, Default)]
struct AddressInfo {
    score: usize,
    is_tested: bool,
}

impl PartialOrd for AddressInfo {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.score.cmp(&other.score))
    }
}

impl PartialEq for AddressInfo {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score
    }
}

impl Ord for AddressInfo {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score.cmp(&other.score)
    }
}

impl Eq for AddressInfo {}
