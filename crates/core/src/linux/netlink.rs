//! Just enough netlink to avoid shelling out to `ip`/`iptables`: rtnetlink (links, addresses,
//! routes) and one nf_tables batch (the NAT rule). Encoders are pure functions, so they are unit tested.
use crate::error::{Error, Result};
use std::{io, mem::size_of, os::fd::{AsRawFd, FromRawFd, OwnedFd}};

pub const NETLINK_ROUTE: i32 = 0;
pub const NETLINK_NETFILTER: i32 = 12;

const REQUEST: u16 = 1;
const ACK: u16 = 4;
const EXCL: u16 = 0x200;
const CREATE: u16 = 0x400;
const APPEND: u16 = 0x800;
const NESTED: u16 = 0x8000;

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_NEWADDR: u16 = 20;
const RTM_NEWROUTE: u16 = 24;

fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

/// One attribute (`struct nlattr` + payload, padded to 4 bytes).
pub fn attr(ty: u16, data: &[u8]) -> Vec<u8> {
    let len = 4 + data.len();
    let mut v = Vec::with_capacity(pad4(len));
    v.extend((len as u16).to_ne_bytes());
    v.extend(ty.to_ne_bytes());
    v.extend(data);
    v.resize(pad4(len), 0);
    v
}

pub fn nest(ty: u16, kids: &[Vec<u8>]) -> Vec<u8> {
    attr(ty | NESTED, &kids.concat())
}

fn strz(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

fn be32(n: u32) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}

fn ne32(n: u32) -> Vec<u8> {
    n.to_ne_bytes().to_vec()
}

/// `struct nlmsghdr` + fixed header + attributes.
pub fn message(ty: u16, flags: u16, fixed: &[u8], attrs: &[Vec<u8>]) -> Vec<u8> {
    let body = attrs.concat();
    let head = 16 + pad4(fixed.len());
    let len = head + body.len();
    let mut v = Vec::with_capacity(len);
    v.extend((len as u32).to_ne_bytes());
    v.extend(ty.to_ne_bytes());
    v.extend(flags.to_ne_bytes());
    v.extend(1u32.to_ne_bytes()); // seq: replies are matched by order, one request at a time
    v.extend(0u32.to_ne_bytes()); // pid
    v.extend(fixed);
    v.resize(head, 0);
    v.extend(body);
    v
}

/// `struct ifinfomsg`
fn ifinfo(index: i32, flags: u32, change: u32) -> Vec<u8> {
    let mut v = vec![0u8; 4]; // family, pad, type
    v.extend(index.to_ne_bytes());
    v.extend(flags.to_ne_bytes());
    v.extend(change.to_ne_bytes());
    v
}

pub fn new_bridge(name: &str) -> Vec<u8> {
    message(
        RTM_NEWLINK,
        REQUEST | ACK | CREATE | EXCL,
        &ifinfo(0, 0, 0),
        &[attr(3, &strz(name)), nest(18, &[attr(1, &strz("bridge"))])],
    )
}

pub fn new_veth(host: &str, peer: &str) -> Vec<u8> {
    let peer_info = nest(1, &[ifinfo(0, 0, 0), attr(3, &strz(peer))]); // VETH_INFO_PEER
    message(
        RTM_NEWLINK,
        REQUEST | ACK | CREATE | EXCL,
        &ifinfo(0, 0, 0),
        &[attr(3, &strz(host)), nest(18, &[attr(1, &strz("veth")), nest(2, &[peer_info])])],
    )
}

pub fn link_up(index: u32) -> Vec<u8> {
    message(RTM_NEWLINK, REQUEST | ACK, &ifinfo(index as i32, 1, 1), &[])
}

pub fn set_master(index: u32, master: u32) -> Vec<u8> {
    message(RTM_NEWLINK, REQUEST | ACK, &ifinfo(index as i32, 0, 0), &[attr(10, &ne32(master))])
}

/// Move a link into the network namespace of `pid`, renaming it on the way.
pub fn move_to_ns(index: u32, pid: u32, new_name: &str) -> Vec<u8> {
    message(
        RTM_NEWLINK,
        REQUEST | ACK,
        &ifinfo(index as i32, 0, 0),
        &[attr(3, &strz(new_name)), attr(19, &ne32(pid))],
    )
}

