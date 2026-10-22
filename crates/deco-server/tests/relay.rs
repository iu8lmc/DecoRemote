//! Test end-to-end: server reale + host e viewer simulati con il protocollo di deco-proto.

use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};

use deco_proto::{
    msg::{ClientHello, HostMsg, ServerMsg, ViewerMsg},
    net::{recv_plain, send_plain},
    secure::{derive_psk, handshake_initiator, handshake_responder},
};
use tokio::{net::TcpStream, time::sleep};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

async fn start_server() -> (Server, String) {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let addr = format!("127.0.0.1:{port}");
    let child = Command::new(env!("CARGO_BIN_EXE_decoremote-server"))
        .arg(&addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..50 {
        if TcpStream::connect(&addr).await.is_ok() {
            return (Server(child), addr);
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("server non raggiungibile");
}

/// Legge il prossimo `Incoming`, ignorando i Ping periodici.
async fn next_incoming(control: &mut TcpStream) -> u64 {
    loop {
        match recv_plain::<_, ServerMsg>(control).await.unwrap() {
            ServerMsg::Incoming { token } => return token,
            ServerMsg::Ping => continue,
            other => panic!("atteso Incoming, ricevuto {other:?}"),
        }
    }
}

async fn register_host(addr: &str, id: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.unwrap();
    send_plain(&mut s, &ClientHello::Host { id: id.into() }).await.unwrap();
    assert!(matches!(recv_plain::<_, ServerMsg>(&mut s).await.unwrap(), ServerMsg::Ok));
    s
}

#[tokio::test]
async fn full_session_through_relay() {
    let (_server, addr) = start_server().await;
    let mut control = register_host(&addr, "123456789").await;

    // Un secondo host con lo stesso ID viene rifiutato.
    let mut dup = TcpStream::connect(&addr).await.unwrap();
    send_plain(&mut dup, &ClientHello::Host { id: "123456789".into() }).await.unwrap();
    assert!(matches!(recv_plain::<_, ServerMsg>(&mut dup).await.unwrap(), ServerMsg::Error(_)));

    let host_addr = addr.clone();
    let host = tokio::spawn(async move {
        let token = next_incoming(&mut control).await;
        let mut s = TcpStream::connect(&host_addr).await.unwrap();
        send_plain(&mut s, &ClientHello::Accept { token, udp: None }).await.unwrap();
        assert!(matches!(recv_plain::<_, ServerMsg>(&mut s).await.unwrap(), ServerMsg::Paired { .. }));

        let psk = derive_psk("pw-giusta", "123456789").unwrap();
        let (r, w) = s.into_split();
        let (mut r, mut w) = handshake_responder(r, w, &psk).await.unwrap();
        w.send(&HostMsg::Hello { host_name: "pc".into(), screens: vec![], current: 0, codecs: vec![] }).await.unwrap();
        match r.recv::<ViewerMsg>().await.unwrap() {
            ViewerMsg::Text(t) => assert_eq!(t, "ciao"),
            other => panic!("inatteso: {other:?}"),
        }
        w.send(&HostMsg::Frame { width: 1, height: 1, jpeg: vec![1; 200_000] }).await.unwrap();
    });

    let mut s = TcpStream::connect(&addr).await.unwrap();
    send_plain(&mut s, &ClientHello::Connect { target: "123456789".into(), udp: None }).await.unwrap();
    assert!(matches!(recv_plain::<_, ServerMsg>(&mut s).await.unwrap(), ServerMsg::Paired { .. }));
    let psk = derive_psk("pw-giusta", "123456789").unwrap();
    let (r, w) = s.into_split();
    let (mut r, mut w) = handshake_initiator(r, w, &psk).await.unwrap();
    assert!(matches!(r.recv::<HostMsg>().await.unwrap(), HostMsg::Hello { .. }));
    w.send(&ViewerMsg::Text("ciao".into())).await.unwrap();
    match r.recv::<HostMsg>().await.unwrap() {
        HostMsg::Frame { jpeg, .. } => assert_eq!(jpeg.len(), 200_000),
        other => panic!("inatteso: {other:?}"),
    }
    host.await.unwrap();
}

#[tokio::test]
async fn unknown_target_is_rejected() {
    let (_server, addr) = start_server().await;
    let mut s = TcpStream::connect(&addr).await.unwrap();
    send_plain(&mut s, &ClientHello::Connect { target: "000000000".into(), udp: None }).await.unwrap();
    assert!(matches!(recv_plain::<_, ServerMsg>(&mut s).await.unwrap(), ServerMsg::Error(_)));
}

#[tokio::test]
async fn wrong_password_cannot_handshake_through_relay() {
    let (_server, addr) = start_server().await;
    let mut control = register_host(&addr, "555666777").await;

    let host_addr = addr.clone();
    let host = tokio::spawn(async move {
        let token = next_incoming(&mut control).await;
        let mut s = TcpStream::connect(&host_addr).await.unwrap();
        send_plain(&mut s, &ClientHello::Accept { token, udp: None }).await.unwrap();
        let _ = recv_plain::<_, ServerMsg>(&mut s).await.unwrap();
        let psk = derive_psk("giusta", "555666777").unwrap();
        let (r, w) = s.into_split();
        handshake_responder(r, w, &psk).await.is_err()
    });

    let mut s = TcpStream::connect(&addr).await.unwrap();
    send_plain(&mut s, &ClientHello::Connect { target: "555666777".into(), udp: None }).await.unwrap();
    let _ = recv_plain::<_, ServerMsg>(&mut s).await.unwrap();
    let psk = derive_psk("sbagliata", "555666777").unwrap();
    let (r, w) = s.into_split();
    // L'host chiude la connessione dopo aver rifiutato il primo messaggio Noise.
    assert!(handshake_initiator(r, w, &psk).await.is_err());
    assert!(host.await.unwrap(), "l'host deve rifiutare la password errata");
}
