# Networking: where it stands, and how WISP attaches

## Done

`virtio-net` works. The guest gets `eth0`, adopts the MAC from config space,
and both directions are proven against the real kernel with sub-millisecond
round trips:

```
64 bytes from 10.0.2.2: seq=0 ttl=64 time=0.595 ms
```

The scripted proof is `riscv-harness/tests/net_roundtrip.rs` (~2 min):

```bash
cargo test --release -p riscv-harness --test net_roundtrip -- --nocapture
```

Interactively:

```bash
cargo run --release -p riscv-vm -- --quiet --net --net-verbose
# then inside the guest:
ip link set eth0 up; ip addr add 10.0.2.15/24 dev eth0; ping -c 1 10.0.2.2
```

### Timing, measured rather than guessed

An earlier version of this file claimed emulated time runs ~45× slower than
real, so a one-second ping interval costs ~45 s of wall clock. That is wrong,
and it is worth recording why, because the two halves of it fail differently.

Measured with the `IDLE-BEGIN`/`IDLE-END` brackets in
`boot_to_userspace` (`cargo test --release -p riscv-harness --test
boot_to_userspace -- --nocapture`):

```
[idle] guest `sleep 1` cost 1681399 host instructions in 0.10s wall
[idle] emulated time advanced 1.016s (0.999s skipped by fast-forward)
[idle] 10 WFIs, 10 advanced time, 0 advanced nothing
```

**Idle is cheap and always was.** A wholly idle guest second costs 0.10 s of
wall clock, because the hart parks in WFI and the clock is skipped to the next
deadline. Sleeping is ~10× *faster* than real time, not slower.

**Busy execution is ~8× slower than real time.** Userspace is reached at
567.6M instructions in ~45 s, so the interpreter runs at ~12.6 MIPS.
`MTIME_STEPS_PER_TICK = 10` against the devicetree's 10 MHz timebase makes one
emulated second cost 100M instructions, hence ~8×.

Neither number is 45×. What the original note was really reacting to is a
separate defect, below.

### Solved: the RX refill storm (was "round-trip latency is inflated")

`ping` used to report RTTs of 270–1450 ms in *emulated* milliseconds. It now
reports **0.595 ms**, measured by `riscv-harness/tests/net_roundtrip.rs`.

The bug was in the virtio-mmio transport, not in any clock policy.
`VirtioMmio::notify` drained every chain the driver had made available on
*any* queue — but buffers posted on a net device's RX queue are the driver
**offering space**, not submitting work. Each kick of the RX queue therefore
retired every offered buffer as a zero-length receive. The driver dropped the
runts, refilled the ring, kicked again, and the guest disappeared into an
infinite refill storm at 100% CPU inside virtio_net module code —
`ip link set eth0 up` alone burned billions of instructions.

The storm also explains every RTT measurement: genuine deliveries went through
`poll`, which could only grab a buffer in the window between the driver's
`virtqueue_add` and the kick that would instantly devour it. Replies landed
seconds late when they landed at all — and no clock policy could change that,
which is why all four earlier idle-skip variants clustered around 1–2 s.

The fix (`virtio.rs::notify`): a kick on the device's RX queue means "space
just appeared" and delivers a pending frame via the take/fill/complete path;
it never executes the offered chains. `test_virtio_net.rs` pins both halves —
`rx_kick_must_not_eat_the_offered_buffers` and
`rx_kick_delivers_a_frame_that_was_waiting` — and both were verified to fail
against the pre-fix transport. virtio-blk is untouched (`rx_queue()` is
`None`; on a blk queue every chain *is* a request).

How it was found, for the next bug of this shape: the per-100M-step,
per-function PC histogram in `net_roundtrip.rs`. A console that stops while
`mtime` advances at exactly the busy rate, with samples pooling past the last
System.map symbol (`__pecoff_data_virt_size+…` = module code), is this failure
mode. Aggregate by function, not by symbol+offset — offsets split one hot loop
into a dozen entries and made 20% look like 0.5% on the first read.

Three clock-policy mechanisms were tested against the symptom before the real
cause surfaced, and all measured no-better-or-worse; the eliminations are kept
here because they rule the idle path out for good:

