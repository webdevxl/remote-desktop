//! Port mapping: asks the router to forward a UDP port to this Mac, so a host can be reached from
//! the internet. This is mDNSResponder's `DNSServiceNATPortMappingCreate` (dns_sd, in libSystem),
//! which speaks PCP, NAT-PMP and UPnP IGD on our behalf:
//! - It renews the mapping for as long as the request lives, maps again after a network change or
//!   wake, and calls back whenever the external address or port changes.
//! - Deallocating the request deletes the mapping on the router. The daemon also notices our socket
//!   close, so quitting or crashing removes it too.
//! - If this Mac already has a public address, the callback reports that address and our own port.
//! - If the router doesn't answer, the callback arrives after about 4 s with address 0.0.0.0, and
//!   the daemon keeps trying in the background.
//! - IPv4 and the primary interface only. The daemon sends the packets, so Local Network privacy
//!   doesn't apply.

use std::ffi::{c_int, c_void};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long a request may go without an answer that names an address before it counts as
/// unanswered. mDNSResponder reports a silent router after about 4 s.
const NO_RESPONSE_AFTER: Duration = Duration::from_secs(6);
/// The mapping thread wakes at least this often to check for stop and the timer.
const POLL_MS: c_int = 200;
/// Wait before asking mDNSResponder again after losing it, doubling up to the maximum.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

// From dns_sd.h.
const PROTOCOL_UDP: u32 = 0x10;
const ERR_UNKNOWN: i32 = -65537;
const ERR_BAD_REFERENCE: i32 = -65541;
const ERR_FIREWALL: i32 = -65550;
const ERR_DOUBLE_NAT: i32 = -65558;
const ERR_SERVICE_NOT_RUNNING: i32 = -65563;
const ERR_NAT_PORT_MAPPING_UNSUPPORTED: i32 = -65564;
const ERR_NAT_PORT_MAPPING_DISABLED: i32 = -65565;
const ERR_NO_ROUTER: i32 = -65566;
const ERR_TIMEOUT: i32 = -65568;
const ERR_DEFUNCT_CONNECTION: i32 = -65569;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapState {
    /// Waiting for the router's answer.
    Requesting,
    /// The router forwards `external` to this Mac. Its port may differ from the one requested.
    Mapped { external: SocketAddrV4 },
    /// This Mac's own address is public: there is nothing to map.
    PublicAddress { external: SocketAddrV4 },
    /// mDNSResponder keeps retrying in the background, so this can still turn into `Mapped`.
    Failed(MapProblem),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapProblem {
    /// The router answered none of PCP, NAT-PMP or UPnP.
    NoResponse,
    /// The router can't open ports automatically.
    Unsupported,
    /// The router could, but automatic port forwarding is turned off.
    Disabled,
    /// The router is behind another NAT. `router_external` is the router's own (private) outside
    /// address, when known.
    DoubleNat { router_external: Option<Ipv4Addr> },
    /// The router's outside address is shared by the internet provider (carrier-grade NAT), so a
    /// mapping on it can't be reached from the internet.
    Cgnat { router_external: Ipv4Addr },
    /// No network.
    NoRouter,
    /// mDNSResponder isn't answering. The request is made again after a backoff.
    ServiceDown,
    /// macOS Firewall blocks it.
    Firewall,
    /// Any other dns_sd error code.
    Other(i32),
}

/// Keeps a UDP port forwarded on the router until stopped or dropped, which deletes the mapping.
pub struct PortMapping {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<MapState>>,
    thread: Option<JoinHandle<()>>,
}

impl PortMapping {
    /// Asks the router to forward UDP `internal_port` (preferring the same external port). `on_change` runs on
    /// the mapping's thread whenever the state changes. It starts out `Requesting`.
    ///
    /// Stopping the mapping waits for `on_change` to return, so `on_change` must not wait for anything
    /// held by the code that stops or drops it (such as a lock around the `PortMapping`): that deadlocks.
    pub fn start(internal_port: u16, preferred_external: u16, on_change: impl Fn(MapState) + Send + 'static) -> Self {
        Self::start_request(PROTOCOL_UDP, internal_port, preferred_external, on_change)
    }

    fn start_request(protocol: u32, internal_port: u16, external_port: u16, on_change: impl Fn(MapState) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(MapState::Requesting));
        let spawned = std::thread::Builder::new().name("lankvm-portmap".into()).spawn({
            let (stop, state) = (stop.clone(), state.clone());
            move || {
                run(protocol, internal_port, external_port, &stop, &|new: MapState| {
                    tracing::info!(state = ?new, "port mapping");
                    *state.lock().unwrap() = new;
                    on_change(new);
                })
            }
        });
        let thread = match spawned {
            Ok(thread) => Some(thread),
            Err(e) => {
                tracing::warn!("couldn't start the port mapping thread: {e}");
                *state.lock().unwrap() = MapState::Failed(MapProblem::Other(ERR_UNKNOWN));
                None
            }
        };
        Self { stop, state, thread }
    }

    pub fn state(&self) -> MapState {
        *self.state.lock().unwrap()
    }

    /// Stops renewing and deletes the mapping (mDNSResponder removes it). Also on Drop.
    ///
    /// Blocks until the mapping's thread has ended, so a later mapping of the same port can't be
    /// deleted by this one's cleanup (UPnP deletes by port). That normally takes under 200 ms plus
    /// whatever `on_change` is doing, but while mDNSResponder hangs it can take about 10 s: dns_sd
    /// waits that long for the daemon to take a new request.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for PortMapping {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // Dropped from `on_change`, on the mapping's own thread: its loop ends once that returns.
            if thread.thread().id() != std::thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}

