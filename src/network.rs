//! Network state the shell's network popup shows: the first IPv4 address of every interface, as
//! `ip -4 addr` lists it, the frequency of every connected Wi-Fi interface, as `iw dev IFACE link`
//! prints it, and NetworkManager's connectivity, as `nmcli general` prints it. Addresses and
//! frequencies come from the kernel, so they work without NetworkManager; changes are followed
//! through rtnetlink and nl80211's MLME events instead of polling.

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde_json::{Map, Value as Json};

pub const NM_BUS_NAME: &str = "org.freedesktop.NetworkManager";
pub const NM_PATH: &str = "/org/freedesktop/NetworkManager";

const NLMSG_HDRLEN: usize = 16;
const NLA_HDRLEN: usize = 4;
const GENL_HDRLEN: usize = 4;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_DUMP: u16 = 0x300;
const NLA_TYPE_MASK: u16 = 0x3fff;
const SOL_NETLINK: libc::c_int = 270;
const NETLINK_ADD_MEMBERSHIP: libc::c_int = 1;
const RTMGRP_LINK: u32 = 0x1;
const RTMGRP_IPV4_IFADDR: u32 = 0x10;
const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const CTRL_ATTR_MCAST_GROUPS: u16 = 7;
const CTRL_ATTR_MCAST_GRP_NAME: u16 = 1;
const CTRL_ATTR_MCAST_GRP_ID: u16 = 2;
const NL80211_CMD_GET_INTERFACE: u8 = 5;
const NL80211_ATTR_IFNAME: u16 = 4;
const NL80211_ATTR_WIPHY_FREQ: u16 = 38;
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

static SEQUENCE: AtomicU32 = AtomicU32::new(1);

/// NetworkManager's `NMConnectivityState` as nmcli names it
pub fn connectivity_name(state: u32) -> &'static str {
    match state {
        1 => "none",
        2 => "portal",
        3 => "limited",
        4 => "full",
        _ => "unknown",
    }
}

fn align(len: usize) -> usize {
    (len + 3) & !3
}

/// The attributes in `data` as (type, payload) pairs
fn attributes(mut data: &[u8]) -> Vec<(u16, &[u8])> {
    let mut found = Vec::new();
    while data.len() >= NLA_HDRLEN {
        let len = u16::from_ne_bytes([data[0], data[1]]) as usize;
        let kind = u16::from_ne_bytes([data[2], data[3]]) & NLA_TYPE_MASK;
        if len < NLA_HDRLEN || len > data.len() {
            break;
        }
        found.push((kind, &data[NLA_HDRLEN..len]));
        data = &data[align(len).min(data.len())..];
    }
    found
}

fn attribute(buf: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    let len = NLA_HDRLEN + payload.len();
    buf.extend_from_slice(&(len as u16).to_ne_bytes());
    buf.extend_from_slice(&kind.to_ne_bytes());
    buf.extend_from_slice(payload);
    buf.resize(align(buf.len()), 0);
}

fn u32_of(payload: &[u8]) -> Option<u32> {
    Some(u32::from_ne_bytes(payload.get(..4)?.try_into().ok()?))
}

fn string_of(payload: &[u8]) -> Option<String> {
    let end = payload.iter().position(|b| *b == 0).unwrap_or(payload.len());
    std::str::from_utf8(&payload[..end]).ok().map(str::to_owned)
}

/// The messages in a datagram as (type, payload after the header) pairs
fn messages(mut data: &[u8]) -> Vec<(u16, &[u8])> {
    let mut found = Vec::new();
    while data.len() >= NLMSG_HDRLEN {
        let len = u32::from_ne_bytes(data[..4].try_into().unwrap()) as usize;
        let kind = u16::from_ne_bytes([data[4], data[5]]);
        if len < NLMSG_HDRLEN || len > data.len() {
            break;
        }
        found.push((kind, &data[NLMSG_HDRLEN..len]));
        data = &data[align(len).min(data.len())..];
    }
    found
}

struct Netlink(OwnedFd);

