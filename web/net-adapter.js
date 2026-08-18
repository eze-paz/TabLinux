// The shim between the emulator's frame queues and v86's fake_network.js.
//
// fake_network.js was written against a v86 adapter object (FetchNetworkAdapter
// / WispNetworkAdapter) wired to v86's event bus. This class is that adapter
// with the bus removed: frames the guest transmitted are handed to `send()`,
// and everything fake_network decides to answer comes back out through
// `receive()`, which forwards it to the worker for `Vm::net_inject`.
//
// What that buys, with zero code of ours: ARP, DHCP (the guest can `udhcpc`
// instead of static config), ICMP echo, NTP, DNS-over-HTTPS via plain fetch,
// and a TCP state machine that terminates connections host-side. TCP is only
// *useful* once something answers `on_tcp_connection` — the WISP egress
// adapter is the next step and slots into exactly that hook.

import {
    create_eth_encoder_buf,
    handle_fake_networking,
} from "./vendor/v86/browser/fake_network.js";

const parse_mac = s => new Uint8Array(s.split(":").map(x => parseInt(x, 16)));
const parse_ip = s => new Uint8Array(s.split(".").map(x => parseInt(x, 10)));

export class RiscvNetAdapter {
    /**
     * `inject(Uint8Array)` delivers a frame to the guest — here, a postMessage
     * to the VM worker. `config` mirrors v86's FetchNetworkAdapter fields.
     */
    constructor(inject, config = {}) {
        this.inject = inject;

        this.router_mac = parse_mac(config.router_mac || "52:54:00:01:02:03");
        this.router_ip = parse_ip(config.router_ip || "192.168.86.1");
        this.vm_ip = parse_ip(config.vm_ip || "192.168.86.100");
        this.vm_mac = parse_mac(config.vm_mac || "52:54:00:12:34:56");
        this.masquerade = config.masquerade === undefined || !!config.masquerade;
        this.dns_method = config.dns_method || "doh";
        this.doh_server = config.doh_server;
        this.mtu = config.mtu;
        this.tcp_conn = {};
        this.eth_encoder_buf = create_eth_encoder_buf(this.mtu);
        // fetch_network's HTTP fallback reads this off the adapter.
        this.fetch = (...args) => fetch(...args);

        // fake_network announces inbound TCP on `bus.pair.send`; without a
        // WISP relay nobody is listening, so a silent sink is the honest stub.
        this.bus = { pair: { send() {} }, register() {}, send() {} };

        // Set this to open egress (see wisp_network.js); unset, unanswered
        // SYNs are refused by fake_network's own RST path, which is correct
        // behavior for a router with nothing behind it.
        this.on_tcp_connection = config.on_tcp_connection;

        this.stats = { tx: 0, rx: 0, icmpDropped: 0, icmpSent: 0, icmpReplies: 0 };
        /** ip -> hostname, learned from DNS answers (see snoopDns). */
        this.dnsNames = new Map();
    }

    /**
     * Is this an ICMP echo request to something other than our own router?
     *
     * fake_network answers echo requests for ANY destination, so `ping
     * google.com` reports replies that were manufactured in this page and
     * never touched the network. That is worse than a failure: it makes a
     * broken egress path look healthy, and it is indistinguishable on screen
     * from a real reply. Egress here is WISP, which carries TCP only, so an
     * ICMP echo to a real host has no path and must be seen not to work.
     *
     * Pings to the router address are still answered — that IS a local device,
     * and confirming the link works is legitimately useful.
     */
    isForeignPing(f) {
        if (f.length < 38 || ((f[12] << 8) | f[13]) !== 0x0800) return false;
        const ihl = (f[14] & 0x0f) * 4;
        if (f[23] !== 1) return false;                 // not ICMP
        if (f[14 + ihl] !== 8) return false;           // not an echo request
        const dst = f.subarray(30, 34);
        return !dst.every((b, i) => b === this.router_ip[i]);
    }

    /** A frame the guest transmitted. */
    send(frame) {
        this.stats.tx++;
        if (this.isForeignPing(frame)) {
            // Off to the relay, which can actually emit ICMP. Still no local
            // answer: if it never comes back, the guest sees the loss.
            if (this.on_icmp_echo) {
                this.stats.icmpSent++;
                this.on_icmp_echo(frame);
            } else {
                this.stats.icmpDropped++;
            }
            return;
        }
        handle_fake_networking(frame, this);
    }

