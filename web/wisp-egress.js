// WISP v1 egress: gives fake_network.js somewhere to send TCP.
//
// fake_network terminates the guest's TCP connections itself and hands each one
// to `on_tcp_connection`. Without an implementation of that hook a SYN is
// answered with RST — correct for a router with nothing behind it, and useless
// for `apk`. This bridges each terminated connection to a WISP stream, where
// the relay opens the real socket.
//
// Protocol shape follows v86's wisp_network.js (BSD-2-Clause), which is the
// reference v1 client; this is a focused reimplementation rather than a vendor
// because that file is welded to v86's event bus and only ~30 lines of it are
// about WISP. Frames are [type:u8][stream:u32 LE][payload]:
//   0x01 CONNECT  → [0x01 TCP][port:u16 LE][hostname]
//   0x02 DATA     ↔ bytes
//   0x03 CONTINUE ← credit:u32 LE   (stream 0 = the initial window)
//   0x04 CLOSE    ↔ reason:u8
//
// Flow control is the part that bites: the relay opens the window with a
// CONTINUE on stream 0, and until it arrives we must queue rather than send.

const CONNECT = 0x01, DATA = 0x02, CONTINUE = 0x03, CLOSE = 0x04;

// Private extension. ICMP is not a stream, so it does not belong in the stream
// protocol; it rides this socket because the socket is already open and already
// authenticated, and because the relay is the only party here that can emit a
// real echo request.
const PING = 0xf1;   // c->s  id in the stream field, payload [dst:4][timeout:u16]
const PONG = 0xf2;   // s->c  id in the stream field, payload [status:u8]

/// Relay-side deadline, and our own backstop for a reply that never lands.
const PING_TIMEOUT_MS = 4000;
const PING_TTL_MS = 8000;

export class WispEgress {
    /** @param {string} url ws:// or wss:// endpoint of the relay */
    constructor(url, { onStatus = () => {} } = {}) {
        this.url = url;
        this.onStatus = onStatus;
        this.nextStream = 1;
        // Echo requests out at the relay: id -> the guest's own frame.
        this.pings = new Map();
        this.nextPing = 1;
        this.streams = new Map(); // id -> {onData, onClose}
        this.credit = 0;          // stream-0 window; 0 = not open yet
        this.queue = [];
        this.ws = null;
        this.open = false;
        this.connect();
    }

    connect() {
        this.onStatus(`wisp: connecting to ${this.url}`);
        const ws = new WebSocket(this.url);
        ws.binaryType = "arraybuffer";
        this.ws = ws;

        ws.onopen = () => { this.open = true; this.onStatus("wisp: connected"); };
        ws.onerror = () => this.onStatus("wisp: error");
        ws.onclose = () => {
            this.open = false;
            this.credit = 0;
            // Fail every live stream: the guest's TCP should see a reset now,
            // not hang until its own timeout.
            for (const s of this.streams.values()) s.onClose?.(0x03);
            this.streams.clear();
            this.onStatus("wisp: disconnected");
        };
        ws.onmessage = e => this.onFrame(new Uint8Array(e.data));
    }

    onFrame(f) {
        if (f.length < 5) return;
        const view = new DataView(f.buffer, f.byteOffset, f.byteLength);
        const type = f[0];
        const id = view.getUint32(1, true);
        switch (type) {
            case DATA:
                this.streams.get(id)?.onData(f.slice(5));
                break;
            case CONTINUE: {
                const credit = view.getUint32(5, true);
                if (id === 0) {
                    // The window just opened; release anything we buffered.
                    this.credit = credit;
                    const q = this.queue;
                    this.queue = [];
                    for (const b of q) this.ws.send(b);
                } else {
                    this.credit = Math.max(this.credit, credit);
                }
                break;
            }
            case PONG: {
                const p = this.pings.get(id);
                if (!p) break; // already given up on, or never ours
                this.pings.delete(id);
                clearTimeout(p.timer);
                // Only an actual reply produces a reply. Anything else means
                // unreachable, and the guest should be told so by silence.
                if (f[5] === 0) p.onReply();
                break;
            }
            case CLOSE: {
                const s = this.streams.get(id);
                this.streams.delete(id);
                s?.onClose?.(f[5]);
                break;
            }
            default:
                break; // PROTOEXT (v2) and anything else: we are a v1 client
        }
    }

