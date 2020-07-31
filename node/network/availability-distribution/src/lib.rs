// Copyright 2020 Parity Technologies (UK) Ltd.
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

//! The bitfield distribution
//!
//! In case this node is a validator, gossips its own signed availability bitfield
//! for a particular relay parent.
//! Independently of that, gossips on received messages from peers to other interested peers.

use codec::{Decode, Encode};
use futures::{channel::oneshot, FutureExt};

use keystore::KeyStorePtr;
use sc_keystore as keystore;

use node_primitives::{ProtocolId, View};

use log::{trace, warn};
use polkadot_erasure_coding::{
	branch_hash, branches, obtain_chunks_v1 as obtain_chunks, reconstruct_v1 as reconstruct,
};
use polkadot_primitives::v1::{
	AvailableData, BlakeTwo256, CommittedCandidateReceipt, CoreState, ErasureChunk,
	GlobalValidationData, Hash as H256, HashT, HeadData, Id as ParaId, LocalValidationData,
	OmittedValidationData, PoV, SigningContext, ValidatorId, ValidatorIndex, ValidatorPair,
};
use polkadot_subsystem::messages::*;
use polkadot_subsystem::{
	ActiveLeavesUpdate, FromOverseer, OverseerSignal, SpawnedSubsystem, Subsystem,
	SubsystemContext, SubsystemError,
};
use sc_network::ReputationChange;
use std::collections::{HashMap, HashSet};
use std::io;
use std::iter;

const TARGET: &'static str = "avad";

#[derive(Debug, derive_more::From)]
enum Error {
	#[from]
	Erasure(polkadot_erasure_coding::Error),
	#[from]
	Io(io::Error),
	#[from]
	Oneshot(oneshot::Canceled),
	#[from]
	Subsystem(SubsystemError),
}

type Result<T> = std::result::Result<T, Error>;

const COST_MERKLE_PROOF_INVALID: ReputationChange =
	ReputationChange::new(-100, "Bitfield signature invalid");
const COST_NOT_A_LIVE_CANDIDATE: ReputationChange =
	ReputationChange::new(-51, "Candidate is not part of the live candidates");
const COST_NOT_IN_VIEW: ReputationChange =
	ReputationChange::new(-51, "Not interested in that parent hash");
const COST_MESSAGE_NOT_DECODABLE: ReputationChange =
	ReputationChange::new(-100, "Not interested in that parent hash");
const COST_PEER_DUPLICATE_MESSAGE: ReputationChange =
	ReputationChange::new(-500, "Peer sent the same message multiple times");
const BENEFIT_VALID_MESSAGE_FIRST: ReputationChange =
	ReputationChange::new(15, "Valid message with new information");
const BENEFIT_VALID_MESSAGE: ReputationChange = ReputationChange::new(10, "Valid message");

/// Checked signed availability bitfield that is distributed
/// to other peers.
#[derive(Encode, Decode, Debug, Clone, PartialEq, Eq, Hash)]
pub struct AvailabilityGossipMessage {
	/// Ankor hash of the candidate this is associated to.
	pub candidate_hash: H256,
	/// The actual signed availability bitfield.
	pub erasure_chunk: ErasureChunk,
}

/// Data used to track information of peers and relay parents the
/// overseer ordered us to work on.
#[derive(Default, Clone)]
struct ProtocolState {
	/// Track all active peers and their views
	/// to determine what is relevant to them.
	peer_views: HashMap<PeerId, View>,

	/// Our own view.
	view: View,

	/// Caches a mapping of relay parents to live candidates
	/// and allows fast + intersection free obtaining of active heads set by unionizing.
	// relay parent -> live candidates
	cache: HashMap<H256, HashSet<CommittedCandidateReceipt>>,

	/// allow reverse caching of view checks
	/// candidate hash -> relay parent
	reverse: HashMap<H256, H256>,

	/// Track things per relay parent
	per_relay_parent: HashMap<H256, PerRelayParent>,
}

#[derive(Debug, Clone, Default)]
struct PerRelayParent {
	/// Track received candidate hashes and chunk indices from peers.
	received_messages: HashMap<PeerId, HashSet<(H256, ValidatorIndex)>>,

	/// Track already sent candidate hashes and the erasure chunk index to the peers.
	sent_messages: HashMap<PeerId, HashSet<(H256, ValidatorIndex)>>,

	/// The set of validators.
	validators: Vec<ValidatorId>,

	/// If this node is a validator, note the index in the validator set.
	validator_index: Option<ValidatorIndex>,

	/// A Candidate and a set of known erasure chunks in form of messages to be gossiped / distributed if the peer view wants that.
	/// This is _across_ peers and not specific to a particular one.
	/// candidate hash + erasure chunk index -> gossip message
	message_vault: HashMap<(H256, ValidatorIndex), AvailabilityGossipMessage>,
}