/// What a dns_sd error code and the address and port reported with it mean. `local_ips` are this
/// Mac's own IPv4 addresses: the daemon reports one of them when the Mac needs no mapping.
pub fn classify(err: i32, external_ip: Ipv4Addr, external_port: u16, local_ips: &[Ipv4Addr]) -> MapState {
    let problem = match err {
        0 if external_ip.is_unspecified() => return MapState::Requesting,
        // The daemon only flags RFC 1918 outside addresses as double NAT; it accepts shared ones, and
        // a Mac that gets one directly (say, from a cellular modem) looks public to it.
        0 if is_shared(external_ip) => MapProblem::Cgnat { router_external: external_ip },
        0 if external_ip.is_private() => MapProblem::DoubleNat { router_external: Some(external_ip) },
        // An address without a port: the router told us its address but hasn't opened the port yet.
        0 if external_port == 0 => return MapState::Requesting,
        0 => {
            let external = SocketAddrV4::new(external_ip, external_port);
            return if local_ips.contains(&external_ip) {
                MapState::PublicAddress { external }
            } else {
                MapState::Mapped { external }
            };
        }
        // The other fields are still valid with this error (dns_sd.h), but may be zero.
        ERR_DOUBLE_NAT => MapProblem::DoubleNat { router_external: (!external_ip.is_unspecified()).then_some(external_ip) },
        ERR_NAT_PORT_MAPPING_UNSUPPORTED => MapProblem::Unsupported,
        ERR_NAT_PORT_MAPPING_DISABLED => MapProblem::Disabled,
        ERR_NO_ROUTER => MapProblem::NoRouter,
        ERR_SERVICE_NOT_RUNNING | ERR_TIMEOUT | ERR_DEFUNCT_CONNECTION => MapProblem::ServiceDown,
        ERR_FIREWALL => MapProblem::Firewall,
        other => MapProblem::Other(other),
    };
    MapState::Failed(problem)
}

/// Addresses an internet provider shares among customers: carrier-grade NAT (100.64.0.0/10,
/// RFC 6598) and DS-Lite's IPv4-in-IPv6 tunnel (192.0.0.0/29, RFC 6333).
fn is_shared(ip: Ipv4Addr) -> bool {
    let [a, b, c, d] = ip.octets();
    (a == 100 && b & 0xc0 == 64) || (a == 192 && b == 0 && c == 0 && d < 8)
}

fn is_service_down(err: i32) -> bool {
    matches!(err, ERR_SERVICE_NOT_RUNNING | ERR_TIMEOUT | ERR_DEFUNCT_CONNECTION)
}

