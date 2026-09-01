use super::peer_transport::{rewrite_mdns_candidate_addresses, PeerSignal};
use rand::{distributions::Alphanumeric, Rng};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const DISCOVERY_PORT: u16 = 17424;
const DISCOVERY_MAGIC: &[u8] = b"GOLIVE1";

pub(crate) struct LanRoom {
    pub(crate) code: String,
    pub(crate) port: u16,
    offer: Arc<Mutex<Option<PeerSignal>>>,
    answer: Arc<Mutex<Option<PeerSignal>>>,
    shutdown: Arc<AtomicBool>,
    _workers: Vec<JoinHandle<()>>,
}

impl LanRoom {
    pub(crate) fn start() -> Result<Self, String> {
        let code = random_code();
        let listener = TcpListener::bind("0.0.0.0:0").map_err(|error| error.to_string())?;
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        let port = listener.local_addr().map_err(|error| error.to_string())?.port();
        let offer = Arc::new(Mutex::new(None));
        let answer = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));
        let tcp_offer = Arc::clone(&offer);
        let tcp_answer = Arc::clone(&answer);
        let tcp_shutdown = Arc::clone(&shutdown);
        let tcp = thread::Builder::new()
            .name("godrinking-room-tcp".into())
            .spawn(move || tcp_loop(listener, tcp_offer, tcp_answer, tcp_shutdown))
            .map_err(|error| error.to_string())?;
        let udp_code = code.clone();
        let udp_shutdown = Arc::clone(&shutdown);
        let udp = thread::Builder::new()
            .name("godrinking-room-udp".into())
            .spawn(move || udp_loop(udp_code, port, udp_shutdown))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            code,
            port,
            offer,
            answer,
            shutdown,
            _workers: vec![tcp, udp],
        })
    }

    pub(crate) fn publish_offer(&self, signal: PeerSignal) {
        if let Ok(mut offer) = self.offer.lock() {
            *offer = Some(signal);
        }
    }

    pub(crate) fn take_answer(&self) -> Option<PeerSignal> {
        self.answer.lock().ok().and_then(|mut answer| answer.take())
    }

    pub(crate) fn addresses() -> Vec<String> {
        local_addresses()
    }
}

impl Drop for LanRoom {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

fn random_code() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .filter(|byte| byte.is_ascii_alphanumeric() && !byte.is_ascii_lowercase())
        .take(6)
        .map(char::from)
        .collect()
}

fn ice_ip_for_peer(peer: Option<IpAddr>) -> String {
    let Some(ip) = peer else {
        return "127.0.0.1".into();
    };
    let rendered = match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(|v4| v4.to_string())
            .unwrap_or_else(|| v6.to_string()),
    };
    if ip.is_loopback() || local_addresses().iter().any(|local| local == &rendered) {
        return "127.0.0.1".into();
    }
    rendered
}

fn local_addresses() -> Vec<String> {
    let mut addresses = Vec::new();
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        let _ = socket.connect("1.1.1.1:80");
        if let Ok(addr) = socket.local_addr() {
            if !addr.ip().is_loopback() {
                addresses.push(addr.ip().to_string());
            }
        }
    }
    if addresses.is_empty() {
        addresses.push("127.0.0.1".into());
    }
    addresses
}