impl ProtocolState {
	/// Obtain an iterator over the actively processed relay parents.
	/// Should be equivalent to the set of relay parents stored in view View
	fn cached_relay_parents<'a>(&'a self) -> impl Iterator<Item = &'a H256> + 'a {
		debug_assert_eq!(self.cache.len(), self.view.0.len());
		self.cache.keys()
	}

	/// Unionize all cached entries for the given relay parents
	/// Ignores all non existant relay parents, so this can be used directly with a peers view.
	/// Returns a map from candidate_hash -> receipt
	fn cached_relay_parents_to_live_candidates_unioned<'a>(
		&'a self,
		relay_parents: impl IntoIterator<Item = &'a H256> + 'a,
	) -> HashMap<H256, CommittedCandidateReceipt> {
		relay_parents
			.into_iter()
			.filter_map(|relay_parent| self.cache.get(relay_parent))
			.map(|receipt_set| receipt_set.into_iter())
			.flatten()
			.map(|receipt| (candidate_hash_of(receipt), receipt.clone()))
			.collect::<HashMap<H256, CommittedCandidateReceipt>>()
	}
}

fn network_update_message(n: NetworkBridgeEvent) -> AllMessages {
    AllMessages::AvailabilityDistribution(AvailabilityDistributionMessage::NetworkBridgeUpdate(n))
}

/// Deal with network bridge updates and track what needs to be tracked
/// which depends on the message type received.
async fn handle_network_msg<Context>(
	ctx: &mut Context,
	state: &mut ProtocolState,
	bridge_message: NetworkBridgeEvent,
) -> Result<()>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	match bridge_message {
		NetworkBridgeEvent::PeerConnected(peerid, _role) => {
			// insert if none already present
			state.peer_views.entry(peerid).or_default();
		}
		NetworkBridgeEvent::PeerDisconnected(peerid) => {
			// get rid of superfluous data
			state.peer_views.remove(&peerid);
		}
		NetworkBridgeEvent::PeerViewChange(peerid, view) => {
			handle_peer_view_change(ctx, state, peerid, view).await?;
		}
		NetworkBridgeEvent::OurViewChange(view) => {
			handle_our_view_change(ctx, state, view).await?;
		}
		NetworkBridgeEvent::PeerMessage(remote, bytes) => {
			if let Ok(gossiped_availability) =
				AvailabilityGossipMessage::decode(&mut (bytes.as_slice()))
			{
				trace!(
					target: TARGET,
					"Received availability gossip from peer {:?}",
					&remote
				);
				process_incoming_peer_message(ctx, state, remote, gossiped_availability).await?;
			} else {
				modify_reputation(ctx, remote, COST_MESSAGE_NOT_DECODABLE).await?;
			}
		}
	}
	Ok(())
}

fn candidate_hash_of(receipt: &CommittedCandidateReceipt) -> H256 {
	// @todo is this correct? or is there a better candidate hash?
	receipt.descriptor().validation_data_hash.clone()
}

