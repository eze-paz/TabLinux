// The echo reply is built by mutating the guest's own request, which is only
// safe if the checksums survive it. Both are one's-complement sums, and the
// argument for each is a paper argument -- so check it against a real
// recomputation rather than trusting the reasoning.

import { RiscvNetAdapter } from "./web/net-adapter.js";

function sum16(b, from, to) {
    let s = 0;
    for (let i = from; i < to; i += 2) s += (b[i] << 8) | (b[i + 1] | 0);
    while (s >> 16) s = (s & 0xffff) + (s >>> 16);
    return (~s) & 0xffff;
}

/** A complete Ethernet/IPv4/ICMP echo request with valid checksums. */
function echoRequest(dst, id, seq, payloadLen) {
    const f = new Uint8Array(14 + 20 + 8 + payloadLen);
    f.set([0x52, 0x54, 0, 0, 0, 1], 0);        // dst mac (gateway)
    f.set([0x52, 0x54, 0, 0, 0, 2], 6);        // src mac (guest)
    f[12] = 0x08; f[13] = 0x00;                // IPv4

    f[14] = 0x45; f[15] = 0;
    const total = 20 + 8 + payloadLen;
    f[16] = total >> 8; f[17] = total & 0xff;
    f[18] = 0x12; f[19] = 0x34;                // ident
    f[22] = 64; f[23] = 1;                     // ttl, ICMP
    f.set([10, 0, 2, 15], 26);                 // src
    f.set(dst, 30);
    const ick = sum16(f, 14, 34);
    f[24] = ick >> 8; f[25] = ick & 0xff;

    const o = 34;
    f[o] = 8; f[o + 1] = 0;                    // echo request
    f[o + 4] = id >> 8; f[o + 5] = id & 0xff;
    f[o + 6] = seq >> 8; f[o + 7] = seq & 0xff;
    for (let i = 0; i < payloadLen; i++) f[o + 8 + i] = (i * 7 + 3) & 0xff;
    const ck = sum16(f, o, f.length);
    f[o + 2] = ck >> 8; f[o + 3] = ck & 0xff;
    return f;
}

let fails = 0;
const check = (name, ok) => { console.log(`${ok ? "ok  " : "FAIL"} ${name}`); if (!ok) fails++; };

// Odd and even payload lengths, and a sequence that carries into the high byte,
// because that is where an end-around carry would go wrong.
for (const [len, seq] of [[56, 1], [56, 0x1ff], [0, 1], [17, 300], [1472, 7]]) {
    const req = echoRequest([8, 8, 8, 8], 0xabcd, seq, len);
    check(`request len=${len} seq=${seq}: own checksums valid`,
        sum16(req, 14, 34) === 0 && sum16(req, 34, req.length) === 0);

    let got = null;
    const net = new RiscvNetAdapter(f => { got = f; });
    net.deliverEchoReply(req);

    const o = 34;
    check(`  reply is an echo reply`, got && got[o] === 0);
    check(`  ICMP checksum recomputes clean`, sum16(got, o, got.length) === 0);
    check(`  IP header checksum still valid`, sum16(got, 14, 34) === 0);
    check(`  addresses swapped`,
        [...got.subarray(26, 30)].join() === "8,8,8,8" &&
        [...got.subarray(30, 34)].join() === "10,0,2,15");
    check(`  MACs swapped`,
        got[0] === 0x52 && got[5] === 2 && got[11] === 1);
    check(`  id/seq/payload preserved`,
        got[o + 4] === 0xab && got[o + 5] === 0xcd &&
        ((got[o + 6] << 8) | got[o + 7]) === seq &&
        [...got.subarray(o + 8)].join() === [...req.subarray(o + 8)].join());
    check(`  request not mutated`, req[o] === 8);
}

// The drop-vs-forward decision: only foreign echo *requests* leave.
const net = new RiscvNetAdapter(() => {});
const req = echoRequest([8, 8, 8, 8], 1, 1, 56);
check("foreign echo request is routed out", net.isForeignPing(req));
const local = echoRequest(net.router_ip, 1, 1, 56);
check("ping to our own router stays local", !net.isForeignPing(local));
const reply = req.slice(); reply[34] = 0;
check("an echo *reply* is not routed out", !net.isForeignPing(reply));

console.log(fails ? `\n${fails} FAILED` : "\nall passed");
process.exit(fails ? 1 : 0);
