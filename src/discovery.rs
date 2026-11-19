//! LocalSend discovery.
//!
//! Devices announce themselves over UDP multicast (224.0.0.167:53317).
//! Announcements that request a response are answered with `POST /register`
//! over HTTP(S), with a UDP unicast announcement as a fallback. A legacy
//! HTTP sweep is available for networks where multicast is blocked.

use crate::core::{Core, Event};
use crate::net;
use crate::protocol::{api_url, DeviceInfo, MULTICAST_IP, MULTICAST_PORT};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const DEVICE_TTL: Duration = Duration::from_secs(20);
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(5);
const PRUNE_INTERVAL: Duration = Duration::from_secs(2);
// Quitting the mobile app stops its HTTP server immediately. Probe often
// enough that two failed probes remove it within five seconds.
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// How often the local addresses are compared, to notice a network change.
const INTERFACE_CHECK: Duration = Duration::from_secs(2);
/// A wall-clock jump this much larger than the monotonic clock is a resume.
const SLEEP_TOLERANCE: Duration = Duration::from_secs(3);
/// Delay before retrying to reopen the sockets after a failure.
const REOPEN_RETRY: Duration = Duration::from_secs(2);

pub fn spawn(core: Arc<Core>) {
    let discovery_core = core.clone();
    let _ = thread::Builder::new()
        .name("localsend-discovery".to_string())
        .spawn(move || run(discovery_core));
    let _ = thread::Builder::new()
        .name("localsend-probe".to_string())
        .spawn(move || probe_loop(core));
}

/// Checks that each visible peer still answers its `/info` endpoint. This
/// removes a peer whose discovery service survives after its app/server is
/// gone, without waiting for it to stop announcing.
fn probe_loop(core: Arc<Core>) {
    loop {
        thread::sleep(PROBE_INTERVAL);
        for (key, info, ip) in core.devices_for_probe() {
            if probe_peer(&core, &key, &info, &ip).is_err() {
                core.mark_device_probe_failed(&key);
            }
        }
    }
}

fn probe_peer(core: &Arc<Core>, key: &str, peer: &DeviceInfo, ip: &str) -> Result<(), String> {
    let protocol = peer.protocol();
    let agent = net::client(
        protocol,
        &peer.fingerprint,
        core.identity().as_deref(),
        Some(PROBE_TIMEOUT),
    )?;
    let url = api_url(ip, peer.port_or_default(), protocol, "/info");
    let response = agent
        .get(&url)
        .timeout(PROBE_TIMEOUT)
        .call()
        .map_err(|error| error.to_string())?;
    let mut info: DeviceInfo = response.into_json().map_err(|error| error.to_string())?;
    if info.fingerprint.is_empty() {
        core.reset_device_probe_failures(key);
    } else {
        // `/info` is allowed to omit connection details. Keep the values the
        // peer announced so probing never downgrades HTTPS to HTTP.
        if info.port.is_none() {
            info.port = Some(peer.port_or_default());
        }
        if info.protocol.is_none() {
            info.protocol = Some(peer.protocol().to_string());
        }
        core.upsert_device_verified(info, ip.to_string());
    }
    Ok(())
}

/// The sockets discovery listens and announces on. Both are reopened when the
/// machine wakes up or the local addresses change: the multicast memberships
/// and the route picked for the group do not survive a suspend, and a stale
/// membership means no announcement is ever received, so nobody can find this
/// device until the app is restarted.
struct Sockets {
    listener: Option<UdpSocket>,
    sender: Option<UdpSocket>,
}

/// Detects time spent suspended. `Instant` is CLOCK_MONOTONIC on Linux and
/// stops while the machine sleeps, so wall-clock time moving further than it
/// did since the previous call means we just woke up.
struct WakeWatch {
    wall: SystemTime,
    steady: Instant,
}

impl WakeWatch {
    fn new() -> Self {
        Self {
            wall: SystemTime::now(),
            steady: Instant::now(),
        }
    }

    fn woke(&mut self) -> bool {
        let wall_now = SystemTime::now();
        let steady_now = Instant::now();
        let wall = wall_now.duration_since(self.wall).unwrap_or_default();
        let steady = steady_now.duration_since(self.steady);
        self.wall = wall_now;
        self.steady = steady_now;
        slept(wall, steady)
    }
}

fn slept(wall: Duration, steady: Duration) -> bool {
    wall > steady.saturating_add(SLEEP_TOLERANCE)
}

