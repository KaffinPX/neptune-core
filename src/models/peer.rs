use super::blockchain::{
    block::{
        block_header::{BlockHeader, PROOF_OF_WORK_COUNT_U32_SIZE},
        block_height::BlockHeight,
        transfer_block::TransferBlock,
        Block,
    },
    digest::Digest,
};
use crate::config_models::network::Network;
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};
use twenty_first::amount::u32s::U32s;

const BAD_BLOCK_BATCH_REQUEST_SEVERITY: u16 = 10;
const INVALID_BLOCK_SEVERITY: u16 = 10;
const DIFFERENT_GENESIS_SEVERITY: u16 = u16::MAX;
const SYNCHRONIZATION_TIMEOUT_SEVERITY: u16 = u16::MAX;
const FLOODED_PEER_LIST_RESPONSE_SEVERITY: u16 = 2;
const FORK_RESOLUTION_ERROR_SEVERITY_PER_BLOCK: u16 = 3;
const INVALID_MESSAGE_SEVERITY: u16 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerInfo {
    pub address_for_incoming_connections: Option<SocketAddr>,
    pub connected_address: SocketAddr,
    pub instance_id: u128,
    pub inbound: bool,
    pub last_seen: SystemTime,
    pub standing: PeerStanding,
    pub version: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum PeerSanctionReason {
    InvalidBlock((BlockHeight, Digest)),
    DifferentGenesis,
    ForkResolutionError((BlockHeight, u16, Digest)),
    SynchronizationTimeout,
    FloodPeerListResponse,
    // Be careful about using this too much as it's bad for log opportunities
    InvalidMessage,
    TooShortBlockBatch,
    ReceivedBatchBlocksOutsideOfSync,
    BatchBlocksInvalidStartHeight,
    BatchBlocksUnknownRequest,
}

/// Used by main thread to manage synchronizations/catch-up. Main thread has
/// a value of this type for each connected peer.

#[derive(Debug, Clone, Copy)]
pub struct PeerSynchronizationState {
    pub claimed_max_height: BlockHeight,
    pub claimed_max_pow_family: U32s<PROOF_OF_WORK_COUNT_U32_SIZE>,
    pub synchronization_start: SystemTime,
    pub last_request_received: Option<SystemTime>,
}

impl PeerSynchronizationState {
    pub fn new(
        claimed_max_height: BlockHeight,
        claimed_max_pow_family: U32s<PROOF_OF_WORK_COUNT_U32_SIZE>,
    ) -> Self {
        Self {
            claimed_max_height,
            claimed_max_pow_family,
            synchronization_start: SystemTime::now(),
            last_request_received: None,
        }
    }
}

impl PeerSanctionReason {
    pub fn to_severity(self) -> u16 {
        match self {
            PeerSanctionReason::InvalidBlock(_) => INVALID_BLOCK_SEVERITY,
            PeerSanctionReason::DifferentGenesis => DIFFERENT_GENESIS_SEVERITY,
            PeerSanctionReason::ForkResolutionError((_height, count, _digest)) => {
                FORK_RESOLUTION_ERROR_SEVERITY_PER_BLOCK * count
            }
            PeerSanctionReason::SynchronizationTimeout => SYNCHRONIZATION_TIMEOUT_SEVERITY,
            PeerSanctionReason::FloodPeerListResponse => FLOODED_PEER_LIST_RESPONSE_SEVERITY,
            PeerSanctionReason::InvalidMessage => INVALID_MESSAGE_SEVERITY,
            PeerSanctionReason::TooShortBlockBatch => INVALID_MESSAGE_SEVERITY,
            PeerSanctionReason::ReceivedBatchBlocksOutsideOfSync => INVALID_MESSAGE_SEVERITY,
            PeerSanctionReason::BatchBlocksInvalidStartHeight => INVALID_MESSAGE_SEVERITY,
            PeerSanctionReason::BatchBlocksUnknownRequest => BAD_BLOCK_BATCH_REQUEST_SEVERITY,
        }
    }
}

/// This is object that gets stored in the database to record how well a peer
/// at a certain IP behaves. A lower number is better.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PeerStanding {
    pub standing: u16,
    pub latest_sanction: Option<PeerSanctionReason>,
    pub timestamp_of_latest_sanction: Option<u64>,
}

impl PeerStanding {
    pub fn default() -> Self {
        Self {
            standing: 0,
            latest_sanction: None,
            timestamp_of_latest_sanction: None,
        }
    }

    /// Sanction peer and return latest standing score
    pub fn sanction(&mut self, reason: PeerSanctionReason) -> u16 {
        let (mut new_standing, overflow) = self.standing.overflowing_add(reason.to_severity());
        if overflow {
            new_standing = u16::MAX;
        }

        self.standing = new_standing;
        self.latest_sanction = Some(reason);
        self.timestamp_of_latest_sanction = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Failed to generate timestamp for peer standing")
                .as_secs(),
        );

