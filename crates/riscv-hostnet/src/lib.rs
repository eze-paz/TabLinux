//! A deliberately tiny host network: enough ARP and ICMP to prove the receive
//! path works against a real kernel, and nothing more.
//!
//! This is NOT the thing that will make `apk` work. That needs TCP terminated
//! host-side and forwarded over WISP, and v86 already has both halves in
//! JavaScript (`fake_network.js`, `wisp_network.js`) — see HANDOFF.md. What
//! this does is close the loop natively: `ping` from the guest gets a reply,
//! which exercises virtio-net RX end to end with Linux driving it, something
//! the unit tests cannot do.
//!
//! Its own crate rather than a module of `riscv-vm`, because that front end is
//! a binary and a test cannot link one. While it lived there, exercising the
//! network against a real kernel meant driving a terminal for eight minutes a
//! run; `riscv-harness/tests/net_roundtrip.rs` now does it in about one.

#![cfg_attr(not(test), no_std)]
extern crate alloc;
use alloc::vec::Vec;

/// The address this fake host answers to, and the MAC it answers with.
pub const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
pub const GATEWAY_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x01];

const ETH_ARP: u16 = 0x0806;
const ETH_IPV4: u16 = 0x0800;

fn be16(b: &[u8], o: usize) -> u16 {
    ((b[o] as u16) << 8) | b[o + 1] as u16
}

/// One's-complement sum, as used by IP and ICMP.
fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += be16(data, i) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Given a frame from the guest, produce a reply if we know how to answer it.
pub fn respond(frame: &[u8]) -> Option<Vec<u8>> {
    if frame.len() < 14 {
        return None;
    }
    let src_mac = &frame[6..12];
    match be16(frame, 12) {
        ETH_ARP => arp_reply(frame, src_mac),
        ETH_IPV4 => icmp_reply(frame, src_mac),
        _ => None, // IPv6 solicitations and everything else: silently ignored
    }
}

/// Answer "who has GATEWAY_IP" with GATEWAY_MAC.
fn arp_reply(frame: &[u8], src_mac: &[u8]) -> Option<Vec<u8>> {
    if frame.len() < 42 {
        return None;
    }
    let arp = &frame[14..42];
    // opcode 1 = request, and the target protocol address must be ours
    if be16(arp, 6) != 1 || arp[24..28] != GATEWAY_IP {
        return None;
    }
    let sender_ip = &arp[14..18];

    let mut out = Vec::with_capacity(42);
    out.extend_from_slice(src_mac); // dst
    out.extend_from_slice(&GATEWAY_MAC); // src
    out.extend_from_slice(&ETH_ARP.to_be_bytes());
    out.extend_from_slice(&[0, 1]); // ethernet
    out.extend_from_slice(&[0x08, 0x00]); // ipv4
    out.push(6); // hlen
    out.push(4); // plen
    out.extend_from_slice(&[0, 2]); // opcode 2 = reply
    out.extend_from_slice(&GATEWAY_MAC);
    out.extend_from_slice(&GATEWAY_IP);
    out.extend_from_slice(&frame[6..12]); // target mac = requester
    out.extend_from_slice(sender_ip);
    Some(out)
}

