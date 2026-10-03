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

/// An idle session still exchanges a packet every few tens of milliseconds in both directions,
/// so neither side's Wi-Fi radio is left idle long enough to doze.
#[tokio::test]
async fn idle_session_keeps_packets_flowing() {
    let host_id = identity("idle-host");
    let client_id = identity("idle-client");
    let host = make_endpoint("127.0.0.1:0".parse().unwrap(), &host_id).unwrap();
    let client = make_endpoint("127.0.0.1:0".parse().unwrap(), &client_id).unwrap();
    let host_addr = host.local_addr().unwrap();
    let accept = tokio::spawn(async move { host.accept().await.unwrap().await.unwrap() });
    let conn = client.connect(host_addr, "lankvm").unwrap().await.unwrap();
    let host_conn = accept.await.unwrap();

    let sent = |c: &quinn::Connection| c.stats().udp_tx.datagrams;
    let (client_before, host_before) = (sent(&conn), sent(&host_conn));
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let (client_sent, host_sent) = (sent(&conn) - client_before, sent(&host_conn) - host_before);
    // 20 ms keep-alive: about 25 PINGs in half a second, plus the answers to the other side's.
    assert!(client_sent >= 10 && host_sent >= 10, "idle for 500 ms: client sent {client_sent}, host sent {host_sent} packets");
    assert!(client_sent + host_sent < 200, "too chatty: {client_sent} + {host_sent} packets in 500 ms");
}

/// How long a big frame (a whole 4K-ish picture, ~280 KB) takes from the first `send_datagram`
/// to reassembled on the other side, over loopback. Ignored: a benchmark.
///
///   cargo test --release -p transport --test loopback -- --ignored --nocapture
#[test]
#[ignore = "benchmark"]
fn big_frame_latency() {
    for interactive in [false, true] {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .on_thread_start(move || {
                if interactive {
                    set_thread_interactive();
                }
            })
            .build()
            .unwrap();
        println!("worker threads {}:", if interactive { "interactive QoS" } else { "default QoS" });
        rt.block_on(big_frames(interactive));
    }
}

/// Like `platform_mac::system::set_thread_interactive` (transport doesn't depend on it).
fn set_thread_interactive() {
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
}

async fn big_frames(interactive: bool) {
    let host_id = identity("bench-host");
    let client_id = identity("bench-client");
    let host = make_endpoint("127.0.0.1:0".parse().unwrap(), &host_id).unwrap();
    let client = make_endpoint("127.0.0.1:0".parse().unwrap(), &client_id).unwrap();
    let host_addr = host.local_addr().unwrap();
    let accept = tokio::spawn(async move { host.accept().await.unwrap().await.unwrap() });
    let conn = client.connect(host_addr, "lankvm").unwrap().await.unwrap();
    let host_conn = accept.await.unwrap();
    for size in [20_000usize, 100_000, 280_000, 600_000] {
        let frame: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let mut packetizer = Packetizer::new();
        let mut r = Reassembler::new();
        let (mut times, mut queue_times, mut first_times) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..40 {
            let max = host_conn.max_datagram_size().unwrap();
            let t0 = std::time::Instant::now();
            let packets = packetizer.packetize(&frame, max).unwrap();
            let sender = host_conn.clone();
            // Sent from a plain thread, like the encoder's callback does.
            let queued = std::thread::spawn(move || {
                if interactive {
                    set_thread_interactive();
                }
                for p in packets {
                    sender.send_datagram(p).unwrap();
                }
                t0.elapsed().as_secs_f64() * 1000.0
            });
            let mut first = None;
            loop {
                let d = conn.read_datagram().await.unwrap();
                first.get_or_insert_with(|| t0.elapsed().as_secs_f64() * 1000.0);
                if r.push(&d).is_some() {
                    break;
                }
            }
            times.push(t0.elapsed().as_secs_f64() * 1000.0);
            queue_times.push(queued.join().unwrap());
            first_times.push(first.unwrap());
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
        for v in [&mut times, &mut queue_times, &mut first_times] {
            v.sort_by(f64::total_cmp);
        }
        println!(
            "{size:>7} bytes in {} datagrams of {}: all in median {:.2} ms, p90 {:.2} ms; queued by sender {:.2} ms; first datagram in {:.2} ms",
            size.div_ceil(host_conn.max_datagram_size().unwrap()),
            host_conn.max_datagram_size().unwrap(),
            times[times.len() / 2],
            times[times.len() * 9 / 10],
            queue_times[queue_times.len() / 2],
            first_times[first_times.len() / 2]
        );
    }
}