        self.standing
    }

    /// Clear peer standing record
    pub fn clear_standing(&mut self) {
        self.standing = PeerStanding::default().standing;
        self.latest_sanction = PeerStanding::default().latest_sanction;
        self.timestamp_of_latest_sanction = PeerStanding::default().timestamp_of_latest_sanction;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HandshakeData {
    pub tip_header: BlockHeader,
    pub listen_address: Option<SocketAddr>,
    pub network: Network,
    pub instance_id: u128,
    pub version: String,
}

/// Used to tell peers that a new block has been found without having toPeerMessage
/// send the entire block
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PeerBlockNotification {
    pub hash: Digest,
    pub height: BlockHeight,
    pub proof_of_work_family: U32s<PROOF_OF_WORK_COUNT_U32_SIZE>,
}

impl From<&Block> for PeerBlockNotification {
    fn from(block: &Block) -> Self {
        PeerBlockNotification {
            hash: block.hash,
            height: block.header.height,
            proof_of_work_family: block.header.proof_of_work_family,
        }
    }
}

impl From<Block> for PeerBlockNotification {
    fn from(block: Block) -> Self {
        PeerBlockNotification {
            hash: block.hash,
            height: block.header.height,
            proof_of_work_family: block.header.proof_of_work_family,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum ConnectionRefusedReason {
    AlreadyConnected,
    BadStanding,
    MaxPeerNumberExceeded,
    SelfConnect,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum ConnectionStatus {
    Refused(ConnectionRefusedReason),
    Accepted,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum PeerMessage {
    Handshake(Box<(Vec<u8>, HandshakeData)>),
    Block(Box<TransferBlock>),
    BlockNotification(PeerBlockNotification),
    BlockRequestByHeight(BlockHeight),
    BlockRequestByHash(Digest),
    BlockRequestBatch(Vec<Digest>, usize), // TODO: Consider restricting this in size
    BlockResponseBatch(Vec<TransferBlock>), // TODO: Consider restricting this in size
    // NewTransaction(i32),
    PeerListRequest, // Argument indicates distance in graph.
    PeerListResponse(Vec<(SocketAddr, u128)>), // (socket address, instance_id)
    Bye,
    ConnectionStatus(ConnectionStatus),
}

impl PeerMessage {
    pub fn get_type(&self) -> String {
        match self {
            PeerMessage::Handshake(_) => "handshake".to_string(),
            PeerMessage::Block(_) => "block".to_string(),
            PeerMessage::BlockNotification(_) => "block notification".to_string(),
            PeerMessage::BlockRequestByHeight(_) => "block req by height".to_string(),
            PeerMessage::BlockRequestByHash(_) => "block req by hash".to_string(),
            PeerMessage::BlockRequestBatch(_, _) => "block req batch".to_string(),
            PeerMessage::BlockResponseBatch(_) => "block resp batch".to_string(),
            // PeerMessage::NewTransaction(_) => "new tx".to_string(),
            PeerMessage::PeerListRequest => "peer list req".to_string(),
            PeerMessage::PeerListResponse(_) => "peer list resp".to_string(),
            PeerMessage::Bye => "bye".to_string(),
            PeerMessage::ConnectionStatus(_) => "connection status".to_string(),
        }
    }

    pub fn ignore_when_not_sync(&self) -> bool {
        match self {
            PeerMessage::Handshake(_) => false,
            PeerMessage::Block(_) => false,
            PeerMessage::BlockNotification(_) => false,
            PeerMessage::BlockRequestByHeight(_) => false,
            PeerMessage::BlockRequestByHash(_) => false,
            PeerMessage::BlockRequestBatch(_, _) => false,
            PeerMessage::BlockResponseBatch(_) => true,
            // PeerMessage::NewTransaction(_) => false,
            PeerMessage::PeerListRequest => false,
            PeerMessage::PeerListResponse(_) => false,
            PeerMessage::Bye => false,
            PeerMessage::ConnectionStatus(_) => false,
        }
    }

    /// Function to filter out messages that should not be handled while the client is syncing
    pub fn ignore_during_sync(&self) -> bool {
        match self {
            PeerMessage::Handshake(_) => false,
            PeerMessage::Block(_) => true,
            PeerMessage::BlockNotification(_) => false,
            PeerMessage::BlockRequestByHeight(_) => false,
            PeerMessage::BlockRequestByHash(_) => false,
            PeerMessage::BlockRequestBatch(_, _) => false,
            PeerMessage::BlockResponseBatch(_) => false,
            // PeerMessage::NewTransaction(_) => true,
            PeerMessage::PeerListRequest => false,
            PeerMessage::PeerListResponse(_) => false,
            PeerMessage::Bye => false,
            PeerMessage::ConnectionStatus(_) => false,
        }
    }
}

/// `MutablePeerState` contains the part of the peer-loop's state that is mutable
#[derive(Clone, Debug)]
pub struct MutablePeerState {
    pub highest_shared_block_height: BlockHeight,
    pub fork_reconciliation_blocks: Vec<Block>,
}

impl MutablePeerState {
    pub fn new(block_height: BlockHeight) -> Self {
        Self {
            highest_shared_block_height: block_height,
            fork_reconciliation_blocks: vec![],
        }
    }
}
