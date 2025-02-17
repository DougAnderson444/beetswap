use blockstore::{
    block::{Block, CidError},
    Blockstore, InMemoryBlockstore,
};
use cid::Cid;
use libp2p::{
    identity::Keypair, request_response::ProtocolSupport, swarm::NetworkBehaviour, StreamProtocol,
};
use multihash_codetable::Code;
use multihash_codetable::MultihashDigest as _;
use serde::{Deserialize, Serialize};
use std::ops::Deref;

pub const CID_SIZE: usize = 64;

/// Handshake protocol on which CID the server should hit the browser up for.
pub const HIT_ME_UP_PROTOCOL: &str = "/hmu/0.1.0";

pub const TEST_BLOCK_DATA: [u8; 5] = [69, 69, 69, 69, 69];

/// Common Behaviour construction:
pub fn new_behaviour(
    _key: &Keypair,
    blockstore: InMemoryBlockstore<CID_SIZE>,
) -> Behaviour<InMemoryBlockstore<CID_SIZE>> {
    let beetswap = beetswap::Behaviour::<CID_SIZE, _>::new(blockstore.into());
    let request_response = libp2p::request_response::cbor::Behaviour::new(
        [(
            StreamProtocol::new(HIT_ME_UP_PROTOCOL),
            ProtocolSupport::Full,
        )],
        libp2p::request_response::Config::default(),
    );

    Behaviour {
        request_response,
        bitswap: beetswap,
    }
}

#[derive(NetworkBehaviour)]
pub struct Behaviour<B: Blockstore + 'static> {
    /// Use RequestResponse to send data to a peer. Extensions can be used
    /// to encode/decode the bytes, giving users a lot of flexibility that they control.
    pub request_response: libp2p::request_response::cbor::Behaviour<PeerRequest, PeerResponse>,
    /// Bitswap
    pub bitswap: beetswap::Behaviour<CID_SIZE, B>,
}

/// Simple CID exchange protocol
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRequest(pub Vec<u8>);

impl PeerRequest {
    pub fn new(cid: Cid) -> Self {
        Self(cid.to_bytes())
    }
}

impl Deref for PeerRequest {
    type Target = Vec<u8>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Jeeves Response Bytes
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PeerResponse(());

impl Deref for PeerResponse {
    type Target = ();

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

const RAW_CODEC: u64 = 0x55;

/// A block that is just raw bytes encoded into a block
/// using the `RAW_CODEC` and `Blake3_256` hash function.
pub struct RawBlakeBlock(pub Vec<u8>);

impl Block<64> for RawBlakeBlock {
    fn cid(&self) -> Result<Cid, CidError> {
        let hash = Code::Blake3_256.digest(&self.0);
        Ok(Cid::new_v1(RAW_CODEC, hash))
    }

    fn data(&self) -> &[u8] {
        self.0.as_ref()
    }
}
