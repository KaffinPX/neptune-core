use crate::application::json_rpc::core::api::rpc::RpcApi;
use crate::application::json_rpc::core::error::RpcError;
use crate::application::json_rpc::core::error::RpcResult;
use crate::application::json_rpc::core::model::message::*;
use neptune_rpc_macros::JsonRouter;
use serde::{Deserialize, Serialize};

/// API version.
pub const RPC_API_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Namespaces {
    Node,
    Networking,
    Chain,
    Mining,
    Archival,
    Mempool,
    Wallet,
}

#[derive(JsonRouter, Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpcMethods {
    #[namespace(Namespaces::Chain)]
    GetHeight,
}
