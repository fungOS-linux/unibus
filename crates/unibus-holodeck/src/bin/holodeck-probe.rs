//! Exercise exactly the Quest's discovery + ENet join path without a headset.
use anyhow::{Context, Result, ensure};
use holodeck_discovery::{DISCOVERY_PORT, PROBE, decode_advert};
use rusty_enet::{EventNoRef, Host, HostSettings, Packet};
use std::{
    net::{Ipv4Addr, UdpSocket},
    time::{Duration, Instant},
};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let ip: Ipv4Addr = args
        .next()
        .context("usage: holodeck-probe DISCOVERY_IP ROOM_NAME [CLIENTS] [--broadcast]")?
        .parse()?;
    let room = args.next().context("missing room name")?;
    let clients: usize = args.next().unwrap_or_else(|| "1".into()).parse()?;
    let broadcast = match args.next().as_deref() {
        None => false,
        Some("--broadcast") => true,
        _ => anyhow::bail!("expected --broadcast"),
    };
    ensure!(
        args.next().is_none() && (1..=4).contains(&clients),
        "expected 1..4 clients"
    );
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_broadcast(broadcast)?;
    socket.set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut bytes = [0; 64];
    let discovery_deadline = Instant::now() + Duration::from_secs(12);
    let port = loop {
        ensure!(
            Instant::now() < discovery_deadline,
            "no ready Holodeck host discovered within 12 seconds"
        );
        socket.send_to(
            &PROBE,
            (
                if broadcast { Ipv4Addr::BROADCAST } else { ip },
                DISCOVERY_PORT,
            ),
        )?;
        match socket.recv_from(&mut bytes) {
            Ok((n, source)) => {
                ensure!(
                    source == std::net::SocketAddr::from((ip, DISCOVERY_PORT)),
                    "unexpected discovery source"
                );
                break decode_advert(&bytes[..n]).context("invalid discovery reply")?;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    };
    println!("discovered relay {ip}:{port}");
    let mut hosts = Vec::new();
    for _ in 0..clients {
        let mut host = Host::new(
            UdpSocket::bind("0.0.0.0:0")?,
            HostSettings {
                peer_limit: 1,
                channel_limit: 2,
                ..Default::default()
            },
        )
        .map_err(|e| anyhow::anyhow!("ENet init: {e:?}"))?;
        host.connect(std::net::SocketAddr::from((ip, port)), 2, 0)
            .map_err(|e| anyhow::anyhow!("ENet connect: {e:?}"))?;
        hosts.push(host);
    }
    let mut welcomed = vec![false; clients];
    let started = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && welcomed.iter().any(|v| !v) {
        for (index, host) in hosts.iter_mut().enumerate() {
            while let Some(event) = host.service()?.map(|e| e.no_ref()) {
                match event {
                    EventNoRef::Connect { peer, .. } => {
                        let json = serde_json::to_vec(
                            &serde_json::json!({"v":1,"t":"join","room":room,"name":format!("unibus-probe-{index}")}),
                        )?;
                        host.get_peer_mut(peer)
                            .context("connected peer missing")?
                            .send(0, &Packet::reliable(json.as_slice()))?;
                    }
                    EventNoRef::Receive { packet, .. } => {
                        let value: serde_json::Value = serde_json::from_slice(packet.data())?;
                        if value["t"] == "welcome" && value["room"] == room {
                            println!(
                                "client {index} joined after {} ms: {value}",
                                started.elapsed().as_millis()
                            );
                            welcomed[index] = true;
                        } else if value["t"] == "error" {
                            anyhow::bail!("room rejected client {index}: {value}");
                        }
                    }
                    EventNoRef::Disconnect { .. } => {
                        anyhow::bail!("client {index} disconnected before verification")
                    }
                }
            }
            host.flush();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    ensure!(welcomed.iter().all(|v| *v), "ENet join timed out");
    for host in &mut hosts {
        for peer in host.peers_mut() {
            peer.disconnect(0);
        }
        host.flush();
    }
    let cleanup_deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < cleanup_deadline {
        for host in &mut hosts {
            while host.service()?.is_some() {}
            host.flush();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("verified {clients} independent ENet clients through LAN relay");
    Ok(())
}
