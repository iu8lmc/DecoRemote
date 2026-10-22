//! Scoperta dell'indirizzo UDP pubblico (come STUN, ma sulla stessa porta del server).
//!
//! Il client invia `DRQ1 + nonce`; il server risponde `DRP1 + nonce + indirizzo osservato`.
//! L'indirizzo osservato serve all'altro peer per il collegamento diretto (hole punching).

use std::{
    io,
    net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket},
    time::Duration,
};

use crate::msg::UdpOffer;

const REQ: &[u8; 4] = b"DRQ1";
const RSP: &[u8; 4] = b"DRP1";

/// Cicla sul socket rispondendo alle richieste di scoperta. Termina solo in caso di errore.
pub async fn serve_discovery(sock: tokio::net::UdpSocket) -> io::Result<()> {
    let mut buf = [0u8; 64];
    loop {
        let (n, peer) = sock.recv_from(&mut buf).await?;
        if n != 12 || &buf[..4] != REQ {
            continue; // rumore o scansioni: ignorate
        }
        let mut reply = Vec::with_capacity(64);
        reply.extend_from_slice(RSP);
        reply.extend_from_slice(&buf[4..12]);
        reply.extend_from_slice(peer.to_string().as_bytes());
        let _ = sock.send_to(&reply, peer).await;
    }
}

/// Risolve "host:porta" scegliendo un indirizzo IPv4.
pub fn resolve_v4(server: &str) -> io::Result<SocketAddr> {
    server
        .to_socket_addrs()?
        .find(SocketAddr::is_ipv4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "nessun indirizzo IPv4 per il server"))
}

/// Chiede al server l'indirizzo con cui `sock` è visto da fuori. Bloccante: usare in `spawn_blocking`.
pub fn observed_addr(sock: &UdpSocket, server: SocketAddr) -> io::Result<SocketAddr> {
    let mut nonce = [0u8; 8];
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    nonce.copy_from_slice(&((t.as_nanos() as u64) ^ ((sock.local_addr()?.port() as u64) << 48)).to_be_bytes());
    let mut req = Vec::with_capacity(12);
    req.extend_from_slice(REQ);
    req.extend_from_slice(&nonce);

    let previous = sock.read_timeout()?;
    sock.set_read_timeout(Some(Duration::from_millis(400)))?;
    let mut result = Err(io::Error::new(io::ErrorKind::TimedOut, "il server non risponde via UDP"));
    let mut buf = [0u8; 128];
    'attempts: for _ in 0..3 {
        sock.send_to(&req, server)?;
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, from)) if from == server && n > 12 && &buf[..4] == RSP && buf[4..12] == nonce => {
                    result = std::str::from_utf8(&buf[12..n])
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "risposta UDP non valida"));
                    break 'attempts;
                }
                Ok(_) => continue, // pacchetto estraneo: si continua ad attendere nel tempo residuo
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => break,
                // Windows segnala con ConnectionReset un ICMP "porta irraggiungibile" di un invio precedente.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => break,
                Err(e) => {
                    result = Err(e);
                    break 'attempts;
                }
            }
        }
    }
    let _ = sock.set_read_timeout(previous);
    result
}

/// Esito della verifica del NAT: come cambia l'indirizzo pubblico al variare della destinazione.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NatReport {
    /// Indirizzo pubblico visto dalla prima porta UDP del server.
    pub observed: SocketAddr,
    /// Indirizzo visto dalla seconda porta (porta + 1), se il server la offre.
    pub second: Option<SocketAddr>,
}

impl NatReport {
    /// `Some(true)` se il NAT riusa lo stesso mapping per destinazioni diverse (il collegamento diretto è
    /// possibile), `Some(false)` se lo cambia (NAT simmetrico), `None` se non si può stabilire.
    pub fn punchable(&self) -> Option<bool> {
        self.second.map(|s| s == self.observed)
    }
}

/// Invia da uno stesso socket a due porte del server e confronta gli indirizzi osservati.
/// Un NAT che li cambia (simmetrico) impedisce l'hole punching. Bloccante.
pub fn probe_nat(sock: &UdpSocket, server: SocketAddr) -> io::Result<NatReport> {
    let observed = observed_addr(sock, server)?;
    let mut other = server;
    other.set_port(server.port().wrapping_add(1));
    Ok(NatReport { observed, second: observed_addr(sock, other).ok() })
}

/// IP locale con cui si raggiungerebbe `server` (utile per peer sulla stessa LAN).
fn local_ip_towards(server: SocketAddr) -> Option<IpAddr> {
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect(server).ok()?;
    probe.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_unspecified())
}

/// Indirizzi a cui il peer che usa `sock` può essere raggiunto: quello osservato dal server e quello locale.
pub fn make_offer(sock: &UdpSocket, server: SocketAddr) -> io::Result<UdpOffer> {
    let observed = observed_addr(sock, server)?;
    let mut addrs = vec![observed];
    if let Some(ip) = local_ip_towards(server) {
        let local = SocketAddr::new(ip, sock.local_addr()?.port());
        if !addrs.contains(&local) {
            addrs.push(local);
        }
    }
    Ok(UdpOffer { addrs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn discovery_reports_observed_address() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        tokio::spawn(serve_discovery(server));

        let client = tokio::task::spawn_blocking(move || {
            let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
            let local = sock.local_addr().unwrap();
            (local, make_offer(&sock, server_addr).unwrap())
        })
        .await
        .unwrap();
        assert_eq!(client.1.addrs[0], client.0);
    }

    #[tokio::test]
    async fn probe_detects_consistent_mapping_with_two_ports() {
        // Cerca due porte consecutive libere per simulare le due porte UDP del server.
        let (first, second) = loop {
            let a = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let port = a.local_addr().unwrap().port();
            if let Ok(b) = tokio::net::UdpSocket::bind(("127.0.0.1", port.wrapping_add(1))).await {
                break (a, b);
            }
        };
        let server = first.local_addr().unwrap();
        tokio::spawn(serve_discovery(first));
        tokio::spawn(serve_discovery(second));

        let report = tokio::task::spawn_blocking(move || {
            let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
            probe_nat(&sock, server).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(report.punchable(), Some(true));
    }

    #[test]
    fn probe_without_second_port_is_inconclusive() {
        let dead_second = UdpSocket::bind("127.0.0.1:0").unwrap();
        let _ = dead_second; // non usato: serve solo a riservare una porta locale
        // Server finto su una sola porta (nessuna risposta sulla seconda).
        let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server = server_sock.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            server_sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            if let Ok((n, peer)) = server_sock.recv_from(&mut buf) {
                let mut reply = RSP.to_vec();
                reply.extend_from_slice(&buf[4..12]);
                reply.extend_from_slice(peer.to_string().as_bytes());
                let _ = (n, server_sock.send_to(&reply, peer));
            }
        });
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let report = probe_nat(&sock, server).unwrap();
        assert_eq!(report.punchable(), None);
        handle.join().unwrap();
    }

    #[test]
    fn unreachable_server_times_out() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        // Porta su cui non ascolta nessuno.
        let dead = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        assert!(observed_addr(&sock, dead).is_err());
    }
}