    /**
     * Ask the relay to ping the destination of `frame`, and call `onReply` if
     * it answers.
     */
    sendEcho(frame, onReply) {
        if (!this.open) return; // no relay: the guest times out, correctly

        const id = this.nextPing++ >>> 0;
        const payload = new Uint8Array(6);
        payload.set(frame.subarray(30, 34), 0); // IPv4 destination
        new DataView(payload.buffer).setUint16(4, PING_TIMEOUT_MS, true);

        this.pings.set(id, {
            onReply,
            timer: setTimeout(() => this.pings.delete(id), PING_TTL_MS),
        });
        this.send(this.frame(PING, id, payload));

        if (globalThis.RISCV_NET_DEBUG) {
            console.debug(`[wisp] ping ${id} -> ${Array.from(frame.subarray(30, 34)).join(".")}`);
        }
    }

    send(buf) {
        if (!this.open) return;              // dropped; the stream will fail
        if (this.credit === 0) this.queue.push(buf);
        else this.ws.send(buf);
    }

    frame(type, id, payload = new Uint8Array(0)) {
        const b = new Uint8Array(5 + payload.length);
        new DataView(b.buffer).setUint32(1, id, true);
        b[0] = type;
        b.set(payload, 5);
        return b;
    }

    /**
     * fake_network's hook. `conn` is its TCPConnection; `packet` is the SYN,
     * whose IPv4 destination is where the guest thinks it is going.
     */
    onTcpConnection(conn, packet) {
        if (!this.open) return false; // no relay: let fake_network RST it

        const id = this.nextStream++;
        const ip = packet.ipv4.dest.join(".");
        // Prefer the name the guest looked up: the relay allowlists hostnames
        // (a list of CDN IPs is unmaintainable) and TLS needs it for SNI.
        const host = this.resolveName?.(ip) || ip;
        const port = conn.sport;

        this.streams.set(id, {
            onData: data => conn.write(data),
            onClose: () => conn.close(),
        });

        const name = new TextEncoder().encode(host);
        const payload = new Uint8Array(3 + name.length);
        payload[0] = 0x01; // TCP
        new DataView(payload.buffer).setUint16(1, port, true);
        payload.set(name, 3);
        this.send(this.frame(CONNECT, id, payload));

        if (globalThis.RISCV_NET_DEBUG) {
            console.debug(`[wisp] CONNECT ${id} -> ${host}:${port}`);
        }
        conn.on("data", data => {
            if (globalThis.RISCV_NET_DEBUG) {
                console.debug(`[wisp] guest->relay ${id}: ${data.length} B`);
            }
            if (data.length) this.send(this.frame(DATA, id, data));
        });
        // WISP has no half-close, so a shutdown has to be a full close.
        conn.on_close = conn.on_shutdown = () => {
            if (this.streams.delete(id)) {
                this.send(this.frame(CLOSE, id, new Uint8Array([0x02])));
            }
        };

        // If this throws, fake_network never emits the SYN-ACK, the guest waits
        // out its timeout, and the relay reports a stream that carried zero
        // bytes — which is exactly the production symptom. An exception here
        // used to vanish silently into the caller.
        try {
            conn.accept();
        } catch (e) {
            console.error("[wisp] conn.accept() threw:", e);
            this.streams.delete(id);
            this.send(this.frame(CLOSE, id, new Uint8Array([0x03])));
            return false;
        }
        if (globalThis.RISCV_NET_DEBUG) console.debug(`[wisp] accepted ${id}`);
        return true;
    }
}
