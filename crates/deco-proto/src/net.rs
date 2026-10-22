//! Framing in chiaro (u32 BE + bincode) usato solo per parlare col server.

use anyhow::{bail, Result};
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{decode, encode};

const MAX_PLAIN: usize = 64 * 1024;

pub async fn send_plain<W, T>(w: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let data = encode(msg)?;
    if data.len() > MAX_PLAIN {
        bail!("messaggio troppo grande");
    }
    let mut buf = Vec::with_capacity(4 + data.len());
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(&data);
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

pub async fn recv_plain<R, T>(r: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_PLAIN {
        bail!("messaggio troppo grande");
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data).await?;
    decode(&data)
}
