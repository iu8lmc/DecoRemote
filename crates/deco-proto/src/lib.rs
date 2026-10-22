//! Protocollo condiviso tra DecoRemote client e server di rendezvous/relay.

pub mod msg;
pub mod net;
pub mod secure;
pub mod udp;

use anyhow::Result;
use bincode::Options;
use serde::{de::DeserializeOwned, Serialize};

/// Dimensione massima di un messaggio applicativo (dopo il riassemblaggio dei chunk).
pub const MAX_MESSAGE: u64 = 64 << 20;

fn opts() -> impl Options {
    bincode::options()
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .with_limit(MAX_MESSAGE)
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>> {
    Ok(opts().serialize(msg)?)
}

pub fn decode<T: DeserializeOwned>(data: &[u8]) -> Result<T> {
    Ok(opts().deserialize(data)?)
}