/// Builds a discovery socket pair, replacing whatever was there before. The
/// listener is dropped first, so a membership from the previous network is
/// never carried over.
fn reopen(core: &Core, sockets: &mut Sockets) -> bool {
    sockets.listener = None;
    let listener = match listen_socket() {
        Ok(listener) => listener,
        Err(error) => {
            core.emit(Event::ServerError(format!(
                "Device discovery is unavailable: {error}"
            )));
            return false;
        }
    };
    let sender = match send_socket() {
        Ok(sender) => sender,
        Err(error) => {
            core.emit(Event::ServerError(format!(
                "Could not open the announcement socket: {error}"
            )));
            return false;
        }
    };
    sockets.listener = Some(listener);
    sockets.sender = Some(sender);
    true
}

fn local_addresses() -> Vec<Ipv4Addr> {
    let mut addresses: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .map(|interfaces| {
            interfaces
                .into_iter()
                .filter(|interface| !interface.is_loopback())
                .filter_map(|interface| match interface.ip() {
                    IpAddr::V4(address) => Some(address),
                    IpAddr::V6(_) => None,
                })
                .collect()
        })
        .unwrap_or_default();
    addresses.sort();
    addresses
}

fn run(core: Arc<Core>) {
    let mut sockets = Sockets {
        listener: None,
        sender: None,
    };
    if !reopen(&core, &mut sockets) {
        return;
    }

    let mut wake = WakeWatch::new();
    let mut addresses = local_addresses();
    let mut last_address_check = Instant::now();
    let mut retry_at: Option<Instant> = None;
    let mut announce_now = true;
    let mut last_announce = Instant::now();
    let mut last_prune = Instant::now();
    let mut buffer = vec![0u8; 64 * 1024];

    loop {
        let mut rebuild = wake.woke();
        if last_address_check.elapsed() >= INTERFACE_CHECK {
            last_address_check = Instant::now();
            let current = local_addresses();
            if current != addresses {
                addresses = current;
                rebuild = true;
            }
        }
        if let Some(at) = retry_at {
            if Instant::now() >= at {
                rebuild = true;
                retry_at = None;
            }
        }
        if rebuild {
            if reopen(&core, &mut sockets) {
                // Peers only learn that this device is back when it
                // announces, so do not wait for the interval to elapse.
                announce_now = true;
            } else {
                retry_at = Some(Instant::now() + REOPEN_RETRY);
            }
        }

        if announce_now || last_announce.elapsed() >= ANNOUNCE_INTERVAL {
            if let Some(sender) = sockets.sender.as_ref() {
                announce(&core, sender);
                announce_now = false;
                last_announce = Instant::now();
            }
        }
        if last_prune.elapsed() >= PRUNE_INTERVAL {
            core.remove_stale_devices(DEVICE_TTL);
            last_prune = Instant::now();
        }

        let Some(listener) = sockets.listener.as_ref() else {
            thread::sleep(Duration::from_millis(200));
            continue;
        };
        match listener.recv_from(&mut buffer) {
            Ok((length, source)) => {
                handle_datagram(&core, &buffer[..length], source);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // Non-blocking receive: keep the announce/prune clock moving
                // even when the network is quiet.
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                eprintln!("Discovery socket error: {error}");
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

fn listen_socket() -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    let _ = socket.set_reuse_port(true);
    socket.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), MULTICAST_PORT).into())?;
    socket.set_nonblocking(true)?;

    let group = Ipv4Addr::from(MULTICAST_IP);
    let mut joined = false;
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            if interface.is_loopback() {
                continue;
            }
            if let IpAddr::V4(address) = interface.ip() {
                if socket.join_multicast_v4(&group, &address).is_ok() {
                    joined = true;
                }
            }
        }
    }
    if !joined {
        let _ = socket.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED);
    }
    Ok(socket.into())
}

fn send_socket() -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_multicast_loop_v4(true)?;
    socket.set_multicast_ttl_v4(2)?;
    socket.set_broadcast(true)?;
    socket.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).into())?;
    Ok(socket.into())
}

fn announce(core: &Core, sender: &UdpSocket) {
    let info = core.info(Some(true));
    let Ok(payload) = serde_json::to_vec(&info) else {
        return;
    };
    let multicast = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(MULTICAST_IP)), MULTICAST_PORT);
    let broadcast = SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), MULTICAST_PORT);
    let _ = sender.send_to(&payload, multicast);
    let _ = sender.send_to(&payload, broadcast);
}