pub fn add_addr(index: u32, ip: &[u8], prefix: u8) -> Vec<u8> {
    let v6 = ip.len() == 16;
    let mut fixed = vec![if v6 { 10u8 /* AF_INET6 */ } else { 2u8 /* AF_INET */ }, prefix, 0, 0];
    fixed.extend(index.to_ne_bytes());
    let mut attrs = vec![attr(2, ip), attr(1, ip)];
    if v6 {
        attrs.push(attr(8, &ne32(0x02))); // IFA_FLAGS = IFA_F_NODAD: usable at once, no duplicate-detection delay
    }
    message(RTM_NEWADDR, REQUEST | ACK | CREATE | EXCL, &fixed, &attrs)
}

pub fn add_default_route(gateway: &[u8]) -> Vec<u8> {
    // struct rtmsg: family, dst_len, src_len, tos, table=main, protocol=boot, scope=universe, type=unicast, flags
    let family = if gateway.len() == 16 { 10u8 } else { 2u8 };
    let fixed = [family, 0, 0, 0, 254, 3, 0, 1, 0, 0, 0, 0];
    message(RTM_NEWROUTE, REQUEST | ACK | CREATE | EXCL, &fixed, &[attr(5, gateway)])
}

pub fn del_link(index: u32) -> Vec<u8> {
    message(RTM_DELLINK, REQUEST | ACK, &ifinfo(index as i32, 0, 0), &[])
}

// ---- nf_tables: `table <ip|ip6> <t> { chain <c> { type nat hook postrouting priority 100; <saddr> <net>/<n> masquerade ... } }`

const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFT_MSG_NEWTABLE: u16 = 0;
const NFT_MSG_DELTABLE: u16 = 2;
const NFT_MSG_NEWCHAIN: u16 = 3;
const NFT_MSG_NEWRULE: u16 = 6;

fn nft_msg(v6: bool, ty: u16, flags: u16, attrs: &[Vec<u8>]) -> Vec<u8> {
    let nfgenmsg = [if v6 { 10u8 /* NFPROTO_IPV6 */ } else { 2u8 /* NFPROTO_IPV4 */ }, 0, 0, 0];
    message((NFNL_SUBSYS_NFTABLES << 8) | ty, REQUEST | flags, &nfgenmsg, attrs)
}

fn expr(name: &str, data: Option<Vec<Vec<u8>>>) -> Vec<u8> {
    let mut kids = vec![attr(1, &strz(name))];
    if let Some(d) = data {
        kids.push(nest(2, &d));
    }
    nest(1, &kids) // NFTA_LIST_ELEM
}

/// Netmask of `prefix` bits as `len` bytes.
fn mask_bytes(prefix: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| (prefix as usize).saturating_sub(i * 8).min(8)).map(|b| (0xffu16 << (8 - b)) as u8).collect()
}

/// batch begin/end carry an nfgenmsg whose res_id (big endian) names the subsystem
fn batch_marker(ty: u16) -> Vec<u8> {
    message(ty, REQUEST, &[0, 0, 0, NFNL_SUBSYS_NFTABLES as u8], &[])
}

/// One transaction that (re)creates the table with one masquerade rule per network in `nets`
/// (network address bytes: 4 for IPv4, 16 for IPv6; and a prefix length). Idempotent: an existing table is
/// replaced atomically, so there is never a moment without NAT and never a duplicate rule.
pub fn nat_batch(table: &str, chain: &str, nets: &[(Vec<u8>, u8)], v6: bool) -> Vec<u8> {
    let (offset, len) = if v6 { (8u32, 16usize) } else { (12, 4) }; // saddr inside the IP header
    let reg = || attr(1, &be32(1)); // NFT_REG_1
    let mut batch = batch_marker(0x10);
    batch.extend(nft_msg(v6, NFT_MSG_NEWTABLE, ACK | CREATE, &[attr(1, &strz(table))])); // makes the delete below safe
    batch.extend(nft_msg(v6, NFT_MSG_DELTABLE, ACK, &[attr(1, &strz(table))]));
    batch.extend(nft_msg(v6, NFT_MSG_NEWTABLE, ACK | CREATE | EXCL, &[attr(1, &strz(table))]));
    batch.extend(nft_msg(
        v6,
        NFT_MSG_NEWCHAIN,
        ACK | CREATE,
        &[
            attr(1, &strz(table)),
            attr(3, &strz(chain)),
            nest(4, &[attr(1, &be32(4)), attr(2, &be32(100))]), // hook postrouting, priority 100
            attr(7, &strz("nat")),
        ],
    ));
    for (net, prefix) in nets {
        let rule_exprs = vec![
            // load saddr
            expr("payload", Some(vec![reg(), attr(2, &be32(1)), attr(3, &be32(offset)), attr(4, &be32(len as u32))])),
            // & netmask
            expr(
                "bitwise",
                Some(vec![
                    reg(),
                    attr(2, &be32(1)),
                    attr(3, &be32(len as u32)),
                    nest(4, &[attr(1, &mask_bytes(*prefix, len))]),
                    nest(5, &[attr(1, &vec![0; len])]),
                ]),
            ),
            // == network
            expr("cmp", Some(vec![reg(), attr(2, &be32(0)), nest(3, &[attr(1, net)])])),
            expr("masq", None),
        ];
        batch.extend(nft_msg(
            v6,
            NFT_MSG_NEWRULE,
            ACK | CREATE | APPEND,
            &[attr(1, &strz(table)), attr(2, &strz(chain)), nest(4, &rule_exprs)],
        ));
    }
    batch.extend(batch_marker(0x11));
    batch
}