/// Handle the changes necassary when our view changes.
async fn handle_our_view_change<Context>(
	ctx: &mut Context,
	state: &mut ProtocolState,
	view: View,
) -> Result<()>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	let old_view = std::mem::replace(&mut (state.view), view);

	let added = state.view.difference(&old_view).collect::<Vec<_>>();

	// @todo iterate over added and query their `::K` ancestors, combine them into one hashset

	// extract all candidates by their hash
	for added in added.iter().cloned() {
		let candidates = live_candidates(ctx, std::iter::once(added.clone())).await?;

		if cfg!(debug_assert) {
			for receipt in candidates.iter() {
				debug_assert_eq!(receipt.descriptor().relay_parent, added.clone());
			}
		}

		state.cache.insert(added.clone(), candidates.clone());

		for candidate in candidates {
			state
				.reverse
				.insert(candidate_hash_of(&candidate), added.clone());
		}
	}

	// handle all candidates
	for (candidate_hash, candidate_receipt) in
		state.cached_relay_parents_to_live_candidates_unioned(added)
	{
		let added = candidate_receipt.descriptor().relay_parent;
		let desc = candidate_receipt.descriptor();
		let para = desc.para_id;

		let per_relay_parent = state
		.per_relay_parent
		.get_mut(&desc.relay_parent)
		.expect("View update message comes after overseer start work, hence must have a relay parent. qed");

		// we are a validator
		let validator_index = if let Some(validator_index) = per_relay_parent.validator_index {
			validator_index
		} else {
			continue;
		};

		// pull the proof of validity
		let pov = if let Some(pov) = query_proof_of_validity(ctx, candidate_hash.clone()).await? {
			pov
		} else {
			continue;
		};

		// perform erasure encoding
		let head_data = query_head_data(ctx, added, para).await?;

		let omitted_validation = query_omitted_validation_data(ctx, added.clone()).await?;

		let available_data = AvailableData {
			pov,
			omitted_validation,
		};

		let chunks: Vec<Vec<u8>> =
			if let Ok(chunks) = obtain_chunks(per_relay_parent.validators.len(), &available_data) {
				chunks
			} else {
				warn!("Failed to create erasure chunks");
				return Ok(());
			};

		// create proofs for each erasure chunk
		let branches = branches(chunks.as_ref());

		let erasure_chunks: Vec<ErasureChunk> = branches
			.map(|(proof, chunk)| ErasureChunk {
				chunk: chunk.to_vec(),
				index: validator_index,
				proof,
			})
			.collect();

		if let Some(erasure_chunk) = erasure_chunks.get(validator_index as usize) {
			match store_chunk(ctx, candidate_hash, validator_index, erasure_chunk.clone()).await {
				Err(e) => warn!(target: TARGET, "Failed to send store message to overseer"),
				Ok(Err(())) => warn!(target: TARGET, "Failed to store our own erasure chunk"),
				Ok(Ok(())) => {}
			}
		} else {
			warn!(
				target: TARGET,
				"Our validation index is out of bounds, no associated message"
			);
			return Ok(());
		}

		// obtain interested pHashMapeers in the candidate hash
		let peers: Vec<PeerId> = state
			.peer_views
			.iter()
			.filter(|(peer, view)| view.contains(&desc.relay_parent))
			.map(|(peer, _view)| peer.clone())
			.collect();

		// distribute all erasure messages to interested peers
		for erasure_chunk in erasure_chunks {
			// only the peers which did not receive this particular erasure chunk
			let peers = peers
				.iter()
				.filter(|peer| {
					!per_relay_parent
						.sent_messages
						.get(*peer)
						.filter(|set| {
							// peer already received this message
							set.contains(&(candidate_hash.clone(), erasure_chunk.index))
						})
						.is_some()
				})
				.map(|peer| peer.clone())
				.collect::<Vec<_>>();
			let message = AvailabilityGossipMessage {
				candidate_hash,
				erasure_chunk,
			};
			send_tracked_gossip_message_to_peers(ctx, per_relay_parent, peers, message).await?;
		}
	}

	// cleanup the removed relay parents and their states
	let removed = old_view.difference(&state.view).collect::<Vec<_>>();
	for removed in removed {
		// cleanup relay parents we are not interested in any more
		// and drop their associated
		if let Some(candidates) = state.cache.remove(&removed) {
			for candidate in candidates {
				state.reverse.remove(&candidate_hash_of(&candidate));
			}
		}
	}
	Ok(())
}

#[inline(always)]
async fn send_tracked_gossip_message_to_peers<Context>(
	ctx: &mut Context,
	per_relay_parent: &mut PerRelayParent,
	peers: Vec<PeerId>,
	message: AvailabilityGossipMessage,
) -> Result<()>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	send_tracked_gossip_messages_to_peers(ctx, per_relay_parent, peers, iter::once(message)).await
}

#[inline(always)]
async fn send_tracked_gossip_messages_to_peer<Context>(
	ctx: &mut Context,
	per_relay_parent: &mut PerRelayParent,
	peer: PeerId,
	message_iter: impl IntoIterator<Item = AvailabilityGossipMessage>,
) -> Result<()>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	send_tracked_gossip_messages_to_peers(ctx, per_relay_parent, vec![peer], message_iter).await
}

async fn send_tracked_gossip_messages_to_peers<Context>(
	ctx: &mut Context,
	per_relay_parent: &mut PerRelayParent,
	peers: Vec<PeerId>,
	message_iter: impl IntoIterator<Item = AvailabilityGossipMessage>,
) -> Result<()>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	// let message_sent_to_peer = &mut (job_data.message_sent_to_peer);
	// state.message_sent_to_peer
	// 	.entry(dest.clone())
	// 	.or_default()
	// 	.insert(validator.clone());

	for message in message_iter {
		let message_id = (message.candidate_hash.clone(), message.erasure_chunk.index);
		for peer in peers.clone() {
			per_relay_parent
				.sent_messages
				.entry(peer)
				.or_default()
				.insert(message_id.clone());
		}

		let encoded = message.encode();
		per_relay_parent.message_vault.insert(message_id, message);

		ctx.send_message(
			AllMessages::NetworkBridge(
				NetworkBridgeMessage::SendMessage(
				peers.clone(),
				AvailabilityDistributionSubsystem::PROTOCOL_ID,
				encoded,
			),
		))
		.await
		.map_err::<Error, _>(Into::into)?;
	}

	Ok(())
}

