//! Two endpoints on loopback: handshake with mutual certificates, control messages, and a
//! packetized frame over datagrams.

use protocol::{ClientMsg, HostMsg};
use transport::endpoint::{make_endpoint, peer_fingerprint};
use transport::framing::{read_msg, write_msg};
use transport::identity::DeviceIdentity;
use transport::video::{Packetizer, Reassembler};

fn identity(name: &str) -> DeviceIdentity {
    let dir = std::env::temp_dir().join(format!("lankvm-test-{name}-{}", std::process::id()));
    DeviceIdentity::load_or_create(&dir).unwrap()
}

#[tokio::test]
async fn session_over_loopback() {
    let host_id = identity("host");
    let client_id = identity("client");
    let host = make_endpoint("127.0.0.1:0".parse().unwrap(), &host_id).unwrap();
    let client = make_endpoint("127.0.0.1:0".parse().unwrap(), &client_id).unwrap();
    let host_addr = host.local_addr().unwrap();
    let frame: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();

    let host_task = {
        let frame = frame.clone();
        let client_fp = client_id.fingerprint;
        tokio::spawn(async move {
            let conn = host.accept().await.unwrap().await.unwrap();
            assert_eq!(peer_fingerprint(&conn), Some(client_fp));
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            let hello: ClientMsg = read_msg(&mut recv).await.unwrap().unwrap();
            assert!(matches!(hello, ClientMsg::Hello { .. }));
            write_msg(&mut send, &HostMsg::Pong { client_time_us: 1, host_time_us: 2 }).await.unwrap();
            let max = conn.max_datagram_size().unwrap();
            for d in Packetizer::new().packetize(&frame, max).unwrap() {
                conn.send_datagram(d).unwrap();
            }
            // Keep the connection open until the client has read everything.
            let _ = read_msg::<ClientMsg>(&mut recv).await;
        })
    };

    let conn = client.connect(host_addr, "lankvm").unwrap().await.unwrap();
    assert_eq!(peer_fingerprint(&conn), Some(host_id.fingerprint));
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello = ClientMsg::Hello { version: 1, device_name: "test".into(), max_width: 1, max_height: 1, fps: 60, trusts_host: false };
    write_msg(&mut send, &hello).await.unwrap();
    let reply: HostMsg = read_msg(&mut recv).await.unwrap().unwrap();
    assert_eq!(reply, HostMsg::Pong { client_time_us: 1, host_time_us: 2 });

    let mut r = Reassembler::new();
    let assembled = loop {
        let d = conn.read_datagram().await.unwrap();
        if let Some(a) = r.push(&d) {
            break a;
        }
    };
    assert_eq!(assembled.data, frame);
    send.finish().unwrap();
    host_task.await.unwrap();
}
