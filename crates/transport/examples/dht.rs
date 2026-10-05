//! Checks how this Mac fares on the BitTorrent DHT, from the terminal:
//!
//! ```text
//! cargo run --release -p transport --example dht -- check [--port N]
//! ```
//!
//! Joins the DHT from a UDP socket (port N, or any), says where the nodes see this Mac and whether
//! its router keeps one public port for every destination (what punching through it needs), then
//! stores a small random item and has a second client (as the other Mac would) find it, timing
//! each step. Nothing in the item means anything; it is gone from the DHT within hours.
//! `RUST_LOG=transport=trace` shows each lookup.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use ring::signature::{Ed25519KeyPair, KeyPair};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use transport::dht::{Config, DEFAULT_BOOTSTRAP, Dht, MutableItem};
use transport::knock::random_bytes;

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (mut command, mut port) = (None, 0u16);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => match args.next().and_then(|p| p.parse().ok()) {
                Some(p) => port = p,
                None => return usage("--port needs a number"),
            },
            "check" if command.is_none() => command = Some(arg),
            other => return usage(&format!("unknown argument {other}")),
        }
    }
    if command.is_none() {
        return usage("say what to do");
    }
    if std::env::var_os("RUST_LOG").is_some() {
        tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_writer(std::io::stderr).init();
    }
    match check(port).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            println!("FAIL: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage(problem: &str) -> ExitCode {
    eprintln!("{problem}\nusage: dht check [--port N]");
    ExitCode::from(2)
}

/// A DHT client on a UDP socket of its own at `port` (0: any).
async fn client(port: u16, seeds: Vec<SocketAddr>) -> Result<(Dht, SocketAddr), String> {
    let socket = Arc::new(UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], port))).await.map_err(|e| format!("bind: {e}"))?);
    let local = socket.local_addr().map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel(512);
    let reader = socket.clone();
    tokio::spawn(async move {
        let mut buf = vec![0; 2048];
        while let Ok((len, from)) = reader.recv_from(&mut buf).await {
            let _ = tx.try_send((from, buf[..len].to_vec()));
        }
    });
    let config = Config { bootstrap: DEFAULT_BOOTSTRAP.iter().map(|s| s.to_string()).collect(), seeds, allow_local: false };
    Ok((Dht::start(tokio::runtime::Handle::current(), socket, rx, config), local))
}

async fn check(port: u16) -> Result<(), String> {
    let (dht, local) = client(port, Vec::new()).await?;
    println!("socket: {local}");

    let started = Instant::now();
    let alive = dht.bootstrap().await;
    println!("joined: {alive} nodes answered in {} ms", started.elapsed().as_millis());
    if alive == 0 {
        return Err("no DHT node answered: is UDP blocked here?".into());
    }

    let observed = dht.observed();
    match observed.addr {
        Some(addr) => println!(
            "public address: {addr} ({} of {} nodes agree){}",
            observed.agreeing,
            observed.reporters,
            if addr.port() == local.port() { ", the router kept the port" } else { "" }
        ),
        None => println!("public address: unknown ({} nodes said)", observed.reporters),
    }
    if observed.varies {
        println!("router: gives each destination a port of its own (\"symmetric NAT\"): punching through it fails");
    } else if observed.agreeing >= 2 {
        println!("router: one public port for every destination: punching works through it");
    }

    let keypair = Ed25519KeyPair::from_seed_unchecked(&random_bytes::<32>()).expect("seed");
    let key: [u8; 32] = keypair.public_key().as_ref().try_into().expect("key");
    let started = Instant::now();
    let (_, storing) = dht.get_mutable(&key, &[]).await;
    println!("lookup: {} nodes close to a random item keep items, found in {} ms", storing.len(), started.elapsed().as_millis());

    let item = MutableItem::sign(&keypair, &[], 1, random_bytes::<32>().to_vec()).expect("item");
    let started = Instant::now();
    let stored = dht.put_mutable(&item, &storing).await;
    println!("put: {stored} nodes stored it in {} ms", started.elapsed().as_millis());
    if stored == 0 {
        return Err("no node stored the item".into());
    }
    let nodes: Vec<SocketAddr> = storing.iter().map(|f| f.node.addr).collect();
    let started = Instant::now();
    let polled = dht.poll_mutable(&nodes, &key, &[], None).await;
    println!("read back from those nodes: {} in {} ms", if polled.as_ref() == Some(&item) { "yes" } else { "NO" }, started.elapsed().as_millis());

    // A second client, as the other Mac would be, with a lookup of its own.
    let (other, _) = client(0, dht.alive_nodes(32)).await?;
    let started = Instant::now();
    let (found, _) = other.find_mutable(&key, &[], &|_| true).await;
    println!("another client's lookup: {} in {} ms", if found.as_ref() == Some(&item) { "found it" } else { "NOT found" }, started.elapsed().as_millis());
    let stats = dht.stats();
    println!("queries sent: {} ({} answered, {} timed out)", stats.queries, stats.responses, stats.timeouts);
    if found.as_ref() != Some(&item) {
        return Err("another client couldn't find the item".into());
    }
    println!("PASS: this Mac can meet others through the DHT");
    Ok(())
}