// Send the difference between two views which were not sent
// to that particular peer.
async fn handle_peer_view_change<Context>(
	ctx: &mut Context,
	state: &mut ProtocolState,
	origin: PeerId,
	view: View,
) -> Result<()>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	let current = state.peer_views.entry(origin.clone()).or_default();

	let delta_vec: Vec<H256> = (*current).difference(&view).cloned().collect();

	*current = view;

	// only contains the intersection of what we are interested and
	// the union of all relay parent's candidates.
	let delta_candidates = state.cached_relay_parents_to_live_candidates_unioned(delta_vec.iter());

	// Send all messages we've seen before and the peer is now interested
	// in to that peer.

	for (candidate_hash, receipt) in delta_candidates {
		if let Some(per_relay_parent) = state
			.per_relay_parent
			.get_mut(&receipt.descriptor().relay_parent)
		{
			// obtain the relevant chunk indices not sent yet
			let messages = ((0 as ValidatorIndex)
				..(per_relay_parent.validators.len() as ValidatorIndex))
				.into_iter()
				.filter(|erasure_chunk_index: &ValidatorIndex| {
					// check if that erasure chunk was already sent before
					if let Some(sent_set) = per_relay_parent.sent_messages.get(&origin) {
						!sent_set.contains(&(candidate_hash, *erasure_chunk_index))
					} else {
						true
					}
				})
				.filter_map(|erasure_chunk_index: ValidatorIndex| {
					// try to pick up the message from the message vault
					per_relay_parent
						.message_vault
						.get(&(candidate_hash, erasure_chunk_index))
						.cloned()
				})
				.collect::<HashSet<_>>();
			send_tracked_gossip_messages_to_peer(ctx, per_relay_parent, origin.clone(), messages)
				.await?;
		}
	}

	Ok(())
}

/// Obtain the first key with a signing key, which must be ours. We obtain the index as `ValidatorIndex`
/// If we cannot find a key in the validator set, which we could use.
fn obtain_our_validator_index<Context>(
	validators: &[ValidatorId],
	keystore: KeyStorePtr,
) -> Option<ValidatorIndex> {
	let keystore = keystore.read();
	validators.iter().enumerate().find_map(|(idx, v)| {
		keystore
			.key_pair::<ValidatorPair>(&v)
			.ok()
			.map(move |_| idx as ValidatorIndex)
	})
}

/// Handle an incoming message from a peer.
async fn process_incoming_peer_message<Context>(
	ctx: &mut Context,
	state: &mut ProtocolState,
	origin: PeerId,
	message: AvailabilityGossipMessage,
) -> Result<()>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	// reverse lookup of the relay parent
	let relay_parent = if let Some(relay_parent) = state.reverse.get(&message.candidate_hash) {
		relay_parent
	} else {
		// not in reverse lookup means nobody has it in their view
		return modify_reputation(ctx, origin, COST_NOT_IN_VIEW).await;
	};

	if !state.view.contains(&relay_parent) {
		// we don't care about this, not part of our view
		// and what is not in our view shall not be gossiped to peers
		return modify_reputation(ctx, origin, COST_NOT_IN_VIEW).await;
	}

	// obtain the set of candidates we are interested in based on our current view
	let live_candidates =
		state.cached_relay_parents_to_live_candidates_unioned(state.view.0.iter());

	// check if the candidate is of interest
	let live_candidate = if let Some(live_candidate) = live_candidates.get(&message.candidate_hash)
	{
		live_candidate
	} else {
		return modify_reputation(ctx, origin, COST_NOT_A_LIVE_CANDIDATE).await;
	};

	// check the merkle proof
	let root = &live_candidate.commitments().erasure_root;
	let anticipated_hash = if let Ok(hash) = branch_hash(
		root,
		&message.erasure_chunk.proof,
		message.erasure_chunk.index as usize,
	) {
		hash
	} else {
		return modify_reputation(ctx, origin, COST_MERKLE_PROOF_INVALID).await;
	};

	let erasure_chunk_hash = BlakeTwo256::hash(&message.erasure_chunk.chunk);
	if anticipated_hash != erasure_chunk_hash {
		return modify_reputation(ctx, origin, COST_MERKLE_PROOF_INVALID).await;
	}

	let mut per_relay_parent = state.per_relay_parent.get_mut(&relay_parent);
	let per_relay_parent = if let Some(ref mut per_relay_parent) = per_relay_parent {
		per_relay_parent
	} else {
		warn!(target: TARGET, "Missing relay parent data");
		return Ok(());
	};

	// an internal unique identifier of this message
	let message_id = (message.candidate_hash, message.erasure_chunk.index);

	// check if this particular erasure chunk was already sent by that peer before
	{
		let received_set = per_relay_parent
			.received_messages
			.entry(origin.clone())
			.or_default();
		if received_set.contains(&message_id) {
			return modify_reputation(ctx, origin, COST_PEER_DUPLICATE_MESSAGE).await;
		} else {
			received_set.insert(message_id.clone());
		}
	}

	// insert into known messages and change reputation
	if per_relay_parent
		.message_vault
		.insert(message_id.clone(), message.clone())
		.is_none()
	{
		modify_reputation(ctx, origin, BENEFIT_VALID_MESSAGE).await?;
	} else {
		modify_reputation(ctx, origin, BENEFIT_VALID_MESSAGE_FIRST).await?;
	};

	// condense the peers to the peers with interest on the candidate
	let peers = state
		.peer_views
		.iter()
		.filter(|(_peer, view)| {
			// peers view must contain the relay parent that is associated to the candidate hash
			view.contains(&relay_parent)
		})
		.filter(|(peer, view)| {
			// quirk to make rustc quiet
			let peer: &PeerId = peer;
			let peer: PeerId = peer.clone();
			// avoid sending duplicate messages
			per_relay_parent
				.sent_messages
				.entry(peer)
				.or_default()
				.contains(&message_id)
		})
		.map(|(peer, _)| -> PeerId { peer.clone() })
		.collect::<Vec<_>>();

	// gossip that message to interested peers
	send_tracked_gossip_message_to_peers(ctx, per_relay_parent, peers, message).await
}

