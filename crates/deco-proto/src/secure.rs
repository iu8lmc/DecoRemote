//! Canale cifrato end-to-end tra host e viewer (Noise NNpsk0).
//!
//! Il PSK deriva dalla password dell'host tramite Argon2id, quindi una password
//! errata fa fallire l'handshake. Il server di relay vede solo byte cifrati.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use argon2::Argon2;
use serde::{de::DeserializeOwned, Serialize};
use snow::{Builder, StatelessTransportState};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{decode, encode, MAX_MESSAGE};

const NOISE_PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
/// Plaintext massimo per chunk (il limite Noise è 65535 incluso il tag da 16 byte).
const CHUNK: usize = 60_000;

pub fn derive_psk(password: &str, host_id: &str) -> Result<[u8; 32]> {
    let salt = format!("decoremote-v1:{host_id}");
    let mut psk = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt.as_bytes(), &mut psk)
        .map_err(|e| anyhow!("derivazione chiave fallita: {e}"))?;
    Ok(psk)
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    let mut buf = Vec::with_capacity(2 + data.len());
    buf.extend_from_slice(&(data.len() as u16).to_be_bytes());
    buf.extend_from_slice(data);
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> Result<()> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await?;
    buf.resize(u16::from_be_bytes(len) as usize, 0);
    r.read_exact(buf).await?;
    Ok(())
}

/// Lettore/scrittore con tipo cancellato, per usare la stessa logica su TCP e su QUIC.
pub type BoxR = Box<dyn AsyncRead + Send + Unpin>;
pub type BoxW = Box<dyn AsyncWrite + Send + Unpin>;

/// Ogni "corsia" (stream) di una stessa sessione usa un intervallo di nonce diverso, così più stream
/// possono condividere le stesse chiavi senza riusare un nonce.
pub const VIDEO_LANE_NONCE_BASE: u64 = 1 << 40;

pub struct SecureReader<R> {
    r: R,
    st: Arc<StatelessTransportState>,
    nonce: u64,
    cipher: Vec<u8>,
    plain: Vec<u8>,
}

pub struct SecureWriter<W> {
    w: W,
    st: Arc<StatelessTransportState>,
    nonce: u64,
    frame: Vec<u8>,
}

fn split<R, W>(r: R, w: W, st: StatelessTransportState) -> (SecureReader<R>, SecureWriter<W>) {
    let st = Arc::new(st);
    (
        SecureReader { r, st: st.clone(), nonce: 0, cipher: Vec::new(), plain: vec![0; 65_535] },
        SecureWriter { w, st, nonce: 0, frame: vec![0; 2 + 65_535] },
    )
}

/// Lato viewer: invia il primo messaggio Noise.
pub async fn handshake_initiator<R, W>(
    mut r: R,
    mut w: W,
    psk: &[u8; 32],
) -> Result<(SecureReader<R>, SecureWriter<W>)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut hs = Builder::new(NOISE_PARAMS.parse()?).psk(0, psk)?.build_initiator()?;
    let mut buf = vec![0u8; 65_535];
    let n = hs.write_message(&[], &mut buf)?;
    write_frame(&mut w, &buf[..n]).await?;

    let mut inbound = Vec::new();
    read_frame(&mut r, &mut inbound).await.context("handshake: l'host ha chiuso la connessione, quasi sempre perché la password è errata")?;
    hs.read_message(&inbound, &mut buf).context("handshake: password errata")?;
    Ok(split(r, w, hs.into_stateless_transport_mode()?))
}

/// Lato host: attende il primo messaggio Noise del viewer.
pub async fn handshake_responder<R, W>(
    mut r: R,
    mut w: W,
    psk: &[u8; 32],
) -> Result<(SecureReader<R>, SecureWriter<W>)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut hs = Builder::new(NOISE_PARAMS.parse()?).psk(0, psk)?.build_responder()?;
    let mut buf = vec![0u8; 65_535];
    let mut inbound = Vec::new();
    read_frame(&mut r, &mut inbound).await?;
    hs.read_message(&inbound, &mut buf).context("handshake: password errata")?;
    let n = hs.write_message(&[], &mut buf)?;
    write_frame(&mut w, &buf[..n]).await?;
    Ok(split(r, w, hs.into_stateless_transport_mode()?))
}

/// Chiavi della sessione, da cui creare più corsie (stream) anche dopo che lettore e scrittore
/// principali sono stati usati: serve al viewer, che scopre la stream video solo quando arriva.
#[derive(Clone)]
pub struct Lanes(Arc<StatelessTransportState>);