1. The idle skip jumping past a frame already queued for the guest. Declining
   to skip while `to_guest` is non-empty moved a 992 ms sample to ~550 ms —
   within the noise of a 3-packet sample.
2. The size of a single skip. Capping each jump at 1 ms did not bound the
   latency at 1 ms, which refutes it outright.
3. Device deadlines counted in retired instructions (RX polled every 512, a
   completion retired 2000 later) being stretched because idle skipping lets
   one instruction carry up to 1 ms. Crawling the clock while I/O is in flight
   did not help either.

`RISCV_NET_TIME=1` traces the emulated timeline end to end — the guest's queue
kick, the frame reaching `to_host`, the reply landing in the guest's RX buffer,
and the completion that raises the IRQ. For one ping:

```
[net t] tx visible  at 88519.778 ms   guest's ICMP request reaches the host
[dev t] rx->guest   at 88520.080 ms   reply placed in the guest's RX buffer
64 bytes from 10.0.2.2: seq=0 ttl=64 time=1448.313 ms
```

**The host round trip costs 0.30 ms of emulated time.** The ARP exchange
immediately before it takes 0.26 ms. Queue kicks are handled synchronously
inside the MMIO write (`VirtioMmio::notify` calls `dev.handle` before
returning), and kick-to-completion measures 20 µs.

So there is no transmit-side latency bug, and an earlier version of this file
saying there was one is wrong. The frame goes out fast and the reply comes back
fast. What is inflated is the guest's *measurement* of the interval: 0.30 ms of
delivery reported as 1448 ms.

That put the remaining question between `rx->guest` and the guest's
`recvmsg` — how and when the guest is told the packet arrived. Two clock-side
fixes were tried and both made it worse:

| variant | RTT |
|---|---|
| skip capped at 1 ms | 343 ms avg over 3 |
| clock crawls (10 µs) while I/O in flight | 1448 ms |
| clock stopped dead while I/O in flight | 2277 ms |

The suspect this pointed at (a missed virtio RX interrupt / NAPI-poll-only
wakeup) was close but not exact: the guest wasn't waiting *idle* for a lost
interrupt, it was *busy* in the RX refill storm described in the section
above, and delivery itself was the thing being starved. The storm fix took the
RTT to 0.595 ms.

The changes kept from the clock investigation are the ones defensible on their
own terms: `idle_skip_mtime` never skips past an already-queued frame (bounded
so an undeliverable frame cannot stall the clock forever), and bounds any
single jump. Both are measurement-neutral on the boot path (identical step
count to userspace, idle unchanged at 0.10 s).

## The seam

`crates/riscv-devices/src/virtio_net.rs` moves frames through `NetBackend`,
with a `SharedNet` pair of queues underneath:

```rust
pub struct NetQueues {
    pub to_host: VecDeque<Vec<u8>>,   // guest transmitted these
    pub to_guest: VecDeque<Vec<u8>>,  // hand these to the guest
}
```

`DeviceBus::attach_virtio_net(mac)` returns the hwirq and an `Rc<RefCell<..>>`
handle. Two queues rather than a callback, because the host side is
asynchronous in every real deployment — natively a loop drains them, in a
browser JS polls them from the event loop.

Today `crates/riscv-vm/src/hostnet.rs` sits on that seam and answers ARP and
ICMP echo for one address. That is a proof of the receive path, not a network.

## What still has to happen for `apk`

`apk` needs TCP to a real mirror. A browser cannot emit raw packets, so TCP has
to be **terminated host-side** and the payload forwarded over something the
browser can speak. That is two pieces, and v86 already has both in JavaScript
under BSD-2-Clause:

| file | lines | what it does |
|---|---|---|
| `Desktop/v86-master/v86-master/src/browser/fake_network.js` | 1480 | ARP, DHCP, ICMP, NTP, DNS-over-HTTPS, and a full `TCPConnection` state machine |
| `Desktop/v86-master/v86-master/src/browser/wisp_network.js` | 251 | WISP — multiplexed TCP-over-WebSocket — for egress |

**Do not port these to Rust.** The host side of the emulator is JavaScript in
the browser build anyway, the logic is already debugged, and reimplementing a
TCP state machine to no benefit is how weeks disappear.

