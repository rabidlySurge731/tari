//  Copyright 2020, The Tari Project
//
//  Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
//  following conditions are met:
//
//  1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
//  disclaimer.
//
//  2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
//  following disclaimer in the documentation and/or other materials provided with the distribution.
//
//  3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
//  products derived from this software without specific prior written permission.
//
//  THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
//  INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
//  DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//  SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
//  SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
//  WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
//  USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use crate::{
    event::DhtEvent,
    network_discovery::{
        discovering::Discovering,
        initializing::Initializing,
        ready::DiscoveryReady,
        waiting::Waiting,
        NetworkDiscoveryError,
    },
    DhtConfig,
};
use futures::{future, future::Either};
use log::*;
use std::{
    fmt,
    fmt::Display,
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tari_comms::{connectivity::ConnectivityRequester, peer_manager::NodeId, NodeIdentity, PeerManager};
use tari_shutdown::ShutdownSignal;
use tokio::{sync::broadcast, task};

const LOG_TARGET: &str = "comms::dht::network_discovery";

#[derive(Debug)]
enum State {
    Initializing,
    Ready(DiscoveryReady),
    Discovering(Discovering),
    Waiting(Waiting),
    Shutdown,
}

impl Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use State::*;
        match self {
            Initializing => write!(f, "Initializing"),
            Ready(_) => write!(f, "Ready"),
            Discovering(_) => write!(f, "Discovering"),
            Waiting(w) => write!(f, "Waiting({:.0?})", w.duration()),
            Shutdown => write!(f, "Shutdown"),
        }
    }
}

impl State {
    pub fn is_shutdown(&self) -> bool {
        match self {
            State::Shutdown => true,
            _ => false,
        }
    }
}

#[derive(Debug)]
pub enum StateEvent {
    Initialized,
    BeginDiscovery(DiscoveryParams),
    Ready,
    Idle,
    DiscoveryComplete(DhtNetworkDiscoveryRoundInfo),
    Errored(NetworkDiscoveryError),
    Shutdown,
}

impl Display for StateEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use StateEvent::*;
        match self {
            Initialized => write!(f, "Initialized"),
            BeginDiscovery(params) => write!(f, "BeginDiscovery({})", params),
            Ready => write!(f, "Ready"),
            Idle => write!(f, "Idle"),
            DiscoveryComplete(stats) => write!(f, "DiscoveryComplete({})", stats),
            Errored(err) => write!(f, "Errored({})", err),
            Shutdown => write!(f, "Shutdown"),
        }
    }
}

impl<E: Into<NetworkDiscoveryError>> From<E> for StateEvent {
    fn from(err: E) -> Self {
        Self::Errored(err.into())
    }
}

#[derive(Debug, Clone)]
pub struct NetworkDiscoveryContext {
    pub config: DhtConfig,
    pub peer_manager: Arc<PeerManager>,
    pub connectivity: ConnectivityRequester,
    pub node_identity: Arc<NodeIdentity>,
    pub num_rounds: Arc<AtomicUsize>,
}

impl NetworkDiscoveryContext {
    /// Increment the number of rounds by 1
    pub fn increment_num_rounds(&self) -> usize {
        self.num_rounds.fetch_add(1, Ordering::AcqRel)
    }

    /// Get the number of rounds
    pub fn num_rounds(&self) -> usize {
        self.num_rounds.load(Ordering::Relaxed)
    }

    /// Reset the number of rounds to 0
    pub fn reset_num_rounds(&self) {
        self.num_rounds.store(0, Ordering::Release);
    }
}

pub struct DhtNetworkDiscovery {
    context: NetworkDiscoveryContext,
    event_tx: broadcast::Sender<Arc<DhtEvent>>,
    shutdown_signal: ShutdownSignal,
}

impl DhtNetworkDiscovery {
    pub fn new(
        config: DhtConfig,
        node_identity: Arc<NodeIdentity>,
        peer_manager: Arc<PeerManager>,
        connectivity: ConnectivityRequester,
        event_tx: broadcast::Sender<Arc<DhtEvent>>,
        shutdown_signal: ShutdownSignal,
    ) -> Self
    {
        Self {
            context: NetworkDiscoveryContext {
                config,
                peer_manager,
                connectivity,
                node_identity,
                num_rounds: Default::default(),
            },
            event_tx,
            shutdown_signal,
        }
    }

    async fn get_next_event(&mut self, state: &mut State) -> StateEvent {
        match state {
            State::Initializing => Initializing::new(&mut self.context).next_event().await,
            State::Ready(ready) => ready.next_event().await,
            State::Discovering(discovering) => discovering.next_event().await,
            State::Waiting(idling) => idling.next_event().await,
            _ => StateEvent::Shutdown,
        }
    }

