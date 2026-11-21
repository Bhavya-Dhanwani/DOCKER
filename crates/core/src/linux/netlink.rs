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

pub fn add_addr(index: u32, ip: [u8; 4], prefix: u8) -> Vec<u8> {
    let mut fixed = vec![2u8 /* AF_INET */, prefix, 0, 0];
    fixed.extend(index.to_ne_bytes());
    message(RTM_NEWADDR, REQUEST | ACK | CREATE | EXCL, &fixed, &[attr(2, &ip), attr(1, &ip)])
}

pub fn add_default_route(gateway: [u8; 4]) -> Vec<u8> {
    // struct rtmsg: family, dst_len, src_len, tos, table=main, protocol=boot, scope=universe, type=unicast, flags
    let fixed = [2u8, 0, 0, 0, 254, 3, 0, 1, 0, 0, 0, 0];
    message(RTM_NEWROUTE, REQUEST | ACK | CREATE | EXCL, &fixed, &[attr(5, &gateway)])
}

// ---- nf_tables: `table ip <t> { chain <c> { type nat hook postrouting priority 100; ip saddr <net>/<n> masquerade } }`

const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFT_MSG_NEWTABLE: u16 = 0;
const NFT_MSG_NEWCHAIN: u16 = 3;
const NFT_MSG_NEWRULE: u16 = 6;

fn nft_msg(ty: u16, flags: u16, attrs: &[Vec<u8>]) -> Vec<u8> {
    let nfgenmsg = [2u8 /* NFPROTO_IPV4 */, 0, 0, 0];
    message((NFNL_SUBSYS_NFTABLES << 8) | ty, REQUEST | flags, &nfgenmsg, attrs)
}

fn expr(name: &str, data: Option<Vec<Vec<u8>>>) -> Vec<u8> {
    let mut kids = vec![attr(1, &strz(name))];
    if let Some(d) = data {
        kids.push(nest(2, &d));
    }
    nest(1, &kids) // NFTA_LIST_ELEM
}

/// One transaction: create table + chain + rule. Fails with EEXIST (applying nothing) if the table is there.
pub fn masquerade_batch(table: &str, chain: &str, net: [u8; 4], prefix: u8) -> Vec<u8> {
    let mask = (u32::MAX << (32 - prefix as u32)).to_be_bytes();
    let reg = || attr(1, &be32(1)); // NFT_REG_1
    let rule_exprs = vec![
        // load ip saddr
        expr("payload", Some(vec![reg(), attr(2, &be32(1)), attr(3, &be32(12)), attr(4, &be32(4))])),
        // & netmask
        expr(
            "bitwise",
            Some(vec![reg(), attr(2, &be32(1)), attr(3, &be32(4)), nest(4, &[attr(1, &mask)]), nest(5, &[attr(1, &[0; 4])])]),
        ),
        // == network
        expr("cmp", Some(vec![reg(), attr(2, &be32(0)), nest(3, &[attr(1, &net)])])),
        expr("masq", None),
    ];
    // batch begin/end carry an nfgenmsg whose res_id (big endian) names the subsystem
    let begin = |ty: u16| message(ty, REQUEST, &[0, 0, 0, NFNL_SUBSYS_NFTABLES as u8], &[]);
    let mut batch = begin(0x10);
    batch.extend(nft_msg(NFT_MSG_NEWTABLE, ACK | CREATE | EXCL, &[attr(1, &strz(table))]));
    batch.extend(nft_msg(
        NFT_MSG_NEWCHAIN,
        ACK | CREATE,
        &[
            attr(1, &strz(table)),
            attr(3, &strz(chain)),
            nest(4, &[attr(1, &be32(4)), attr(2, &be32(100))]), // hook postrouting, priority 100
            attr(7, &strz("nat")),
        ],
    ));
    batch.extend(nft_msg(
        NFT_MSG_NEWRULE,
        ACK | CREATE | APPEND,
        &[attr(1, &strz(table)), attr(2, &strz(chain)), nest(4, &rule_exprs)],
    ));
    batch.extend(begin(0x11));
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
        while out.len() < n {
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

    /// Send the NAT batch. Returns Ok(true) if applied, Ok(false) if it was already there.
    pub fn nft_batch(&self, batch: &[u8]) -> Result<bool> {
        self.send(batch)?;
        let acks = self.acks(3);
        match acks.first() {
            Some(&e) if -e == libc::EEXIST => Ok(false),
            _ if acks.len() == 3 && acks.iter().all(|&e| e == 0) => Ok(true),
            _ => {
                let bad = acks.iter().copied().find(|&e| e != 0).unwrap_or(-libc::ETIMEDOUT);
                Err(Error::io("nftables NAT rule", io::Error::from_raw_os_error(-bad)))
            }
        }
    }
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
        let b = masquerade_batch("t", "c", [172, 30, 0, 0], 16);
        let mut off = 0;
        let mut types = vec![];
        while off < b.len() {
            let len = u32::from_ne_bytes(b[off..off + 4].try_into().unwrap()) as usize;
            types.push(u16::from_ne_bytes(b[off + 4..off + 6].try_into().unwrap()));
            assert!(len >= 16 && len % 4 == 0);
            off += len;
        }
        assert_eq!(off, b.len());
        let nft = |t: u16| (NFNL_SUBSYS_NFTABLES << 8) | t;
        assert_eq!(types, [0x10, nft(0), nft(3), nft(6), 0x11]);
        assert!(b.windows(3).any(|w| w == b"nat")); // chain type
        assert!(b.windows(4).any(|w| w == [0xff, 0xff, 0x00, 0x00])); // /16 mask
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