### Wiring

0. ~~**Point v86's adapter at the queues.**~~ **Done — see `web/`.**
   `web/vendor/v86/browser/fake_network.js` is v86's file verbatim
   (BSD-2-Clause, license and stub notes in `web/vendor/v86/README.md`);
   `web/net-adapter.js` is the shim — the v86 adapter object with the event
   bus removed, `send()` fed from `Vm::net_take` and `receive()` forwarded to
   `Vm::net_inject`. The demo page:

   ```bash
   cargo build --release --target wasm32-unknown-unknown -p riscv-wasm
   wasm-bindgen --target web --out-dir web/pkg \
       target/wasm32-unknown-unknown/release/riscv_wasm.wasm
   python3 -m http.server 8139   # from the repo root
   # open http://localhost:8139/web/
   ```

   The VM runs in a module Worker (`web/vm-worker.js`); the adapter stays on
   the main thread so fake_network's `fetch()`-based DNS never blocks the run
   loop. What the guest gets for free now: ARP, DHCP, ICMP echo, NTP, DoH DNS.
   TCP handshakes are terminated but refused until something answers
   `on_tcp_connection` — that hook is exactly where `wisp_network.js` slots in.

1. ~~**Build the emulator for `wasm32-unknown-unknown`.**~~ **Done.**
   `crates/riscv-machine` holds the portable boot logic as a `Machine` struct
   taking kernel/initrd/DTB as byte slices, and `crates/riscv-wasm` exposes it
   over `wasm-bindgen`:

   ```bash
   cargo build --release --target wasm32-unknown-unknown -p riscv-wasm
   ```

   147 KB of wasm. The devicetree is passed in rather than embedded, because it
   encodes memory size and command line; `fdt::patch_initrd` rewrites its
   initrd addresses in place to wherever the initramfs lands. Generate one with
   `python3 kernels/gen_dtb_v2.py 0x1000 0x2000 && mv virt.dtb boot.dtb`.

   `cargo test -p riscv-machine` boots the real Alpine images through `Machine`
   and asserts the kernel unpacks the initramfs, which is what proves the
   devicetree patch is right — a failure there is a stack trace instead of a
   blank canvas in a browser tab.
2. ~~**Expose the two queues across the wasm boundary.**~~ **Done** —
   `Vm::net_take` (length-prefixed batch out) / `Vm::net_inject` (one frame
   in), split back into plain `Uint8Array` frames in `web/vm-worker.js`.
3. ~~**Point v86's adapter at them.**~~ **Done** — item 0 above.
4. ~~**Stand up a WISP relay.**~~ **Done — `apk` works.**

   The relay is `wisp.js` in the *sandpie-server* repo (mounted at `/wisp`,
   inert unless `WISP_ENABLED=1`, allowlisted to Alpine mirrors by default).
   Neither `wisp-server-node` nor `epoxy-server` was used: the protocol is four
   packet types, and writing it meant the allowlist and SSRF guard could be
   part of the thing rather than bolted around it.

   Client side here is `web/wisp-egress.js`, which implements `on_tcp_connection`
   — the hook fake_network calls for every TCP connection it terminates:

   ```bash
   # relay (in the sandpie-server checkout)
   WISP_ENABLED=1 WISP_ALLOW_ORIGINS=http://localhost:8139 \
       node scripts/wisp-standalone.mjs 6970
   # then open
   http://localhost:8139/web/?wisp=ws://127.0.0.1:6970/wisp
   ```

   `?wisp=1` uses this origin's own `/wisp` instead.

   **Gotcha: the hook only knows a destination IP.** The guest resolved the
   name itself and the SYN carries no trace of it, but the relay allowlists
   *hostnames* (a list of CDN IPs is unmaintainable) and TLS wants one for SNI.
   Rather than fork the vendored file, `net-adapter.js` snoops the DNS answers
   passing through `receive()` and keeps an ip→name map.

   Inside the guest, from the initramfs shell:

   ```bash
   ip link set eth0 up
   ip addr add 192.168.86.100/24 dev eth0
   ip route add default via 192.168.86.1
   echo nameserver 192.168.86.1 > /etc/resolv.conf
   mkdir -p /etc/apk /lib/apk/db /var/cache/apk
   echo http://dl-cdn.alpinelinux.org/alpine/v3.22/main > /etc/apk/repositories
   touch /lib/apk/db/installed /etc/apk/world   # apk-tools 3 has no --initdb
   apk update && apk add --no-scripts bash
   ```

   Result: `5588 distinct packages available`, then 7 packages / 2585 KiB
   installed, and `bash -c 'echo $BASH_VERSION'` → `5.2.37(1)-release` on
   riscv64. Use `http://`, not `https://` — apk verifies package signatures
   against `/etc/apk/keys` (which the initramfs has), so transport TLS buys
   nothing here and would need libssl in the guest.

   `apk-tools 3.0.6` errors are misleading if the database is missing: "Unable
   to lock database" then "Unable to read database" both mean the two files
   above do not exist, not that the network failed.

