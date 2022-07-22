use crate::models::blockchain::block::block_height::BlockHeight;
use crate::models::blockchain::digest::{Digest, Hashable};
use crate::models::peer::PeerInfo;
use crate::models::state::State;
use futures::future::{self, Ready};
use std::net::SocketAddr;
use tarpc::context;

#[tarpc::service]
pub trait RPC {
    /// Returns the current block height.
    async fn block_height() -> BlockHeight;

    /// Returns info about the peers we are connected to
    async fn get_peer_info() -> Vec<PeerInfo>;

    /// Returns the digest of the latest block
    async fn head() -> Digest;
}

#[derive(Clone)]
pub struct NeptuneRPCServer {
    pub socket_address: SocketAddr,
    pub state: State,
}

impl RPC for NeptuneRPCServer {
    type BlockHeightFut = Ready<BlockHeight>;
    type GetPeerInfoFut = Ready<Vec<PeerInfo>>;
    type HeadFut = Ready<Digest>;

    fn block_height(self, _: context::Context) -> Self::BlockHeightFut {
        // let mut databases = executor::block_on(self.state.block_databases.lock());
        // let lookup_res = databases.latest_block_header.get(());
        let latest_block = self.state.chain.light_state.get_latest_block_header();
        future::ready(latest_block.height)
    }

    fn head(self, _: context::Context) -> Ready<Digest> {
        let latest_block = self.state.chain.light_state.get_latest_block_header();
        future::ready(latest_block.hash())
    }

    fn get_peer_info(self, _: context::Context) -> Self::GetPeerInfoFut {
        let peer_map = self
            .state
            .net
            .peer_map
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();

        future::ready(peer_map)
    }
}