fn tcp_loop(
    listener: TcpListener,
    offer: Arc<Mutex<Option<PeerSignal>>>,
    answer: Arc<Mutex<Option<PeerSignal>>>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = handle_tcp(stream, &offer, &answer);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn handle_tcp(
    mut stream: TcpStream,
    offer: &Arc<Mutex<Option<PeerSignal>>>,
    answer: &Arc<Mutex<Option<PeerSignal>>>,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(4)))
        .map_err(|error| error.to_string())?;
    let mut buffer = String::new();
    stream
        .read_to_string(&mut buffer)
        .map_err(|error| error.to_string())?;
    let line = buffer.lines().next().unwrap_or("").trim();
    if line == "GET_OFFER" {
        let payload = offer
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .ok_or_else(|| "offer is not ready".to_owned())?;
        let body = serde_json::to_string(&payload).map_err(|error| error.to_string())?;
        stream
            .write_all(format!("OFFER {body}\n").as_bytes())
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    if let Some(json) = line.strip_prefix("ANSWER ") {
        let mut signal: PeerSignal =
            serde_json::from_str(json).map_err(|error| error.to_string())?;
        let peer_ip = stream.peer_addr().ok().map(|addr| addr.ip());
        signal.sdp = rewrite_mdns_candidate_addresses(&signal.sdp, &ice_ip_for_peer(peer_ip));
        if let Ok(mut answer) = answer.lock() {
            *answer = Some(signal);
        }
        stream
            .write_all(b"OK\n")
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    Err("unsupported room request".into())
}

fn udp_loop(code: String, tcp_port: u16, shutdown: Arc<AtomicBool>) {
    let Ok(socket) = UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)) else {
        return;
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));
    let mut buffer = [0_u8; 256];
    while !shutdown.load(Ordering::Acquire) {
        let Ok((size, from)) = socket.recv_from(&mut buffer) else {
            continue;
        };
        let request = String::from_utf8_lossy(&buffer[..size]);
        let mut parts = request.split_whitespace();
        if parts.next() != Some(std::str::from_utf8(DISCOVERY_MAGIC).unwrap_or("GOLIVE1")) {
            continue;
        }
        if parts.next() != Some("FIND") {
            continue;
        }
        if parts.next() != Some(code.as_str()) {
            continue;
        }
        let reply = format!(
            "GOLIVE1 HOST {code} {tcp_port}"
        );
        let _ = socket.send_to(reply.as_bytes(), from);
    }
}

pub fn discover_room(code: &str) -> Result<(SocketAddr, PeerSignal), String> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|error| error.to_string())?;
    socket
        .set_broadcast(true)
        .map_err(|error| error.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| error.to_string())?;
    let query = format!("GOLIVE1 FIND {code}");
    socket
        .send_to(query.as_bytes(), ("255.255.255.255", DISCOVERY_PORT))
        .map_err(|error| error.to_string())?;
    let mut buffer = [0_u8; 256];
    let (size, from) = socket
        .recv_from(&mut buffer)
        .map_err(|_| "room was not found on this network".to_owned())?;
    let reply = String::from_utf8_lossy(&buffer[..size]);
    let mut parts = reply.split_whitespace();
    if parts.next() != Some("GOLIVE1") || parts.next() != Some("HOST") {
        return Err("invalid room discovery reply".into());
    }
    if parts.next() != Some(code) {
        return Err("room code mismatch".into());
    }
    let port = parts
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| "room reply is missing a port".to_owned())?;
    let host = SocketAddr::new(from.ip(), port);
    let offer = fetch_offer(host)?;
    Ok((host, offer))
}

pub(crate) fn fetch_offer(host: SocketAddr) -> Result<PeerSignal, String> {
    let mut stream = TcpStream::connect_timeout(&host, Duration::from_secs(3))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(b"GET_OFFER\n")
        .map_err(|error| error.to_string())?;
    stream.shutdown(std::net::Shutdown::Write).ok();
    let mut buffer = String::new();
    stream
        .read_to_string(&mut buffer)
        .map_err(|error| error.to_string())?;
    let line = buffer.lines().next().unwrap_or("").trim();
    let json = line
        .strip_prefix("OFFER ")
        .ok_or_else(|| "host did not return an offer".to_owned())?;
    serde_json::from_str(json).map_err(|error| error.to_string())
}

pub fn submit_answer(host: SocketAddr, answer: &PeerSignal) -> Result<(), String> {
    let mut stream = TcpStream::connect_timeout(&host, Duration::from_secs(3))
        .map_err(|error| error.to_string())?;
    let body = serde_json::to_string(answer).map_err(|error| error.to_string())?;
    stream
        .write_all(format!("ANSWER {body}\n").as_bytes())
        .map_err(|error| error.to_string())?;
    stream.shutdown(std::net::Shutdown::Write).ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::random_code;

    #[test]
    fn room_codes_are_short_and_uppercase() {
        let code = random_code();
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|ch| ch.is_ascii_alphanumeric()));
        assert_eq!(code, code.to_ascii_uppercase());
    }
}