    /**
     * Turn a guest echo request into its reply and hand it back.
     *
     * Built from the request rather than assembled fresh, so the identifier,
     * sequence and payload are the guest's own -- ping matches on those, and
     * inventing them is how you end up with a reply that looks right and
     * corresponds to nothing.
     */
    deliverEchoReply(request) {
        const f = request.slice();

        // Ethernet: the reply comes back from the gateway to the guest.
        for (let i = 0; i < 6; i++) {
            const t = f[i];
            f[i] = f[6 + i];
            f[6 + i] = t;
        }
        // IPv4 source and destination. The header checksum stays valid: it is a
        // sum over the header, and swapping two of its words does not change a
        // sum.
        for (let i = 0; i < 4; i++) {
            const t = f[26 + i];
            f[26 + i] = f[30 + i];
            f[30 + i] = t;
        }

        const o = 14 + (f[14] & 0x0f) * 4;
        f[o] = 0; // echo request (8) -> echo reply (0)

        // The type is the high byte of the first checksummed word, so dropping
        // it from 8 to 0 lowers the sum by 0x0800 and raises the one's
        // complement checksum by the same, with an end-around carry.
        let ck = (f[o + 2] << 8) | f[o + 3];
        ck += 0x0800;
        if (ck > 0xffff) ck = (ck & 0xffff) + 1;
        f[o + 2] = ck >> 8;
        f[o + 3] = ck & 0xff;

        this.stats.icmpReplies++;
        this.receive(f);
    }

    /**
     * The hostname the guest resolved to `ip`, if we saw the lookup.
     *
     * The TCP hook only knows a destination IP — the guest resolved the name
     * itself and the SYN carries no trace of it. But every DNS answer
     * fake_network sends passes through `receive()`, so snooping them here
     * recovers the mapping without forking the vendored file. The relay needs
     * it because an allowlist of CDN IPs is unmaintainable, and TLS needs it
     * for SNI.
     */
    hostnameFor(ip) {
        return this.dnsNames.get(ip);
    }

    /** Record A records out of a DNS reply the guest is about to receive. */
    snoopDns(f) {
        // Ethernet(14) + IPv4 + UDP, source port 53.
        if (f.length < 42 || ((f[12] << 8) | f[13]) !== 0x0800) return;
        const ihl = (f[14] & 0x0f) * 4;
        if (f[23] !== 17) return; // not UDP
        const udp = 14 + ihl;
        if (((f[udp] << 8) | f[udp + 1]) !== 53) return; // not from our resolver
        const dns = udp + 8;
        const v = new DataView(f.buffer, f.byteOffset, f.byteLength);
        const qd = v.getUint16(dns + 4), an = v.getUint16(dns + 6);
        if (!an) return;

        // Question section: labels until a zero byte, then qtype+qclass.
        let p = dns + 12;
        const labels = [];
        for (let q = 0; q < qd; q++) {
            const parts = [];
            while (p < f.length && f[p] !== 0) {
                if ((f[p] & 0xc0) === 0xc0) { p += 2; break; }   // compressed
                const n = f[p++];
                parts.push(new TextDecoder().decode(f.subarray(p, p + n)));
                p += n;
            }
            if (f[p] === 0) p++;
            p += 4;
            labels.push(parts.join("."));
        }
        const qname = labels[0];
        if (!qname) return;

        for (let a = 0; a < an && p + 12 <= f.length; a++) {
            if ((f[p] & 0xc0) === 0xc0) p += 2;
            else { while (p < f.length && f[p] !== 0) p += f[p] + 1; p++; }
            const type = v.getUint16(p), rdlen = v.getUint16(p + 8);
            p += 10;
            if (type === 1 && rdlen === 4) {
                this.dnsNames.set(Array.from(f.subarray(p, p + 4)).join("."), qname);
            }
            p += rdlen;
        }
    }

    /** fake_network's answers land here on their way into the guest. */
    receive(frame) {
        this.stats.rx++;
        if (globalThis.RISCV_NET_DEBUG) {
            const et = frame.length >= 14 ? ((frame[12] << 8) | frame[13]) : 0;
            if (et === 0x0800 && frame[23] === 6) {
                const ihl = (frame[14] & 0x0f) * 4;
                const fl = frame[14 + ihl + 13];
                console.debug(`[net] ->guest TCP flags=${fl.toString(2).padStart(6, "0")}` +
                    ` (SYN=${!!(fl & 2)} ACK=${!!(fl & 16)} RST=${!!(fl & 4)}) len=${frame.length}`);
            }
        }
        try { this.snoopDns(frame); } catch { /* never break the data path */ }
        // Always copy. fake_network builds every reply inside the one shared
        // `eth_encoder_buf` and hands out a subarray of it; if the caller
        // transfers that buffer to a worker, the encoder is detached and every
        // later reply becomes a silent zero-length no-op. Exactly one frame
        // ever arrives — the ARP reply — and every ping after it vanishes.
        const copy = new Uint8Array(frame.byteLength);
        copy.set(frame);
        this.inject(copy);
    }
}