5. **Optional: a relay you did not write.** `wisp-server-node` or
   `epoxy-server`. Check the
   current WISP spec version before pinning — there was a v1 to v2 revision and
   the v86 file's vintage should be confirmed rather than assumed.

### Things that will bite

- **A public WISP relay is an open proxy.** It will be found. Restrict the
  allowlist to Alpine mirrors, or put it behind auth on your own box.
- **TLS is a non-issue.** WISP is a byte pipe, so the guest's TLS terminates at
  the real mirror. No MITM, no certificate injection.
- **Plain HTTP to the repo is fine too.** `apk` verifies package signatures
  against `/etc/apk/keys`; transport integrity is not what protects you.
- **DNS needs no relay.** `fake_network.js` defaults to DNS-over-HTTPS via
  plain `fetch`, so name resolution works without touching the WISP server.
- `/etc/apk/repositories` in the generated rootfs already points at
  `dl-cdn.alpinelinux.org`, so `apk update` is the single command that proves
  the whole path.

### If a WebSocket relay is not an option

`src/browser/fetch_network.js` in the same directory tunnels over plain HTTP
requests instead. Slower, but it needs no persistent connection.

## Persistent disk (browser)

`/dev/vda` is a 256 MiB ext4 living in OPFS. Verified: mount, write a file,
reload the page (destroying the VM), remount — same filesystem UUID, file
intact.

```bash
python3 kernels/mkdisk.py     # build disk-ext4.img.gz (256 MiB -> ~255 KiB)
python3 serve.py 8139         # no-cache static server; see the warning below
```

The worker mounts it for you on restore; the terminal prints
`[disk] persistent /mnt/disk ready`. To do it by hand:

```bash
mkdir -p /mnt/disk && mount -t ext4 /dev/vda /mnt/disk
```

The mount happens on restore rather than being baked into the snapshot on
purpose. A snapshot taken while mounted also captures the guest page cache
(superblock, bitmaps, inode tables), and that cache must match the disk exactly
when it is restored. The disk is persistent and changes between sessions, so a
mounted snapshot would eventually restore a stale cache over a modified
filesystem and corrupt it silently. Mounting at restore costs about a second of
emulated time and is always coherent.

The image is formatted host-side rather than with `mkfs` in the guest. Doing it
in the guest would mean apk-installing e2fsprogs and running mkfs on an
emulated CPU on every clean start — minutes of work for a byte-identical
result. The worker seeds OPFS the first time it sees a disk with no ext4
superblock magic at offset 0x438.

Two things that will bite:

- **Use `serve.py`, not `python -m http.server`.** The stdlib server sends no
  `Cache-Control`, so Chrome caches the ES modules and the Worker script.
  Editing `vm-worker.js` and reloading then silently runs the OLD code. An hour
  went into debugging a "bug" that had simply never been loaded; the giveaway
  was a global added to `main.js` being `undefined` in the page.
- **The OPFS sync access handle is exclusive**, and on reload the previous
  page's Worker has not necessarily released it before the new one starts.
  Losing that race leaves the disk detached, and the guest reports
  `I/O error ... unable to read superblock` — which reads as a corrupt
  filesystem rather than a handle that was busy for 200 ms. Handled by
  terminating the Worker on `pagehide` plus a retry loop in `openDisk`.