/// The bitfield distribution subsystem.
pub struct AvailabilityDistributionSubsystem {
	/// Pointer to a keystore, which is required for determining this nodes validator index.
	keystore: KeyStorePtr,
}

impl AvailabilityDistributionSubsystem {
	/// The protocol identifier for bitfield distribution.
	const PROTOCOL_ID: ProtocolId = *b"avad";

	/// Number of ancestors to keep around for the relay-chain heads.
	const K: usize = 3;

	/// Create a new instance of the availability distribution.
	fn new(keystore: KeyStorePtr) -> Self {
		Self { keystore }
	}

	/// Start processing work as passed on from the Overseer.
	async fn run<Context>(self, mut ctx: Context) -> Result<()>
	where
		Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
	{
		// startup: register the network protocol with the bridge.
		ctx.send_message(AllMessages::NetworkBridge(
			NetworkBridgeMessage::RegisterEventProducer(Self::PROTOCOL_ID, network_update_message),
		))
		.await
		.map_err::<Error, _>(Into::into)?;

		// work: process incoming messages from the overseer.
		let mut state = ProtocolState::default();
		loop {
			let message = ctx.recv().await.map_err::<Error, _>(Into::into)?;
			match message {
				FromOverseer::Communication {
					msg: AvailabilityDistributionMessage::NetworkBridgeUpdate(event),
				} => {
					trace!(target: TARGET, "Processing NetworkMessage");
					// a network message was received
					if let Err(e) = handle_network_msg(&mut ctx, &mut state, event).await {
						warn!(
							target: TARGET,
							"Failed to handle incomming network messages: {:?}", e
						);
					}
				}
				FromOverseer::Communication {
					msg: AvailabilityDistributionMessage::DistributeChunk(_hash, _erasure_chunk),
				} => {
					trace!(target: TARGET, "Processing incoming erasure chunk");
				}
				FromOverseer::Communication {
					msg: AvailabilityDistributionMessage::FetchChunk(_hash, _erasure_chunk),
				} => {
					trace!(target: TARGET, "Processing incoming erasure chunk");
				}
				FromOverseer::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
					activated,
					deactivated,
				})) => {
					for relay_parent in activated {
						trace!(target: TARGET, "Start {:?}", relay_parent);
						let per_relay_parent = state
							.per_relay_parent
							.entry(relay_parent.clone())
							.or_default();
						let validators = query_validators(&mut ctx, relay_parent).await?;
						per_relay_parent.validator_index = obtain_our_validator_index::<Context>(
							&validators,
							self.keystore.clone(),
						);
						per_relay_parent.validators = validators;
					}
					for relay_parent in deactivated {
						trace!(target: TARGET, "Stop {:?}", relay_parent);
					}
				}
				FromOverseer::Signal(OverseerSignal::BlockFinalized(_)) => {}
				FromOverseer::Signal(OverseerSignal::Conclude) => {
					trace!(target: TARGET, "Conclude");
					return Ok(());
				}
			}
		}
	}
}

impl<Context> Subsystem<Context> for AvailabilityDistributionSubsystem
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage> + Sync + Send,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		SpawnedSubsystem {
			name: "availability-distribution",
			future: Box::pin(async move { self.run(ctx) }.map(|_| ())),
		}
	}
}

/// Obtain all live candidates based on a iterator of relay heads.
async fn live_candidates<Context>(
	ctx: &mut Context,
	relay_parents: impl IntoIterator<Item = H256>,
) -> Result<HashSet<CommittedCandidateReceipt>>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	let iter = relay_parents.into_iter();
	let hint = iter.size_hint();

	let mut live_candidates = HashSet::with_capacity(hint.1.unwrap_or(hint.0));
	for relay_parent in iter {
		let paras = query_para_ids(ctx, relay_parent.clone()).await?;
		for para in paras {
			if let Some(ccr) = query_pending_availability(ctx, relay_parent, para).await? {
				live_candidates.insert(ccr);
			}
		}
	}
	Ok(live_candidates)
}

// @todo move these into util.rs

