use std::{net::SocketAddr, sync::Arc};

use async_trait::async_trait;
use axum::{extract::State, routing::post, Json, Router};
use serde_json::Value;
use tokio::net::TcpListener;

use crate::{
    application::json_rpc::core::{
        api::{ops::RpcMethods, rpc::RpcApi},
        error::{RpcError, RpcRequest, RpcResponse},
        model::message::*,
    },
    state::GlobalStateLock,
};

#[derive(Clone, Debug)]
pub struct RpcServer {
    pub(crate) state: GlobalStateLock,
}

impl RpcServer {
    pub fn new(state: GlobalStateLock) -> Self {
        Self { state }
    }

    pub async fn serve(&self) {
        let api: Arc<dyn RpcApi> = Arc::new(self.clone());
        let router = Router::new().route("/", post(rpc_handler)).with_state(api);
        let addr = SocketAddr::from(([127, 0, 0, 1], 3031));
        let listener = TcpListener::bind(addr).await.unwrap();

        axum::serve(listener, router).await.unwrap();
    }
}

pub async fn rpc_handler(
    State(api): State<Arc<dyn RpcApi>>,
    Json(body): Json<Value>,
) -> Json<RpcResponse> {
    let request: RpcRequest = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(_) => {
            return Json(RpcResponse::error(None, RpcError::ParseError));
        }
    };

    let res = RpcMethods::dispatch(&api, &request.method, request.params).await;

    let response = match res {
        Ok(result) => RpcResponse::success(request.id, result),
        Err(error) => RpcResponse::error(request.id, error),
    };

    Json(response)
}

#[async_trait]
impl RpcApi for RpcServer {
    async fn get_height_call(&self, _: GetHeightRequest) -> GetHeightResponse {
        GetHeightResponse {
            height: self
                .state
                .lock_guard()
                .await
                .chain
                .light_state()
                .kernel
                .header
                .height
                .into(),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use crate::{
        api::export::Network,
        application::{
            config::cli_args,
            json_rpc::{core::api::rpc::RpcApi, server::server::RpcServer},
        },
        state::wallet::wallet_entropy::WalletEntropy,
        tests::{shared::globalstate::mock_genesis_global_state, shared_tokio_runtime},
    };
    use anyhow::Result;
    use macro_rules_attr::apply;

    async fn test_rpc_server() -> RpcServer {
        let global_state_lock = mock_genesis_global_state(
            2,
            WalletEntropy::new_random(),
            cli_args::Args::default_with_network(Network::Main),
        )
        .await;

        RpcServer::new(global_state_lock)
    }

    #[apply(shared_tokio_runtime)]
    async fn test_height_is_correct() -> Result<()> {
        let rpc_server = test_rpc_server().await;
        assert_eq!(0, rpc_server.get_height().await.height);
        Ok(())
    }
}