/// Answer an ICMP echo request addressed to GATEWAY_IP.
fn icmp_reply(frame: &[u8], src_mac: &[u8]) -> Option<Vec<u8>> {
    let ip = frame.get(14..)?;
    if ip.len() < 20 || ip[0] >> 4 != 4 {
        return None;
    }
    let ihl = (ip[0] & 0x0F) as usize * 4;
    if ip[9] != 1 || ip.len() < ihl + 8 {
        return None; // not ICMP
    }
    if ip[16..20] != GATEWAY_IP {
        return None; // not for us
    }
    let icmp = &ip[ihl..];
    if icmp[0] != 8 {
        return None; // not an echo request
    }
    let src_ip = &ip[12..16];
    let payload = &icmp[8..];

    // ICMP echo reply: type 0, same id/seq/payload, checksum recomputed.
    let mut icmp_out = Vec::with_capacity(icmp.len());
    icmp_out.extend_from_slice(&[0, 0, 0, 0]);
    icmp_out.extend_from_slice(&icmp[4..8]); // id + seq
    icmp_out.extend_from_slice(payload);
    let ck = checksum(&icmp_out);
    icmp_out[2..4].copy_from_slice(&ck.to_be_bytes());

    let total_len = (20 + icmp_out.len()) as u16;
    let mut iph = Vec::with_capacity(20);
    iph.extend_from_slice(&[0x45, 0x00]);
    iph.extend_from_slice(&total_len.to_be_bytes());
    iph.extend_from_slice(&[0, 0, 0x40, 0x00]); // id 0, don't fragment
    iph.push(64); // ttl
    iph.push(1); // icmp
    iph.extend_from_slice(&[0, 0]); // checksum placeholder
    iph.extend_from_slice(&GATEWAY_IP);
    iph.extend_from_slice(src_ip);
    let ck = checksum(&iph);
    iph[10..12].copy_from_slice(&ck.to_be_bytes());

    let mut out = Vec::with_capacity(14 + iph.len() + icmp_out.len());
    out.extend_from_slice(src_mac);
    out.extend_from_slice(&GATEWAY_MAC);
    out.extend_from_slice(&ETH_IPV4.to_be_bytes());
    out.extend_from_slice(&iph);
    out.extend_from_slice(&icmp_out);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const GUEST_IP: [u8; 4] = [10, 0, 2, 15];

    fn arp_request(target: [u8; 4]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&[0xFF; 6]);
        f.extend_from_slice(&GUEST_MAC);
        f.extend_from_slice(&ETH_ARP.to_be_bytes());
        f.extend_from_slice(&[0, 1, 0x08, 0x00, 6, 4, 0, 1]);
        f.extend_from_slice(&GUEST_MAC);
        f.extend_from_slice(&GUEST_IP);
        f.extend_from_slice(&[0; 6]);
        f.extend_from_slice(&target);
        f
    }

    #[test]
    fn answers_arp_for_the_gateway() {
        let r = respond(&arp_request(GATEWAY_IP)).expect("should reply");
        assert_eq!(&r[0..6], &GUEST_MAC, "reply is unicast back to the asker");
        assert_eq!(&r[6..12], &GATEWAY_MAC);
        assert_eq!(be16(&r, 12), ETH_ARP);
        assert_eq!(be16(&r, 20), 2, "opcode 2 = reply");
        assert_eq!(&r[22..28], &GATEWAY_MAC, "sender hardware address");
        assert_eq!(&r[28..32], &GATEWAY_IP, "sender protocol address");
    }

    #[test]
    fn ignores_arp_for_anyone_else() {
        assert!(respond(&arp_request([10, 0, 2, 99])).is_none());
    }

    #[test]
    fn echo_reply_has_valid_checksums() {
        // Minimal ICMP echo request to the gateway.
        let payload = [0xAAu8; 8];
        let mut icmp = vec![8u8, 0, 0, 0, 0x12, 0x34, 0x00, 0x01];
        icmp.extend_from_slice(&payload);
        let ck = checksum(&icmp);
        icmp[2..4].copy_from_slice(&ck.to_be_bytes());

        let total = (20 + icmp.len()) as u16;
        let mut ip = vec![0x45, 0x00];
        ip.extend_from_slice(&total.to_be_bytes());
        ip.extend_from_slice(&[0, 0, 0x40, 0x00, 64, 1, 0, 0]);
        ip.extend_from_slice(&GUEST_IP);
        ip.extend_from_slice(&GATEWAY_IP);

        let mut f = Vec::new();
        f.extend_from_slice(&GATEWAY_MAC);
        f.extend_from_slice(&GUEST_MAC);
        f.extend_from_slice(&ETH_IPV4.to_be_bytes());
        f.extend_from_slice(&ip);
        f.extend_from_slice(&icmp);

        let r = respond(&f).expect("should reply");
        let rip = &r[14..34];
        assert_eq!(checksum(rip), 0, "IP header checksum must verify to zero");
        assert_eq!(checksum(&r[34..]), 0, "ICMP checksum must verify to zero");
        assert_eq!(r[34], 0, "type 0 = echo reply");
        assert_eq!(&r[34 + 4..34 + 8], &icmp[4..8], "id and seq echoed back");
        assert_eq!(&r[34 + 8..], &payload, "payload echoed back");
        assert_eq!(&rip[12..16], &GATEWAY_IP, "source is the gateway");
        assert_eq!(&rip[16..20], &GUEST_IP, "destination is the asker");
    }
}