/// One transaction that removes the table (creating it first so a missing one is not an error).
pub fn drop_table_batch(table: &str, v6: bool) -> Vec<u8> {
    let mut batch = batch_marker(0x10);
    batch.extend(nft_msg(v6, NFT_MSG_NEWTABLE, ACK | CREATE, &[attr(1, &strz(table))]));
    batch.extend(nft_msg(v6, NFT_MSG_DELTABLE, ACK, &[attr(1, &strz(table))]));
    batch.extend(batch_marker(0x11));
    batch
}

// ---- socket

pub struct Socket(OwnedFd);

impl Socket {
    pub fn open(protocol: i32) -> Result<Socket> {
        let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, protocol) };
        if fd < 0 {
            return Err(Error::sys("netlink socket"));
        }
        let s = Socket(unsafe { OwnedFd::from_raw_fd(fd) });
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as u16;
        let r = unsafe { libc::bind(fd, (&addr as *const libc::sockaddr_nl).cast(), size_of::<libc::sockaddr_nl>() as u32) };
        if r < 0 {
            return Err(Error::sys("netlink bind"));
        }
        let tv = libc::timeval { tv_sec: 2, tv_usec: 0 };
        unsafe {
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, (&tv as *const libc::timeval).cast(), size_of::<libc::timeval>() as u32)
        };
        Ok(s)
    }

    fn send(&self, data: &[u8]) -> Result<()> {
        let r = unsafe { libc::send(self.0.as_raw_fd(), data.as_ptr().cast(), data.len(), 0) };
        if r < 0 { Err(Error::sys("netlink send")) } else { Ok(()) }
    }

    /// Error codes (0 = success) of up to `n` acknowledgements.
    fn acks(&self, n: usize) -> Vec<i32> {
        let mut out = vec![];
        let mut buf = vec![0u8; 16384];
        while out.len() < n && out.iter().all(|&e| e == 0) {
            let r = unsafe { libc::recv(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if r <= 0 {
                break; // timeout
            }
            out.extend(parse_acks(&buf[..r as usize]));
        }
        out
    }

    /// Send one request and require success. `tolerate` is an errno (positive) treated as success.
    pub fn request(&self, ctx: &str, msg: Vec<u8>, tolerate: Option<i32>) -> Result<()> {
        self.send(&msg)?;
        match self.acks(1).first() {
            Some(0) => Ok(()),
            Some(&e) if Some(-e) == tolerate => Ok(()),
            Some(&e) => Err(Error::io(ctx, io::Error::from_raw_os_error(-e))),
            None => Err(Error::Usage(format!("{ctx}: no reply from the kernel"))),
        }
    }

    /// Send an nf_tables batch; every message in it must be acknowledged with success.
    pub fn nft_batch(&self, batch: &[u8]) -> Result<()> {
        self.send(batch)?;
        let want = count_acks(batch);
        let acks = self.acks(want);
        if acks.len() == want && acks.iter().all(|&e| e == 0) {
            return Ok(());
        }
        let bad = acks.iter().copied().find(|&e| e != 0).unwrap_or(-libc::ETIMEDOUT);
        Err(Error::io("nftables NAT rule", io::Error::from_raw_os_error(-bad)))
    }
}

/// How many messages of a batch ask for an acknowledgement.
fn count_acks(batch: &[u8]) -> usize {
    let (mut off, mut n) = (0, 0);
    while off + 16 <= batch.len() {
        let len = u32::from_ne_bytes(batch[off..off + 4].try_into().unwrap()) as usize;
        n += (u16::from_ne_bytes(batch[off + 6..off + 8].try_into().unwrap()) & ACK != 0) as usize;
        off += pad4(len.max(16));
    }
    n
}

/// Pull the error code out of every NLMSG_ERROR in a receive buffer.
fn parse_acks(buf: &[u8]) -> Vec<i32> {
    let mut out = vec![];
    let mut off = 0;
    while off + 16 <= buf.len() {
        let len = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        let ty = u16::from_ne_bytes(buf[off + 4..off + 6].try_into().unwrap());
        if len < 16 {
            break;
        }
        if ty == 2 && off + 20 <= buf.len() {
            out.push(i32::from_ne_bytes(buf[off + 16..off + 20].try_into().unwrap()));
        }
        off += pad4(len);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_are_padded_and_nested_flagged() {
        let a = attr(3, b"abc");
        assert_eq!(a.len(), 8); // 4 header + 3 data + 1 pad
        assert_eq!(u16::from_ne_bytes([a[0], a[1]]), 7);
        let n = nest(18, &[attr(1, b"x")]);
        assert_eq!(u16::from_ne_bytes([n[2], n[3]]), 18 | 0x8000);
    }

    #[test]
    fn message_header_is_consistent() {
        let m = new_veth("rdvaaaaaaaa", "rdpaaaaaaaa");
        assert_eq!(u32::from_ne_bytes(m[0..4].try_into().unwrap()) as usize, m.len());
        assert_eq!(m.len() % 4, 0);
        assert_eq!(u16::from_ne_bytes(m[4..6].try_into().unwrap()), RTM_NEWLINK);
        assert!(m.windows(4).any(|w| w == b"veth"));
    }

    #[test]
    fn batch_is_one_aligned_transaction() {
        let b = nat_batch("t", "c", &[(vec![172, 30, 0, 0], 16), (vec![10, 5, 0, 0], 24)], false);
        let mut off = 0;
        let mut types = vec![];
        while off < b.len() {
            let len = u32::from_ne_bytes(b[off..off + 4].try_into().unwrap()) as usize;
            types.push(u16::from_ne_bytes(b[off + 4..off + 6].try_into().unwrap()));
            assert!(len >= 16 && len.is_multiple_of(4));
            off += len;
        }
        assert_eq!(off, b.len());
        let nft = |t: u16| (NFNL_SUBSYS_NFTABLES << 8) | t;
        assert_eq!(types, [0x10, nft(0), nft(2), nft(0), nft(3), nft(6), nft(6), 0x11]);
        assert_eq!(count_acks(&b), 6);
        assert!(b.windows(3).any(|w| w == b"nat")); // chain type
        assert!(b.windows(4).any(|w| w == [0xff, 0xff, 0x00, 0x00])); // /16 mask
        assert!(b.windows(4).any(|w| w == [0xff, 0xff, 0xff, 0x00])); // /24 mask
    }

    #[test]
    fn ipv6_batch_and_masks() {
        assert_eq!(mask_bytes(64, 16), [&[0xff; 8][..], &[0; 8][..]].concat());
        assert_eq!(mask_bytes(20, 4), [0xff, 0xff, 0xf0, 0]);
        let net = [0xfd, 0, 0x5a, 0x57, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let b = nat_batch("t", "c", &[(net.to_vec(), 64)], true);
        assert_eq!(b[16], 10, "nfgenmsg family is NFPROTO_IPV6");
        assert!(b.windows(16).any(|w| w == net));
        let d = drop_table_batch("t", false);
        assert_eq!(count_acks(&d), 2);
    }

    #[test]
    fn address_and_route_families() {
        let a4 = add_addr(3, &[10, 0, 0, 2], 24);
        let a6 = add_addr(3, &[0xfd; 16], 64);
        assert_eq!((a4[16], a6[16]), (2, 10));
        assert!(a6.len() > a4.len() + 12, "v6 carries 16-byte addresses and IFA_FLAGS");
        assert_eq!(add_default_route(&[0xfd; 16])[16], 10);
        assert_eq!(add_default_route(&[10, 0, 0, 1])[16], 2);
    }

    #[test]
    fn parses_acks() {
        let mut buf = vec![];
        buf.extend(36u32.to_ne_bytes());
        buf.extend(2u16.to_ne_bytes());
        buf.extend([0u8; 10]);
        buf.extend((-17i32).to_ne_bytes()); // -EEXIST
        buf.extend([0u8; 16]);
        assert_eq!(parse_acks(&buf), [-17]);
    }
}