async fn query_para_ids<Context>(ctx: &mut Context, relay_parent: H256) -> Result<Vec<ParaId>>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	let (tx, rx) = oneshot::channel();
	ctx.send_message(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
		relay_parent,
		RuntimeApiRequest::AvailabilityCores(tx),
	)))
	.await
	.map_err::<Error, _>(Into::into)?;

	let all_para_ids = rx.await.map_err::<Error, _>(Into::into)?;

	let occupied_para_ids = all_para_ids
		.into_iter()
		.filter_map(|core_state| {
			if let CoreState::Occupied(occupied) = core_state {
				Some(occupied.para_id)
			} else {
				None
			}
		})
		.collect();
	Ok(occupied_para_ids)
}

/// Modify the reputation of a peer based on its behaviour.
async fn modify_reputation<Context>(
	ctx: &mut Context,
	peer: PeerId,
	rep: ReputationChange,
) -> Result<()>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	trace!(
		target: TARGET,
		"Reputation change of {:?} for peer {:?}",
		rep,
		peer
	);
	ctx.send_message(AllMessages::NetworkBridge(
		NetworkBridgeMessage::ReportPeer(peer, rep),
	))
	.await
	.map_err::<Error, _>(Into::into)
}

async fn query_proof_of_validity<Context>(
	ctx: &mut Context,
	candidate_hash: H256,
) -> Result<Option<PoV>>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	let (tx, rx) = oneshot::channel();
	ctx.send_message(AllMessages::AvailabilityStore(
		AvailabilityStoreMessage::QueryPoV(candidate_hash, tx),
	))
	.await
	.map_err::<Error, _>(Into::into)?;
	rx.await.map_err::<Error, _>(Into::into)
}

async fn query_stored_chunk<Context>(
	ctx: &mut Context,
	candidate_hash: H256,
	validator_index: ValidatorIndex,
) -> Result<Option<ErasureChunk>>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	let (tx, rx) = oneshot::channel();
	ctx.send_message(AllMessages::AvailabilityStore(
		AvailabilityStoreMessage::QueryChunk(candidate_hash, validator_index, tx),
	))
	.await
	.map_err::<Error, _>(Into::into)?;
	rx.await.map_err::<Error, _>(Into::into)
}

async fn store_chunk<Context>(
	ctx: &mut Context,
	candidate_hash: H256,
	validator_index: ValidatorIndex,
	erasure_chunk: ErasureChunk,
) -> Result<std::result::Result<(), ()>>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	let (tx, rx) = oneshot::channel();
	ctx.send_message(AllMessages::AvailabilityStore(
		AvailabilityStoreMessage::StoreChunk(candidate_hash, validator_index, erasure_chunk, tx),
	))
	.await
	.map_err::<Error, _>(Into::into)?;
	rx.await.map_err::<Error, _>(Into::into)
}

/// Request the head data for a particular para.
async fn query_head_data<Context>(
	ctx: &mut Context,
	relay_parent: H256,
	para: ParaId,
) -> Result<HeadData>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	let (tx, rx) = oneshot::channel();
	ctx.send_message(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
		relay_parent,
		RuntimeApiRequest::HeadData(para, tx),
	)))
	.await
	.map_err::<Error, _>(Into::into)?;
	rx.await.map_err::<Error, _>(Into::into)
}

/// Request the head data for a particular para.
async fn query_pending_availability<Context>(
	ctx: &mut Context,
	relay_parent: H256,
	para: ParaId,
) -> Result<Option<CommittedCandidateReceipt>>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	let (tx, rx) = oneshot::channel();
	ctx.send_message(AllMessages::RuntimeApi(RuntimeApiMessage::Request(
		relay_parent,
		RuntimeApiRequest::CandidatePendingAvailability(para, tx),
	)))
	.await
	.map_err::<Error, _>(Into::into)?;
	rx.await.map_err::<Error, _>(Into::into)
}

/// Query the validator set.
async fn query_validators<Context>(
	ctx: &mut Context,
	relay_parent: H256,
) -> Result<Vec<ValidatorId>>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	let (tx, rx) = oneshot::channel();
	let query_validators = AllMessages::RuntimeApi(RuntimeApiMessage::Request(
		relay_parent.clone(),
		RuntimeApiRequest::Validators(tx),
	));

	ctx.send_message(query_validators)
		.await
		.map_err::<Error, _>(Into::into)?;
	rx.await.map_err::<Error, _>(Into::into)
}

/// Query omitted validation data.
#[cfg(feature = "std")]
async fn query_omitted_validation_data<Context>(
	ctx: &mut Context,
	relay_parent: H256,
) -> Result<OmittedValidationData>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	let global_validation = query_global_validation_data(ctx, relay_parent.clone()).await?;
	let local_validation = query_local_validation_data(ctx, relay_parent).await?;
	Ok(OmittedValidationData {
		global_validation,
		local_validation,
	})
}

