//! Browser portion of the interop test.
#![cfg(target_arch = "wasm32")]

use anyhow::Context;
use blockstore::block::Block as _;
use blockstore::Blockstore;
use blockstore::InMemoryBlockstore;
use cid::Cid;
use libp2p::futures::StreamExt;
use libp2p::request_response::Message;
use libp2p::request_response::ProtocolSupport;
use libp2p::StreamProtocol;
use libp2p::{
    core::upgrade::Version,
    identity::Keypair,
    noise,
    swarm::{NetworkBehaviour, Swarm, SwarmEvent},
    Transport as _,
};
use libp2p::{multiaddr::Protocol, Multiaddr};
use serde::{Deserialize, Serialize};
use std::ops::Deref;
use test_utils::RawBlakeBlock;
use test_utils::TEST_BLOCK_DATA;
use test_utils::{PeerRequest, PeerResponse};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_time::Duration;

const CID_SIZE: usize = 64;

#[wasm_bindgen]
pub async fn run_test_wasm(libp2p_endpoint: String) -> Result<(), JsValue> {
    tracing_wasm::set_as_global_default();
    tracing::info!(
        "Running wasm test with libp2p endpoint: {}",
        libp2p_endpoint
    );

    let mut remote_maddr =
        Multiaddr::try_from(libp2p_endpoint.clone()).expect("Failed to parse Multiaddr");

    let Some(Protocol::P2p(remote_peer_id)) = remote_maddr.pop() else {
        return Err(JsError::new("Failed to parse Multiaddr").into());
    };

    let blockstore: InMemoryBlockstore<CID_SIZE> = InMemoryBlockstore::new();

    // Add some data to the blockstore to be retrieved by the server
    let block = RawBlakeBlock(TEST_BLOCK_DATA.to_vec());
    let browser_cid = block.cid().unwrap();

    tracing::info!("Putting CID in blockstore {:?}", browser_cid);

    blockstore
        .put_keyed(&browser_cid, block.data())
        .await
        .unwrap();

    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_wasm_bindgen()
        .with_other_transport(|local_key| {
            libp2p_webrtc_websys::Transport::new(libp2p_webrtc_websys::Config::new(&local_key))
        })
        .map_err(|e| JsValue::from_str(&format!("{:?}", e)))?
        .with_behaviour(|key| test_utils::new_behaviour(key, blockstore))
        .map_err(|e| JsValue::from_str(&format!("{:?}", e)))?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(32_212_254u64)))
        .build();

    // Dial the remote peer
    swarm
        .dial(remote_maddr)
        .map_err(|e| JsValue::from_str(&format!("{:?}", e)))?;

    // Once connected, send req_res with the CID so the server can fetch the block from the browser blockstore
    // loop until connected to peer
    loop {
        match swarm.select_next_some().await {
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                if peer_id == remote_peer_id {
                    break;
                }
            }
            _ => {}
        }
    }

    swarm
        .behaviour_mut()
        .request_response
        .send_request(&remote_peer_id, PeerRequest::new(browser_cid));

    loop {
        match swarm.select_next_some().await {
            SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                if peer_id == remote_peer_id {
                    if let Some(err) = cause {
                        return Err(JsValue::from_str(&format!("{:?}", err)));
                    }
                    break;
                }
            }
            // Got the ACK PeerResponse from the server
            SwarmEvent::Behaviour(test_utils::BehaviourEvent::RequestResponse(
                libp2p::request_response::Event::Message {
                    peer,
                    message:
                        Message::Response {
                            request_id,
                            response: PeerResponse,
                        },
                },
            )) => {
                if peer == remote_peer_id {
                    tracing::info!("Received PeerResponse from server");
                }
            }
            _ => {}
        }
    }

    Ok(())
}