fn handle_datagram(core: &Arc<Core>, data: &[u8], source: SocketAddr) {
    let Ok(info) = serde_json::from_slice::<DeviceInfo>(data) else {
        return;
    };
    if core.is_own_fingerprint(&info.fingerprint) {
        return;
    }
    let ip = source.ip().to_string();
    if info.announce == Some(false) {
        core.upsert_device(info, ip);
        return;
    }

    core.upsert_device(info.clone(), ip.clone());

    // Answer over HTTP(S) so the peer learns about us; fall back to a UDP
    // unicast announcement if that fails.
    let core_for_register = core.clone();
    let info_for_register = info.clone();
    let ip_for_register = ip.clone();
    thread::spawn(move || {
        if register_peer(&core_for_register, &info_for_register, &ip_for_register).is_err() {
            let reply = core_for_register.info(Some(false));
            if let Ok(payload) = serde_json::to_vec(&reply) {
                if let Ok(socket) = send_socket() {
                    let target = SocketAddr::new(
                        ip_for_register
                            .parse()
                            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                        MULTICAST_PORT,
                    );
                    let _ = socket.send_to(&payload, target);
                }
            }
        }
    });
}

/// Sends our device information to a peer and records the response.
pub fn register_peer(core: &Arc<Core>, peer: &DeviceInfo, ip: &str) -> Result<(), String> {
    let protocol = peer.protocol();
    let agent = net::client(
        protocol,
        &peer.fingerprint,
        core.identity().as_deref(),
        Some(Duration::from_secs(6)),
    )?;
    let local = core.info(None);
    let mut response: DeviceInfo = net::post_json_parse(
        &agent,
        ip,
        peer.port_or_default(),
        protocol,
        "/register",
        &local,
        Some(Duration::from_secs(6)),
    )
    .map_err(|error| error.to_string())?;
    if response.port.is_none() {
        response.port = Some(peer.port_or_default());
    }
    if response.protocol.is_none() {
        response.protocol = Some(peer.protocol().to_string());
    }
    if !response.fingerprint.is_empty() {
        core.upsert_device_verified(response, ip.to_string());
    }
    Ok(())
}

/// Legacy discovery: probe every host on the local /24 over plain HTTP.
pub fn scan(core: Arc<Core>) {
    if core.scanning.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(move || {
        let port = core.port();
        let mut targets: Vec<String> = Vec::new();
        if let Ok(interfaces) = if_addrs::get_if_addrs() {
            for interface in interfaces {
                if interface.is_loopback() {
                    continue;
                }
                if let IpAddr::V4(address) = interface.ip() {
                    let octets = address.octets();
                    for host in 1u8..=254 {
                        if host != octets[3] {
                            targets.push(format!(
                                "{}.{}.{}.{}",
                                octets[0], octets[1], octets[2], host
                            ));
                        }
                    }
                }
            }
        }
        targets.sort();
        targets.dedup();

        let chunk_size = (targets.len() / 32).max(1);
        thread::scope(|scope| {
            for chunk in targets.chunks(chunk_size) {
                let core = core.clone();
                scope.spawn(move || {
                    let Ok(agent) = net::client("http", "", None, Some(Duration::from_millis(700)))
                    else {
                        return;
                    };
                    let body = core.info(None);
                    for ip in chunk {
                        let url = api_url(ip, port, "http", "/register");
                        let Ok(payload) = serde_json::to_string(&body) else {
                            continue;
                        };
                        if let Ok(response) = agent
                            .post(&url)
                            .timeout(Duration::from_millis(700))
                            .set("Content-Type", "application/json")
                            .send_string(&payload)
                        {
                            if let Ok(info) = response.into_json::<DeviceInfo>() {
                                core.upsert_device_verified(info, ip.clone());
                            }
                        }
                    }
                });
            }
        });

        core.scanning.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_tick_is_not_a_resume() {
        assert!(!slept(Duration::from_secs(100), Duration::from_secs(100)));
    }

    #[test]
    fn clock_drift_smaller_than_the_tolerance_is_ignored() {
        assert!(!slept(Duration::from_secs(102), Duration::from_secs(100)));
    }

    #[test]
    fn a_wall_clock_jump_while_the_monotonic_clock_stalled_is_a_resume() {
        // Ten minutes of wall-clock time, a tenth of a second of uptime.
        assert!(slept(Duration::from_secs(600), Duration::from_millis(100)));
    }

    #[test]
    fn the_monotonic_clock_running_ahead_is_not_a_resume() {
        assert!(!slept(Duration::from_secs(10), Duration::from_secs(600)));
    }
}