/// Query omitted validation data.
// @todo stub
#[cfg(feature = "std")]
async fn query_global_validation_data<Context>(
	ctx: &mut Context,
	relay_parent: H256,
) -> Result<GlobalValidationData>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	// @todo
	Ok(GlobalValidationData::default())
}

/// Query local validation data.
// @todo stub
#[cfg(feature = "std")]
async fn query_local_validation_data<Context>(
	ctx: &mut Context,
	relay_parent: H256,
) -> Result<LocalValidationData>
where
	Context: SubsystemContext<Message = AvailabilityDistributionMessage>,
{
	// @todo
	Ok(LocalValidationData::default())
}

#[cfg(test)]
mod test {
	use super::*;
	use assert_matches::assert_matches;
	use bitvec::bitvec;
	use maplit::hashmap;
	use polkadot_primitives::v1::{
		AvailableData, BlockData, CandidateDescriptor, GlobalValidationData, GroupIndex, HeadData,
		LocalValidationData, OccupiedCore, OmittedValidationData, PoV, Signed, ValidatorPair,
	};
	use polkadot_subsystem::test_helpers::{self, make_subsystem_context};

	use futures::{channel::oneshot, executor, future, Future};
	use smol_timeout::TimeoutExt;
	use sp_core::crypto::Pair;
	use std::time::Duration;

	macro_rules! view {
		( $( $hash:expr ),* $(,)? ) => [
			View(vec![ $( $hash.clone() ),* ])
		];
	}

	macro_rules! peers {
		( $( $peer:expr ),* $(,)? ) => [
			vec![ $( $peer.clone() ),* ]
		];
	}

	macro_rules! launch {
		($fut:expr) => {
			$fut.timeout(Duration::from_millis(10))
				.await
				.expect("10ms is more than enough for sending messages.")
				.expect("Launching a message to the overseer must never fail.")
		};
	}

	struct TestHarness {
		virtual_overseer: test_helpers::TestSubsystemContextHandle<AvailabilityDistributionMessage>,
	}

	struct TestState {
		global_validation_schedule: GlobalValidationData,
		local_validation_data: LocalValidationData,
	}

	impl Default for TestState {
		fn default() -> Self {
			let local_validation_data = LocalValidationData {
				parent_head: HeadData(vec![7, 8, 9]),
				balance: Default::default(),
				code_upgrade_allowed: None,
				validation_code_hash: Default::default(),
			};

			let global_validation_schedule = GlobalValidationData {
				max_code_size: 1000,
				max_head_data_size: 1000,
				block_number: Default::default(),
			};

			Self {
				local_validation_data,
				global_validation_schedule,
			}
		}
	}

	fn test_harness<T: Future<Output = ()>>(
		mut keystore: KeyStorePtr,
		test: impl FnOnce(TestHarness) -> T,
	) {
		let pool = sp_core::testing::TaskExecutor::new();
		let (context, virtual_overseer) = test_helpers::make_subsystem_context(pool.clone());

		let subsystem = AvailabilityDistributionSubsystem::new(keystore);
		let subsystem = subsystem.run(context);

		let test_fut = test(TestHarness { virtual_overseer });

		futures::pin_mut!(test_fut);
		futures::pin_mut!(subsystem);

		executor::block_on(future::select(test_fut, subsystem));
	}


	async fn overseer_send(overseer: &mut test_helpers::TestSubsystemContextHandle<AvailabilityDistributionMessage>, msg: AvailabilityDistributionMessage) {
		overseer.send(FromOverseer::Communication { msg })
			.timeout(Duration::from_millis(10))
					.await
					.expect("10ms is more than enough for sending messages.");
	}

	async fn overseer_recv(overseer:  &mut test_helpers::TestSubsystemContextHandle<AvailabilityDistributionMessage> ) -> AllMessages {
		let msg = overseer
			.recv()
			.await;
		msg
	}

	fn dummy_occupied_core(para: ParaId) -> CoreState {
		CoreState::Occupied(OccupiedCore {
			para_id: para,
			next_up_on_available: None,
			occupied_since: 0,
			time_out_at: 5,
			next_up_on_time_out: None,
			availability: Default::default(),
			group_responsible: GroupIndex::from(0),
		})
	}

	fn generate_n_validators(n: usize) -> Vec<ValidatorId> {
		iter::repeat(n)
			.map(|_| {
				let validator_pair = ValidatorPair::generate();
				validator_pair.0.public()
			})
			.collect()
	}

	fn valid_availability_gossip(validator_count: usize) -> AvailabilityGossipMessage {
		unimplemented!("noty yet, not just yet")
	}