/// The mapping thread: one request at a time, made again with backoff whenever the connection to
/// mDNSResponder breaks, until `stop`. Ends by dropping the request, which deletes the mapping.
fn run(protocol: u32, internal_port: u16, external_port: u16, stop: &AtomicBool, publish: &dyn Fn(MapState)) {
    let mut tracker = Tracker::new();
    let mut backoff = BACKOFF_MIN;
    let changed = |change: Option<MapState>| {
        if let Some(state) = change {
            publish(state);
        }
    };
    while !stop.load(Ordering::Relaxed) {
        // Why the request couldn't be made or stopped working: a dns_sd error code.
        let err = match Request::new(protocol, internal_port, external_port) {
            Err(err) => err,
            Ok(mut request) => {
                changed(tracker.restart(Instant::now()));
                let broken = loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let replies = match request.wait(POLL_MS) {
                        Ok(replies) => replies,
                        Err(err) => break err,
                    };
                    let mut down = None;
                    for reply in replies {
                        tracing::debug!(
                            err = reply.error,
                            interface = reply.interface,
                            external = %SocketAddrV4::new(reply.external_ip, reply.external_port),
                            ttl = reply.ttl,
                            "port mapping reply"
                        );
                        if is_service_down(reply.error) {
                            down = Some(reply.error);
                            break;
                        }
                        backoff = BACKOFF_MIN;
                        let state = classify(reply.error, reply.external_ip, reply.external_port, &local_ipv4_addresses());
                        changed(tracker.reply(state, Instant::now()));
                    }
                    if let Some(err) = down {
                        break err;
                    }
                    changed(tracker.tick(Instant::now()));
                };
                // Drop the request (and its socket) before waiting to ask again.
                drop(request);
                broken
            }
        };
        tracing::warn!(err, retry_in = ?backoff, "port mapping request to mDNSResponder failed");
        let problem = match classify(err, Ipv4Addr::UNSPECIFIED, 0, &[]) {
            MapState::Failed(problem) => problem,
            _ => MapProblem::Other(err),
        };
        changed(tracker.fail(problem));
        let until = Instant::now() + backoff;
        while !stop.load(Ordering::Relaxed) && Instant::now() < until {
            std::thread::sleep(until.saturating_duration_since(Instant::now()).min(Duration::from_millis(POLL_MS as u64)));
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// The state between replies: a request nobody answers turns into `NoResponse` after a while.
struct Tracker {
    state: MapState,
    /// When a `Requesting` state gives up waiting.
    deadline: Option<Instant>,
}

impl Tracker {
    fn new() -> Self {
        Self { state: MapState::Requesting, deadline: None }
    }

    /// A new request to mDNSResponder.
    fn restart(&mut self, now: Instant) -> Option<MapState> {
        self.deadline = Some(now + NO_RESPONSE_AFTER);
        self.set(MapState::Requesting)
    }

    fn reply(&mut self, state: MapState, now: Instant) -> Option<MapState> {
        if state == MapState::Requesting {
            // No address yet. While waiting, keep the current deadline; after a mapping or an error
            // (say, the mapping was lost on a network change), start a new wait.
            return match self.state {
                MapState::Requesting | MapState::Failed(MapProblem::NoResponse) => None,
                _ => self.restart(now),
            };
        }
        self.deadline = None;
        self.set(state)
    }

    fn tick(&mut self, now: Instant) -> Option<MapState> {
        if self.state == MapState::Requesting && self.deadline.is_some_and(|d| now >= d) {
            self.deadline = None;
            return self.set(MapState::Failed(MapProblem::NoResponse));
        }
        None
    }

    fn fail(&mut self, problem: MapProblem) -> Option<MapState> {
        self.deadline = None;
        self.set(MapState::Failed(problem))
    }

    /// The new state, if it differs from the current one.
    fn set(&mut self, state: MapState) -> Option<MapState> {
        if state == self.state {
            return None;
        }
        self.state = state;
        Some(state)
    }
}

/// One answer from mDNSResponder, as the callback got it (address and ports in host order).
#[derive(Clone, Copy, Debug)]
struct Reply {
    error: i32,
    interface: u32,
    external_ip: Ipv4Addr,
    external_port: u16,
    ttl: u32,
}

/// One request to mDNSResponder. Its callback appends to `inbox`, which only the mapping thread
/// touches: the callback runs inside `DNSServiceProcessResult` on that thread.
struct Request {
    sd: ffi::DNSServiceRef,
    inbox: *mut Vec<Reply>,
}

impl Request {
    /// Ports in host order. Protocol 0 with ports 0 only asks for the router's external address.
    fn new(protocol: u32, internal_port: u16, external_port: u16) -> Result<Self, i32> {
        let inbox = Box::into_raw(Box::new(Vec::new()));
        let mut sd = ptr::null_mut();
        // ttl 0: the system default lease (7200 s), which the daemon renews.
        let err = unsafe {
            ffi::DNSServiceNATPortMappingCreate(
                &mut sd,
                0,
                0,
                protocol,
                internal_port.to_be(),
                external_port.to_be(),
                0,
                on_reply,
                inbox.cast(),
            )
        };
        if err != 0 {
            // No request was made, so nothing else holds the inbox.
            drop(unsafe { Box::from_raw(inbox) });
            return Err(err);
        }
        Ok(Self { sd, inbox })
    }

    /// Waits up to `timeout_ms` for the daemon and returns what it sent. An error means the
    /// connection to the daemon is broken and the request must be made again.
    fn wait(&mut self, timeout_ms: c_int) -> Result<Vec<Reply>, i32> {
        let fd = unsafe { ffi::DNSServiceRefSockFD(self.sd) };
        if fd < 0 {
            return Err(ERR_BAD_REFERENCE);
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let ready = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if ready > 0 {
            // Readable or hung up: either way this doesn't block, and a broken connection is an error.
            let err = unsafe { ffi::DNSServiceProcessResult(self.sd) };
            if err != 0 {
                return Err(err);
            }
        } else if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            std::thread::sleep(Duration::from_millis(timeout_ms as u64));
        }
        Ok(std::mem::take(unsafe { &mut *self.inbox }))
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        unsafe {
            // Closes the socket, so the daemon deletes the mapping; no callback runs after this.
            ffi::DNSServiceRefDeallocate(self.sd);
            drop(Box::from_raw(self.inbox));
        }
    }
}

extern "C" fn on_reply(
    _sd: ffi::DNSServiceRef,
    _flags: u32,
    interface: u32,
    error: i32,
    external_address: u32,
    _protocol: u32,
    _internal_port: u16,
    external_port: u16,
    ttl: u32,
    context: *mut c_void,
) {
    // The address's bytes are in network order in memory, as are the ports.
    let reply = Reply {
        error,
        interface,
        external_ip: Ipv4Addr::from(external_address.to_ne_bytes()),
        external_port: u16::from_be(external_port),
        ttl,
    };
    // Called only from `DNSServiceProcessResult` in `Request::wait`, which holds no reference to the
    // inbox meanwhile; the request (and with it the inbox) outlives every call.
    unsafe { (*context.cast::<Vec<Reply>>()).push(reply) };
}

/// This Mac's IPv4 addresses, on every interface.
fn local_ipv4_addresses() -> Vec<Ipv4Addr> {
    let mut list = Vec::new();
    let mut head: *mut libc::ifaddrs = ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return list;
    }
    let mut next = head;
    while let Some(ifa) = unsafe { next.as_ref() } {
        if let Some(addr) = unsafe { ifa.ifa_addr.as_ref() }
            && c_int::from(addr.sa_family) == libc::AF_INET
        {
            let sin: libc::sockaddr_in = unsafe { ptr::read_unaligned(ifa.ifa_addr.cast()) };
            list.push(Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes()));
        }
        next = ifa.ifa_next;
    }
    unsafe { libc::freeifaddrs(head) };
    list
}