impl Netlink {
    fn open(protocol: libc::c_int, groups: u32) -> io::Result<Netlink> {
        // SAFETY: socket has no preconditions; a valid descriptor is owned from here on
        let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, protocol) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh descriptor nothing else owns
        let socket = Netlink(unsafe { OwnedFd::from_raw_fd(fd) });
        // SAFETY: sockaddr_nl is plain data, valid when zeroed
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_groups = groups;
        let size = std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t;
        // SAFETY: addr is a valid sockaddr_nl of the given size
        if unsafe { libc::bind(fd, (&raw const addr).cast(), size) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let timeout = libc::timeval { tv_sec: REPLY_TIMEOUT.as_secs() as libc::time_t, tv_usec: 0 };
        let size = std::mem::size_of::<libc::timeval>() as libc::socklen_t;
        // SAFETY: timeout is a valid timeval of the given size
        unsafe { libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, (&raw const timeout).cast(), size) };
        Ok(socket)
    }

    fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    fn join(&self, group: u32) -> io::Result<()> {
        let size = std::mem::size_of::<u32>() as libc::socklen_t;
        // SAFETY: group is a valid u32 of the given size
        let done = unsafe {
            libc::setsockopt(self.fd(), SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, (&raw const group).cast(), size)
        };
        if done < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    fn receive(&self, buf: &mut [u8], flags: libc::c_int) -> io::Result<usize> {
        // SAFETY: buf is writable for its whole length
        let n = unsafe { libc::recv(self.fd(), buf.as_mut_ptr().cast(), buf.len(), flags) };
        if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
    }

    /// Reads whatever arrived without waiting
    fn drain(&self) {
        let mut buf = [0u8; 8192];
        while self.receive(&mut buf, libc::MSG_DONTWAIT).is_ok_and(|n| n > 0) {}
    }

    /// Sends a request and returns the payloads of its replies, all of them for a dump
    fn request(&self, kind: u16, flags: u16, body: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut message = Vec::with_capacity(NLMSG_HDRLEN + body.len());
        message.extend_from_slice(&((NLMSG_HDRLEN + body.len()) as u32).to_ne_bytes());
        message.extend_from_slice(&kind.to_ne_bytes());
        message.extend_from_slice(&(NLM_F_REQUEST | flags).to_ne_bytes());
        message.extend_from_slice(&sequence.to_ne_bytes());
        message.extend_from_slice(&0u32.to_ne_bytes());
        message.extend_from_slice(body);
        // SAFETY: message is readable for its whole length
        if unsafe { libc::send(self.fd(), message.as_ptr().cast(), message.len(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let dump = flags & NLM_F_DUMP == NLM_F_DUMP;
        let mut replies = Vec::new();
        let mut buf = vec![0u8; 32768];
        loop {
            let n = self.receive(&mut buf, 0)?;
            for (kind, payload) in messages(&buf[..n]) {
                match kind {
                    NLMSG_DONE => return Ok(replies),
                    NLMSG_ERROR => {
                        let code = payload.get(..4).map_or(0, |b| i32::from_ne_bytes(b.try_into().unwrap()));
                        return if code == 0 { Ok(replies) } else { Err(io::Error::from_raw_os_error(-code)) };
                    }
                    _ => replies.push(payload.to_vec()),
                }
            }
            if !dump && !replies.is_empty() {
                return Ok(replies);
            }
        }
    }
}

fn genl_header(command: u8) -> Vec<u8> {
    vec![command, 1, 0, 0]
}

/// nl80211's family id and the id of its MLME event group, which signals connects, roams,
/// disconnects and channel switches
fn nl80211_family(socket: &Netlink) -> io::Result<(u16, Option<u32>)> {
    let mut body = genl_header(CTRL_CMD_GETFAMILY);
    attribute(&mut body, CTRL_ATTR_FAMILY_NAME, b"nl80211\0");
    let reply = socket.request(GENL_ID_CTRL, 0, &body)?;
    let payload = reply.first().and_then(|r| r.get(GENL_HDRLEN..)).unwrap_or_default();
    let mut family = None;
    let mut mlme = None;
    for (kind, value) in attributes(payload) {
        match kind {
            CTRL_ATTR_FAMILY_ID => family = value.get(..2).map(|b| u16::from_ne_bytes([b[0], b[1]])),
            CTRL_ATTR_MCAST_GROUPS => {
                for (_, group) in attributes(value) {
                    let fields = attributes(group);
                    let name =
                        fields.iter().find(|(k, _)| *k == CTRL_ATTR_MCAST_GRP_NAME).and_then(|(_, v)| string_of(v));
                    if name.as_deref() == Some("mlme") {
                        mlme = fields.iter().find(|(k, _)| *k == CTRL_ATTR_MCAST_GRP_ID).and_then(|(_, v)| u32_of(v));
                    }
                }
            }
            _ => {}
        }
    }
    family.map(|f| (f, mlme)).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no nl80211 family"))
}

/// The first IPv4 address the kernel lists for every interface that has one
fn addresses() -> BTreeMap<String, Ipv4Addr> {
    let mut found = BTreeMap::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: list receives a list that freeifaddrs releases below
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return found;
    }
    let mut entry = list;
    while !entry.is_null() {
        // SAFETY: entry is a node of the list getifaddrs returned, alive until freeifaddrs
        let ifa = unsafe { &*entry };
        entry = ifa.ifa_next;
        // SAFETY: a non-null ifa_addr points to a sockaddr whose family says what it holds
        if ifa.ifa_addr.is_null() || i32::from(unsafe { (*ifa.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        // SAFETY: an AF_INET address is a sockaddr_in, and ifa_name a C string
        let (addr, name) = unsafe { (&*ifa.ifa_addr.cast::<libc::sockaddr_in>(), CStr::from_ptr(ifa.ifa_name)) };
        let Ok(name) = name.to_str() else { continue };
        found.entry(name.to_owned()).or_insert_with(|| Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)));
    }
    // SAFETY: list came from getifaddrs and is released once
    unsafe { libc::freeifaddrs(list) };
    found
}

/// The frequency in MHz of every Wi-Fi interface operating on a channel, as for a connected one
fn frequencies(socket: &Netlink, family: u16) -> BTreeMap<String, u32> {
    let replies = socket.request(family, NLM_F_DUMP, &genl_header(NL80211_CMD_GET_INTERFACE)).unwrap_or_default();
    let mut found = BTreeMap::new();
    for reply in &replies {
        let fields = attributes(reply.get(GENL_HDRLEN..).unwrap_or_default());
        let name = fields.iter().find(|(k, _)| *k == NL80211_ATTR_IFNAME).and_then(|(_, v)| string_of(v));
        let freq = fields.iter().find(|(k, _)| *k == NL80211_ATTR_WIPHY_FREQ).and_then(|(_, v)| u32_of(v));
        if let (Some(name), Some(freq)) = (name, freq) {
            found.insert(name, freq);
        }
    }
    found
}

/// `{"wlan0": {"ipv4": "192.168.1.5", "frequency": 5180}, "eth0": {"ipv4": "10.0.0.2"}}`
pub fn interfaces_json(addresses: &BTreeMap<String, Ipv4Addr>, frequencies: &BTreeMap<String, u32>) -> Json {
    let mut all: BTreeMap<&str, Map<String, Json>> = BTreeMap::new();
    for (name, addr) in addresses {
        all.entry(name).or_default().insert("ipv4".into(), addr.to_string().into());
    }
    for (name, freq) in frequencies {
        all.entry(name).or_default().insert("frequency".into(), (*freq).into());
    }
    Json::Object(all.into_iter().map(|(name, fields)| (name.to_owned(), Json::Object(fields))).collect())
}

/// The sockets that turn readable when addresses, links or Wi-Fi associations change, and the one
/// nl80211 is asked through
pub struct Watch {
    route: Option<Netlink>,
    events: Option<Netlink>,
    nl80211: Option<(Netlink, u16)>,
}

impl Watch {
    pub fn open() -> Watch {
        let route = Netlink::open(libc::NETLINK_ROUTE, RTMGRP_LINK | RTMGRP_IPV4_IFADDR)
            .inspect_err(|e| eprintln!("serpantinum-core: cannot follow network links: {e}"))
            .ok();
        let nl80211 = Netlink::open(libc::NETLINK_GENERIC, 0)
            .and_then(|socket| nl80211_family(&socket).map(|(family, mlme)| (socket, family, mlme)))
            .ok();
        let events = nl80211.as_ref().and_then(|(_, _, mlme)| *mlme).and_then(|group| {
            let socket = Netlink::open(libc::NETLINK_GENERIC, 0).ok()?;
            socket.join(group).ok()?;
            Some(socket)
        });
        Watch { route, events, nl80211: nl80211.map(|(socket, family, _)| (socket, family)) }
    }

    pub fn fds(&self) -> Vec<RawFd> {
        [&self.route, &self.events].into_iter().flatten().map(Netlink::fd).collect()
    }

    pub fn drain(&self) {
        [&self.route, &self.events].into_iter().flatten().for_each(Netlink::drain);
    }

    pub fn interfaces(&self) -> Json {
        let frequencies =
            self.nl80211.as_ref().map(|(socket, family)| frequencies(socket, *family)).unwrap_or_default();
        interfaces_json(&addresses(), &frequencies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connectivity_is_named_as_nmcli_names_it() {
        assert_eq!(connectivity_name(0), "unknown");
        assert_eq!(connectivity_name(2), "portal");
        assert_eq!(connectivity_name(4), "full");
        assert_eq!(connectivity_name(9), "unknown");
    }

    #[test]
    fn attributes_round_trip() {
        let mut buf = Vec::new();
        attribute(&mut buf, CTRL_ATTR_FAMILY_NAME, b"nl80211\0");
        attribute(&mut buf, CTRL_ATTR_FAMILY_ID, &28u16.to_ne_bytes());
        let found = attributes(&buf);
        assert_eq!(found.len(), 2);
        assert_eq!(string_of(found[0].1).as_deref(), Some("nl80211"));
        assert_eq!(found[1], (CTRL_ATTR_FAMILY_ID, &28u16.to_ne_bytes()[..]));
    }

    #[test]
    fn truncated_attributes_are_dropped() {
        let mut buf = Vec::new();
        attribute(&mut buf, 1, &7u32.to_ne_bytes());
        buf.extend_from_slice(&[40, 0, 2, 0]);
        assert_eq!(attributes(&buf).len(), 1);
    }

    #[test]
    fn interfaces_merge_addresses_and_frequencies() {
        let addresses = BTreeMap::from([
            ("lo".to_owned(), Ipv4Addr::LOCALHOST),
            ("wlan0".to_owned(), Ipv4Addr::new(192, 168, 1, 5)),
        ]);
        let frequencies = BTreeMap::from([("wlan0".to_owned(), 5180), ("wlan1".to_owned(), 2412)]);
        assert_eq!(
            interfaces_json(&addresses, &frequencies).to_string(),
            r#"{"lo":{"ipv4":"127.0.0.1"},"wlan0":{"frequency":5180,"ipv4":"192.168.1.5"},"wlan1":{"frequency":2412}}"#
        );
    }

    #[test]
    fn the_kernel_answers() {
        assert!(Watch::open().interfaces().is_object());
    }
}