	#[test]
	fn magic_mike() {
		let keystore = keystore::Store::new_in_memory();

		// @todo fails with invalid seed
		// let validator_keypair: ValidatorPair = keystore
		// 	.write()
		// 	.insert("seed str")
		// 	.expect("Must be able to generate keypar internally.");

		// create a validator set of 3 + us
		let validators = generate_n_validators(3);
		// let our_public = validator_keypair.public();
		// validators.push(our_public);
		test_harness(keystore, |test_harness| async move {
			let TestHarness {
				mut virtual_overseer,
			} = test_harness;
			let validator_index = 5;

			let relay_parent_x = H256::repeat_byte(0x01);
			let relay_parent_y = H256::repeat_byte(0x02);

			let chunk = ErasureChunk {
				chunk: vec![1, 2, 3],
				index: validator_index,
				proof: vec![vec![3, 4, 5]],
			};

			let peer_a = PeerId::from_bytes(vec!['a' as u8]).unwrap();
			let peer_b = PeerId::from_bytes(vec!['b' as u8]).unwrap();

			let para_27 = ParaId::from(27);
			let para_81 = ParaId::from(81);

			// make us interested in parents x and y
			overseer_send(
				&mut virtual_overseer,
					AvailabilityDistributionMessage::NetworkBridgeUpdate(
						NetworkBridgeEvent::OurViewChange(
							view![
								relay_parent_x,
								relay_parent_y
							]
						)
					)
			).await;

			// obtain the validators per relay parent
			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Validators(tx),
				)) => {
					assert_eq!(relay_parent, relay_parent_x);
					let _ = tx.send(validators.clone());
				}
			);

			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::Validators(tx),
				)) => {
					assert_eq!(relay_parent, relay_parent_y);
					let _ = tx.send(validators.clone());
				}
			);

			// subsystem peer id collection
			// which will query the availability cores
			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::AvailabilityCores(tx)
				)) => {
					assert_eq!(relay_parent, relay_parent_x);
					// respond with a set of availability core states
					let _ = tx.send(vec![
						dummy_occupied_core(para_27),
						dummy_occupied_core(para_81)
					]);
				}
			);

			// now each of those will be queried for candidate pending availability
			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::CandidatePendingAvailability(para, tx)
				)) => {
					assert_eq!(relay_parent, relay_parent_x);
					assert_eq!(para, para_27);
					let _ = tx.send(Some(CommittedCandidateReceipt {
						descriptor: CandidateDescriptor {
							para_id: para,
							relay_parent,
							.. Default::default()
						},
						.. Default::default()
					}));
				}
			);

			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					relay_parent,
					RuntimeApiRequest::CandidatePendingAvailability(para, tx)
				)) => {
					assert_eq!(relay_parent, relay_parent_y);
					assert_eq!(para, para_81);
					let _ = tx.send(Some(CommittedCandidateReceipt {
						descriptor: CandidateDescriptor {
							para_id: para,
							relay_parent,
							.. Default::default()
						},
						.. Default::default()
					}));
				}
			);

			// setup peer a with interest in parent x
			overseer_send(
				&mut virtual_overseer,
				AvailabilityDistributionMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerConnected(
						peer_a.clone(),
						ObservedRole::Full
					))
			).await;

			overseer_send(
				&mut virtual_overseer,
					AvailabilityDistributionMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerViewChange(
						peer_a.clone(),
						view![relay_parent_x]
					))
			).await;

			// setup peer b with interest in parent y
			overseer_send(
				&mut virtual_overseer,
					AvailabilityDistributionMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerConnected(
						peer_b.clone(),
						ObservedRole::Full
					))
			).await;

			overseer_send(
				&mut virtual_overseer,
					AvailabilityDistributionMessage::NetworkBridgeUpdate(NetworkBridgeEvent::PeerViewChange(
						peer_b.clone(),
						view![relay_parent_y]
					))
			).await;

			/////////////////////////////////////////////////////////
			// ready for action

			// check if garbage messages are detected and peer rep is changed as expected
			let garbage = b"I am garbage";

			overseer_send(
				&mut virtual_overseer,
				AvailabilityDistributionMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::PeerMessage(
						peer_b.clone(),
						// AvailabilityDistributionSubsystem::PROTOCOL_ID,
						garbage.to_vec()
					)
				)
			).await;

			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::NetworkBridge(
					NetworkBridgeMessage::ReportPeer(
						peer,
						rep
					)
				) => {
					assert_eq!(peer, peer_b);
					assert_eq!(rep, COST_MESSAGE_NOT_DECODABLE);
				}
			);

			let valid: AvailabilityGossipMessage = valid_availability_gossip(validators.len());

			// valid (first)
			overseer_send(
				&mut virtual_overseer,
				AvailabilityDistributionMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::PeerMessage(
						peer_b.clone(),
						valid.encode()
					))
			).await;

			assert_matches!(
				overseer_recv(&mut virtual_overseer).await,
				AllMessages::NetworkBridge(
					NetworkBridgeMessage::ReportPeer(
						peer,
						rep
					)
				) => {
					assert_eq!(peer, peer_b);
					assert_eq!(rep, BENEFIT_VALID_MESSAGE_FIRST);
				}
			);
		});
	}
}