mod ffi {
    use std::ffi::{c_int, c_void};

    #[repr(C)]
    pub struct DNSService {
        _private: [u8; 0],
    }
    pub type DNSServiceRef = *mut DNSService;

    /// `DNSServiceNATPortMappingReply`. The address and ports are in network byte order.
    pub type NatPortMappingReply = extern "C" fn(
        sd: DNSServiceRef,
        flags: u32,
        interface_index: u32,
        error: i32,
        external_address: u32,
        protocol: u32,
        internal_port: u16,
        external_port: u16,
        ttl: u32,
        context: *mut c_void,
    );

    // dns_sd, part of libSystem.
    unsafe extern "C" {
        /// Ports in network byte order.
        pub fn DNSServiceNATPortMappingCreate(
            sd: *mut DNSServiceRef,
            flags: u32,
            interface_index: u32,
            protocol: u32,
            internal_port: u16,
            external_port: u16,
            ttl: u32,
            callback: NatPortMappingReply,
            context: *mut c_void,
        ) -> i32;
        pub fn DNSServiceRefSockFD(sd: DNSServiceRef) -> c_int;
        pub fn DNSServiceProcessResult(sd: DNSServiceRef) -> i32;
        pub fn DNSServiceRefDeallocate(sd: DNSServiceRef);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBLIC: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);
    const LAN: [Ipv4Addr; 2] = [Ipv4Addr::new(192, 168, 1, 20), Ipv4Addr::new(127, 0, 0, 1)];