    fn transition(&mut self, current_state: State, next_event: StateEvent) -> State {
        let config = &self.config().network_discovery;
        debug!(
            target: LOG_TARGET,
            "Transition triggered from current state `{}` by event `{}`", current_state, next_event
        );
        match (current_state, next_event) {
            (State::Initializing, StateEvent::Initialized) => {
                State::Ready(DiscoveryReady::initial(self.context.clone()))
            },
            (_, StateEvent::Ready) => State::Ready(DiscoveryReady::initial(self.context.clone())),
            (State::Ready(_), StateEvent::BeginDiscovery(params)) => {
                State::Discovering(Discovering::new(params, self.context.clone()))
            },

            (State::Discovering(_), StateEvent::DiscoveryComplete(stats)) => {
                if stats.has_new_peers() {
                    self.publish_event(DhtEvent::NetworkDiscoveryPeersAdded(stats.clone()));
                }
                if !stats.is_success() {
                    return State::Waiting(self.config().network_discovery.on_failure_idle_period.into());
                }

                State::Ready(DiscoveryReady::new(self.context.clone(), stats))
            },
            (State::Ready(_), StateEvent::Idle) => State::Waiting(config.idle_period.into()),
            (_, StateEvent::Shutdown) => State::Shutdown,
            (_, StateEvent::Errored(err)) => {
                error!(
                    target: LOG_TARGET,
                    "Network discovery errored: {}. Waiting for {:.0?}", err, config.on_failure_idle_period
                );
                State::Waiting(config.on_failure_idle_period.into())
            },
            (state, event) => {
                debug!(
                    target: LOG_TARGET,
                    "No state transition for event `{}`. The current state is `{}`", event, state
                );
                state
            },
        }
    }

    fn publish_event(&self, event: DhtEvent) {
        let _ = self.event_tx.send(Arc::new(event));
    }

    #[inline]
    fn config(&self) -> &DhtConfig {
        &self.context.config
    }

    pub fn spawn(self) -> task::JoinHandle<()> {
        task::spawn(self.run())
    }

    pub async fn run(mut self) {
        if !self.config().network_discovery.enabled {
            warn!(
                target: LOG_TARGET,
                "Network discovery is disabled. This node may fail to participate in the network."
            );

            return;
        }
        let mut state = State::Initializing;
        loop {
            let shutdown_signal = self.shutdown_signal.clone();
            let next_event = {
                let fut = self.get_next_event(&mut state);
                futures::pin_mut!(fut);
                or_shutdown(shutdown_signal, fut).await
            };
            state = self.transition(state, next_event);
            if state.is_shutdown() {
                break;
            }
        }
    }
}

async fn or_shutdown<Fut>(shutdown_signal: ShutdownSignal, fut: Fut) -> StateEvent
where Fut: Future<Output = StateEvent> + Unpin {
    match future::select(shutdown_signal, fut).await {
        Either::Left(_) => StateEvent::Shutdown,
        Either::Right((event, _)) => event,
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveryParams {
    pub peers: Vec<NodeId>,
    pub num_peers_to_request: usize,
    pub max_accept_closer_peers: usize,
}

impl Display for DiscoveryParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DiscoveryParams({} peer(s) selected, num_peers_to_request = {}, max_accept_closer_peers = {})",
            self.peers.len(),
            self.num_peers_to_request,
            self.max_accept_closer_peers
        )
    }
}

#[derive(Debug, Default, Clone)]
pub struct DhtNetworkDiscoveryRoundInfo {
    pub num_new_neighbours: usize,
    pub num_new_peers: usize,
    pub num_duplicate_peers: usize,
    pub num_succeeded: usize,
    pub sync_peers: Vec<NodeId>,
}

impl DhtNetworkDiscoveryRoundInfo {
    pub fn has_new_peers(&self) -> bool {
        self.num_new_peers > 0
    }

    pub fn has_new_neighbours(&self) -> bool {
        self.num_new_neighbours > 0
    }

    /// Returns true if the round succeeded (i.e. at least one sync peer was contacted and succeeded in the protocol),
    /// otherwise false
    pub fn is_success(&self) -> bool {
        self.num_succeeded > 0
    }
}

impl Display for DhtNetworkDiscoveryRoundInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Synced {}/{}, num_new_neighbours = {}, num_new_peers = {}, num_duplicate_peers = {}",
            self.num_succeeded,
            self.sync_peers.len(),
            self.num_new_neighbours,
            self.num_new_peers,
            self.num_duplicate_peers,
        )
    }
}