impl Lanes {
    pub fn reader<R>(&self, r: R, base_nonce: u64) -> SecureReader<R> {
        SecureReader { r, st: self.0.clone(), nonce: base_nonce, cipher: Vec::new(), plain: vec![0; 65_535] }
    }

    pub fn writer<W>(&self, w: W, base_nonce: u64) -> SecureWriter<W> {
        SecureWriter { w, st: self.0.clone(), nonce: base_nonce, frame: vec![0; 2 + 65_535] }
    }
}

impl<W> SecureWriter<W> {
    pub fn lanes(&self) -> Lanes {
        Lanes(self.st.clone())
    }

    /// Nuova corsia sulle stesse chiavi, con un proprio contatore di nonce che parte da `base_nonce`.
    pub fn lane<W2>(&self, w: W2, base_nonce: u64) -> SecureWriter<W2> {
        SecureWriter { w, st: self.st.clone(), nonce: base_nonce, frame: vec![0; 2 + 65_535] }
    }
}

impl<R> SecureReader<R> {
    pub fn lanes(&self) -> Lanes {
        Lanes(self.st.clone())
    }

    /// Corsia corrispondente a `SecureWriter::lane` sul lato opposto.
    pub fn lane<R2>(&self, r: R2, base_nonce: u64) -> SecureReader<R2> {
        SecureReader { r, st: self.st.clone(), nonce: base_nonce, cipher: Vec::new(), plain: vec![0; 65_535] }
    }
}

impl<W: AsyncWrite + Unpin> SecureWriter<W> {
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        let data = encode(msg)?;
        let mut chunks = data.chunks(CHUNK).peekable();
        let mut plain = Vec::with_capacity(CHUNK + 1);
        while let Some(chunk) = chunks.next() {
            plain.clear();
            plain.push(chunks.peek().is_none() as u8);
            plain.extend_from_slice(chunk);
            let n = self.st.write_message(self.nonce, &plain, &mut self.frame[2..])?;
            self.nonce += 1;
            self.frame[..2].copy_from_slice(&(n as u16).to_be_bytes());
            self.w.write_all(&self.frame[..2 + n]).await?;
        }
        self.w.flush().await?;
        Ok(())
    }
}

impl<R: AsyncRead + Unpin> SecureReader<R> {
    /// Non cancel-safe: usare in un task dedicato, mai dentro `select!`.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T> {
        let mut acc = Vec::new();
        loop {
            read_frame(&mut self.r, &mut self.cipher).await?;
            let n = self.st.read_message(self.nonce, &self.cipher, &mut self.plain)?;
            self.nonce += 1;
            if n == 0 {
                bail!("frame vuoto");
            }
            acc.extend_from_slice(&self.plain[1..n]);
            if acc.len() as u64 > MAX_MESSAGE {
                bail!("messaggio troppo grande");
            }
            if self.plain[0] == 1 {
                break;
            }
        }
        decode(&acc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::HostMsg;

    async fn pair(pw_a: &str, pw_b: &str) -> (Result<()>, Result<()>) {
        let (a, b) = tokio::io::duplex(1 << 20);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let psk_a = derive_psk(pw_a, "123").unwrap();
        let psk_b = derive_psk(pw_b, "123").unwrap();
        let host = tokio::spawn(async move {
            let (mut r, mut w) = handshake_responder(br, bw, &psk_b).await?;
            let m: HostMsg = r.recv().await?;
            w.send(&m).await
        });
        let viewer = async {
            let (mut r, mut w) = handshake_initiator(ar, aw, &psk_a).await?;
            let big = HostMsg::Frame { width: 1, height: 1, jpeg: vec![7; 300_000] };
            w.send(&big).await?;
            match r.recv::<HostMsg>().await? {
                HostMsg::Frame { jpeg, .. } if jpeg.len() == 300_000 => Ok(()),
                other => bail!("risposta inattesa: {other:?}"),
            }
        };
        let v = viewer.await;
        (v, host.await.unwrap())
    }

    #[tokio::test]
    async fn roundtrip_large_message() {
        let (v, h) = pair("segreta", "segreta").await;
        v.unwrap();
        h.unwrap();
    }

    #[tokio::test]
    async fn wrong_password_fails() {
        let (v, _) = pair("segreta", "sbagliata").await;
        assert!(v.is_err());
    }
}