    fn failed(err: i32, ip: Ipv4Addr) -> MapProblem {
        match classify(err, ip, 47800, &LAN) {
            MapState::Failed(problem) => problem,
            other => panic!("{err} {ip}: expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn no_address_is_still_requesting() {
        assert_eq!(classify(0, Ipv4Addr::UNSPECIFIED, 0, &LAN), MapState::Requesting);
        assert_eq!(classify(0, Ipv4Addr::UNSPECIFIED, 47800, &LAN), MapState::Requesting);
    }

    #[test]
    fn public_router_address_is_mapped_with_the_port_it_gave() {
        assert_eq!(classify(0, PUBLIC, 47801, &LAN), MapState::Mapped { external: SocketAddrV4::new(PUBLIC, 47801) });
        // An address without a port: not mapped yet.
        assert_eq!(classify(0, PUBLIC, 0, &LAN), MapState::Requesting);
    }

    #[test]
    fn own_public_address_needs_no_mapping() {
        let local = [LAN[0], PUBLIC];
        assert_eq!(classify(0, PUBLIC, 47800, &local), MapState::PublicAddress { external: SocketAddrV4::new(PUBLIC, 47800) });
    }

    #[test]
    fn shared_addresses_are_carrier_grade_nat() {
        for ip in [[100, 64, 0, 1], [100, 100, 7, 9], [100, 127, 255, 254], [192, 0, 0, 2]] {
            let ip = Ipv4Addr::from(ip);
            assert_eq!(failed(0, ip), MapProblem::Cgnat { router_external: ip }, "{ip}");
        }
        // Even when it's this Mac's own address: the internet still can't reach it.
        let own = Ipv4Addr::new(100, 72, 1, 2);
        assert_eq!(classify(0, own, 47800, &[own]), MapState::Failed(MapProblem::Cgnat { router_external: own }));
        // Just outside the ranges.
        for ip in [[100, 63, 255, 255], [100, 128, 0, 0], [192, 0, 0, 8], [192, 0, 1, 2]] {
            let ip = Ipv4Addr::from(ip);
            assert_eq!(classify(0, ip, 47800, &LAN), MapState::Mapped { external: SocketAddrV4::new(ip, 47800) }, "{ip}");
        }
    }

    #[test]
    fn private_router_address_is_double_nat() {
        for ip in [[10, 0, 0, 1], [172, 16, 5, 4], [172, 31, 255, 1], [192, 168, 0, 12]] {
            let ip = Ipv4Addr::from(ip);
            assert_eq!(failed(0, ip), MapProblem::DoubleNat { router_external: Some(ip) }, "{ip}");
        }
        let outer = Ipv4Addr::new(192, 168, 0, 12);
        assert_eq!(failed(ERR_DOUBLE_NAT, outer), MapProblem::DoubleNat { router_external: Some(outer) });
        assert_eq!(failed(ERR_DOUBLE_NAT, Ipv4Addr::UNSPECIFIED), MapProblem::DoubleNat { router_external: None });
    }

    #[test]
    fn error_codes() {
        // The other fields are undefined with these errors, so they're ignored.
        for ip in [Ipv4Addr::UNSPECIFIED, PUBLIC] {
            assert_eq!(failed(-65564, ip), MapProblem::Unsupported);
            assert_eq!(failed(-65565, ip), MapProblem::Disabled);
            assert_eq!(failed(-65566, ip), MapProblem::NoRouter);
            assert_eq!(failed(-65563, ip), MapProblem::ServiceDown);
            assert_eq!(failed(-65568, ip), MapProblem::ServiceDown);
            assert_eq!(failed(-65569, ip), MapProblem::ServiceDown);
            assert_eq!(failed(-65550, ip), MapProblem::Firewall);
            assert_eq!(failed(-65557, ip), MapProblem::Other(-65557));
            assert_eq!(failed(-65537, ip), MapProblem::Other(-65537));
        }
        assert!(is_service_down(-65563) && is_service_down(-65568) && is_service_down(-65569) && !is_service_down(-65566));
    }

    #[test]
    fn unanswered_request_becomes_no_response_until_an_answer() {
        let start = Instant::now();
        let mut t = Tracker::new();
        assert_eq!(t.restart(start), None, "starts out requesting");
        // The daemon's "no answer" callback after about 4 s changes nothing by itself.
        assert_eq!(t.reply(MapState::Requesting, start + Duration::from_secs(4)), None);
        assert_eq!(t.tick(start + NO_RESPONSE_AFTER - Duration::from_millis(1)), None);
        assert_eq!(t.tick(start + NO_RESPONSE_AFTER), Some(MapState::Failed(MapProblem::NoResponse)));
        assert_eq!(t.tick(start + NO_RESPONSE_AFTER * 2), None);
        assert_eq!(t.reply(MapState::Requesting, start + Duration::from_secs(30)), None);
        // A later success flips it.
        let mapped = MapState::Mapped { external: SocketAddrV4::new(PUBLIC, 47800) };
        assert_eq!(t.reply(mapped, start + Duration::from_secs(60)), Some(mapped));
        assert_eq!(t.reply(mapped, start + Duration::from_secs(61)), None, "only changes are reported");
    }

    #[test]
    fn lost_mapping_waits_again_before_no_response() {
        let start = Instant::now();
        let mut t = Tracker::new();
        t.restart(start);
        let mapped = MapState::Mapped { external: SocketAddrV4::new(PUBLIC, 47800) };
        assert_eq!(t.reply(mapped, start), Some(mapped));
        assert_eq!(t.tick(start + NO_RESPONSE_AFTER * 2), None);
        let lost = start + Duration::from_secs(100);
        assert_eq!(t.reply(MapState::Requesting, lost), Some(MapState::Requesting));
        assert_eq!(t.tick(lost + NO_RESPONSE_AFTER / 2), None);
        assert_eq!(t.tick(lost + NO_RESPONSE_AFTER), Some(MapState::Failed(MapProblem::NoResponse)));
    }

    #[test]
    fn service_down_then_new_request() {
        let start = Instant::now();
        let mut t = Tracker::new();
        t.restart(start);
        assert_eq!(t.fail(MapProblem::ServiceDown), Some(MapState::Failed(MapProblem::ServiceDown)));
        assert_eq!(t.tick(start + NO_RESPONSE_AFTER), None, "no timer while failed");
        let again = start + Duration::from_secs(1);
        assert_eq!(t.restart(again), Some(MapState::Requesting));
        assert_eq!(t.tick(again + NO_RESPONSE_AFTER), Some(MapState::Failed(MapProblem::NoResponse)));
    }

    #[test]
    fn own_addresses_include_loopback() {
        assert!(local_ipv4_addresses().contains(&Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn mapping_can_be_shared_between_threads() {
        fn shareable<T: Send + Sync>() {}
        shareable::<PortMapping>();
    }

    /// Asks mDNSResponder for the router's external address only: protocol 0 and ports 0. No UDP
    /// port is mapped, but on a PCP router the daemon learns the address by mapping TCP port 9,
    /// and deletes that mapping when the request is dropped. Prints what comes back, then runs the
    /// mapping thread the same way and checks that it stops promptly.
    /// `cargo test -p platform-mac --lib portmap -- --ignored --nocapture`
    #[test]
    #[ignore = "talks to mDNSResponder and the router"]
    fn external_address_query() {
        const ADDRESS_ONLY: u32 = 0;
        let started = Instant::now();
        let mut request = Request::new(ADDRESS_ONLY, 0, 0).expect("DNSServiceNATPortMappingCreate");
        let mut replies = Vec::new();
        while replies.is_empty() && started.elapsed() < Duration::from_secs(12) {
            replies = request.wait(POLL_MS).expect("connection to mDNSResponder");
        }
        drop(request);
        println!("after {:?}:", started.elapsed());
        for r in &replies {
            println!("  {r:?} -> {:?}", classify(r.error, r.external_ip, r.external_port, &local_ipv4_addresses()));
        }
        assert!(!replies.is_empty(), "no callback within 12 s");

        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        let mapping = PortMapping::start_request(ADDRESS_ONLY, 0, 0, move |state| {
            let _ = tx.send(state);
        });
        let until = started + Duration::from_secs(8);
        while let Ok(state) = rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            println!("after {:?}: {state:?}", started.elapsed());
        }
        println!("state after {:?}: {:?}", started.elapsed(), mapping.state());
        let stopping = Instant::now();
        mapping.stop();
        println!("stopped in {:?}", stopping.elapsed());
        assert!(stopping.elapsed() < Duration::from_secs(1));
    }
}
