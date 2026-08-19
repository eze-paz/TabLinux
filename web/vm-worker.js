// The VM lives in a Worker so a 100%-busy guest cannot freeze the page.
//
// Protocol, all transferable-friendly:
//   worker -> main: {console: Uint8Array}  guest UART output
//                   {tx: Uint8Array}       one Ethernet frame the guest sent
//                   {status: string}       lifecycle notes for the page
//                   {stats: {...}}         periodic instruction counter
//   main -> worker: {input: Uint8Array}    keystrokes
//                   {rx: Uint8Array}       one Ethernet frame for the guest
//
// The network adapter runs on the MAIN thread, not here: fake_network's DNS
// uses fetch(), and keeping it out of the worker means the VM never blocks on
// the network stack and vice versa.

import init, { Vm } from "./pkg/riscv_wasm.js?v=2";
// The raw wasm exports: the JIT needs the linear memory, the indirect
// function table, and the load*/store* that generated blocks import.
// wasm-bindgen keeps these module-private, so build.sh appends an export.
import { __wasm } from "./pkg/riscv_wasm.js?v=2";
// Cross-session compiled-block cache (?jitcache). Content-addressed by the
// module's own wasm bytes, so reusing a cached module is correct by construction.
import { jitCacheInit, jitCacheGet, jitCachePut, jitCacheStats } from "./jit-cache.js";

// Per slice of the run loop. Big enough to amortise the JS/wasm boundary,
// small enough that input and inbound frames land promptly — and Machine::run
// returns early anyway the moment the guest idles with a frame outstanding.
const SLICE = 2_000_000;

let vm = null;

const pending = [];

/// Boot-timing marks, from when the worker script started. Logged to the console
/// and mirrored to the page (?boottime shows them in the status line). Cheap
/// enough to leave in; the point is to know where the restore second goes.
const _bootT0 = performance.now();
const _bootTime = false;
let _interactiveMarked = false;
let _bootTail = "";
// True while the restore path's setup script is still running. Like
// `coldSetupPending` on the cold path, it forces VIRTUAL time (host_ns = 0) so
// the setup's virtio-completion waits skip instantly instead of handing control
// back on every wait — each hand-back would otherwise cost a ~4ms clamped
// setTimeout in the browser, which turned an ~1s setup into ~18s.
let restoreSetupPending = false;
// Diagnostics for the setup phase: how many pump iterations ran, and the JIT
// block / step baselines when it started — so INTERACTIVE can report pumps (=
// setTimeout tax), blocks compiled (= one-shot JIT overhead) and steps (= work).
let _setupPumps = 0;
let _setupBaseSteps = 0;
let _setupBaseBlocks = 0;
function bmark(label) {
    const ms = Math.round(performance.now() - _bootT0);
    console.log(`[boot] +${ms}ms  ${label}`);
    if (_bootTime) postMessage({ status: `boot +${ms}ms: ${label}` });
}

/// Terminal size the page last reported, applied by the boot script.
///
/// Recorded outside the queue below, because it must be known while the boot
/// script is being built — which happens before `vm` exists and before
/// anything queued is replayed.
let winsize = null;

/// A cold boot still owes its setup commands.
///
/// They cannot be typed at construction the way a restore's can: the kernel has
/// not booted, and nothing reads the console until the initramfs reaches its
/// rescue shell. So the run loop watches the output and types them once the
/// prompt appears.
let coldSetupPending = false;
/// RAM size this machine was cold booted at, so the result can be cached under
/// that size. Zero when the machine was resumed and there is nothing to cache.
let coldMb = 0;
/// Whether that cold boot actually got a disk. See where the cache is written.
let coldHadDisk = false;
/// Tail of recent console output, kept only while the above is true, so the
/// prompt is still recognised when it straddles two chunks.
let coldTail = "";
/// What the initramfs rescue shell prints when it is ready for input. Matched
/// rather than the "Launching ... shell" banner above it, which appears while
/// the shell is still starting.
const COLD_PROMPT = "~ # ";

onmessage = e => {
    // The wake futex, once, right after worker creation. Not machine state,
    // so it is handled before the vm-exists check.
    if (e.data.wake) {
        wakeInt = new Int32Array(e.data.wake);
        return;
    }
    if (e.data.winsize) winsize = e.data.winsize;
    if (!vm) {
        pending.push(e.data);
        return;
    }
    apply(e.data);
    // A keystroke or an RX frame makes a parked guest runnable; if the pump
    // is asleep on a deep-idle timer, run it now instead. (A futex nap was
    // already woken by the sender's Atomics.notify before this handler ran.)
    wakePump();
};

function apply(msg) {
    if (msg.input) vm.input(msg.input);
    if (msg.rx) {
        vm.net_inject(msg.rx);
        lastNetMs = performance.now();
    }
    if (msg.p9index !== undefined) p9CloudIndex = msg.p9index;
}

/// When a network frame last moved in either direction. While traffic is
/// recent the guest is NOT idle even though it parks exactly like an idle
/// machine (far TCP-timer deadline, nothing pending — the reply simply has
/// not arrived), and its TCP timers must not be paced to real time: the
/// vendored network shim is timing-sensitive stop-and-wait with no
/// retransmission, and pacing its delayed-ACK/retry timers turned apk
/// installs into a crawl. Within NET_ACTIVE_MS of traffic the clock reverts
/// to virtual fast-forward and naps are suppressed — the pre-realtime
/// behavior, scoped to transfers.
let lastNetMs = -1e9;
const NET_ACTIVE_MS = 1000;

// Persistent disk. Must match the size the snapshot was taken with, since
// virtio-blk refuses a capacity mismatch — a disk that changes size under a
// filesystem corrupts it silently.
const DISK_MB = 256;
const SECTOR = 512;

// The 9p shared folder. The guest mounts it under this tag; the host mirrors it
// to an OPFS directory of the same name, so files written on either side show up
// on the other. Unlike the disk (an opaque ext4 image), these are real OPFS
// entries the rest of the page — and the user — can see.
const P9_TAG = "shared";
const P9_DIR = "shared"; // OPFS directory mirroring the share

// One mode, every feature on — no query flags. The 9p share roots the whole
// OPFS tree and serves it on demand: directory listings are walked from OPFS
// here and merged with the dehydrated-Dropbox cloud index the page pushes over
// (so cloud-only placeholders show up); file contents are fetched through
// `/files/<path>`, which the sandpie service worker serves from OPFS or hydrates
// from Dropbox. Guest changes write back to OPFS and, via the page's sync-core,
// to Dropbox. Compiled JIT blocks persist across sessions, and the boot is quiet.
// Names the VM keeps in the OPFS root (its own disk image and snapshots) are
// hidden from the guest's view of `/`.
const lazy9p = true;
const p9write = true;
const jitCacheOn = true;
const verbose = false;
const P9_HIDE = /^(disk\.img|disk-.*\.img|snap-|snapshot|\.)/;

// The dehydrated-Dropbox cloud index, pushed from the page (which can read the
// localStorage it lives in; a Worker cannot). Maps OPFS-relative path -> entry
// { kind: 'file'|'folder', size }. Used to surface cloud-only placeholders in
// directory listings. Null until the page sends it (or if not in dehydrated
// mode).
let p9CloudIndex = null;

/// Percent-encode a `/`-separated OPFS path for use in a `/files/<path>` URL,
/// segment by segment so the separators survive.
function filesUrl(rel) {
    return "/files/" + rel.split("/").map(encodeURIComponent).join("/");
}

/// Resolve the OPFS directory handle at a `/`-separated relative path, or null.
async function opfsDirAt(rel) {
    let dir;
    try {
        dir = await navigator.storage.getDirectory();
    } catch {
        return null;
    }
    for (const seg of rel.split("/").filter(Boolean)) {
        try {
            dir = await dir.getDirectoryHandle(seg);
        } catch {
            return null;
        }
    }
    return dir;
}

/// Merge dehydrated Dropbox placeholders into a directory listing. The cloud
/// index (localStorage on the page, pushed here as `p9CloudIndex`) holds the
/// full Dropbox tree keyed by OPFS-relative path; a Worker cannot read
/// localStorage, so the page hands it over. Entries the guest can see locally
/// win; cloud-only ones are added as placeholders (folders inferred from any
/// deeper key, files carrying their known size), so `ls` shows the whole tree
/// and reading one hydrates it through `/files/<path>`.
function mergeCloudEntries(rel, entries) {
    if (!p9CloudIndex) return;
    const prefix = rel === "" ? "" : rel + "/";
    const pl = prefix.toLowerCase();
    for (const k of Object.keys(p9CloudIndex)) {
        if (prefix && !k.toLowerCase().startsWith(pl)) continue;
        const rest = k.slice(prefix.length);
        if (!rest) continue;
        const slash = rest.indexOf("/");
        if (slash >= 0) {
            const seg = rest.slice(0, slash);
            if (!entries.has(seg)) entries.set(seg, { isDir: true, size: 0 });
        } else if (!entries.has(rest)) {
            const e = p9CloudIndex[k] || {};
            const isDir = e.kind === "folder" || e.kind === "dir";
            entries.set(rest, { isDir, size: isDir ? 0 : e.size || 0 });
        }
    }
}

/// List an OPFS directory in the wire format `Virtio9p::apply_listing` parses:
///   namelen[u16 LE] | name | flags[u8] (bit0 = is_dir) | size[u64 LE]
/// repeated. The root hides the VM's own artifacts; cloud placeholders are
/// merged in on top of the local entries.
async function listDirSerialized(rel) {
    const entries = new Map(); // name -> { isDir, size }
    const dir = await opfsDirAt(rel);
    if (dir) {
        for await (const [name, handle] of dir.entries()) {
            if (rel === "" && P9_HIDE.test(name)) continue;
            const isDir = handle.kind === "directory";
            let size = 0;
            if (!isDir) {
                try {
                    size = (await handle.getFile()).size;
                } catch {
                    size = 0;
                }
            }
            entries.set(name, { isDir, size });
        }
    }
    mergeCloudEntries(rel, entries);

    const parts = [];
    for (const [name, { isDir, size }] of entries) {
        const nb = new TextEncoder().encode(name);
        const rec = new Uint8Array(2 + nb.length + 1 + 8);
        const dv = new DataView(rec.buffer);
        dv.setUint16(0, nb.length, true);
        rec.set(nb, 2);
        rec[2 + nb.length] = isDir ? 1 : 0;
        dv.setBigUint64(2 + nb.length + 1, BigInt(size), true);
        parts.push(rec);
    }
    const total = parts.reduce((n, p) => n + p.length, 0);
    const out = new Uint8Array(total);
    let o = 0;
    for (const p of parts) {
        out.set(p, o);
        o += p.length;
    }
    return out;
}

/// Fetch the bytes of a file the way sandpie serves them: `/files/<path>` is
/// intercepted by its service worker, which returns the local OPFS copy or
/// hydrates a Dropbox placeholder first.
async function fetchFileBytes(rel) {
    try {
        const resp = await fetch(filesUrl(rel));
        if (!resp.ok) return new Uint8Array(0);
        return new Uint8Array(await resp.arrayBuffer());
    } catch {
        return new Uint8Array(0);
    }
}

/// Ids already being fetched, so a request the device re-hands out (a listing
/// that must precede a walk's next level) is not fetched twice.
const p9Inflight = new Set();

/// Drain the guest's pending 9p faults and satisfy each asynchronously. Fired
/// (not awaited) from the pump: the guest stays blocked on its RPC until
/// `p9_supply` lands, which is exactly the deferred-completion contract.
function serve9pFaults(vm) {
    let blob;
    try {
        blob = vm.p9_take_reqs();
    } catch {
        return;
    }
    if (!blob || blob.length < 4) return;
    const dv = new DataView(blob.buffer, blob.byteOffset, blob.byteLength);
    let p = 4;
    const count = dv.getUint32(0, true);
    for (let i = 0; i < count && p + 17 <= blob.length; i++) {
        const id = dv.getUint32(p, true);
        const kind = blob[p + 4];
        const off = dv.getBigUint64(p + 5, true);
        const len = dv.getUint32(p + 13, true);
        const pl = dv.getUint32(p + 17, true);
        p += 21;
        const path = new TextDecoder().decode(blob.subarray(p, p + pl));
        p += pl;
        void off;
        void len;
        if (p9Inflight.has(id)) continue;
        p9Inflight.add(id);
        const work = kind === 1 ? listDirSerialized(path) : fetchFileBytes(path);
        work
            .then(payload => { vm.p9_supply(id, payload || new Uint8Array(0)); wakePump(); })
            .catch(() => { vm.p9_supply(id, new Uint8Array(0)); wakePump(); })
            .finally(() => p9Inflight.delete(id));
    }
}

/// Drain the guest mutations the device recorded and relay each to the page,
/// which owns OPFS writes and the sync engine. Kept off the worker because the
/// page (a window) has the cloud index and sync-core; the worker only has the
/// device. Data buffers are transferred, not copied.
function drain9pChanges(vm) {
    let blob;
    try {
        blob = vm.p9_take_changes();
    } catch {
        return;
    }
    if (!blob || blob.length < 4) return;
    const dv = new DataView(blob.buffer, blob.byteOffset, blob.byteLength);
    let p = 4;
    const count = dv.getUint32(0, true);
    for (let i = 0; i < count && p + 1 <= blob.length; i++) {
        const op = blob[p];
        p += 1;
        const pl = dv.getUint32(p, true);
        p += 4;
        const path = new TextDecoder().decode(blob.subarray(p, p + pl));
        p += pl;
        const dl = dv.getUint32(p, true);
        p += 4;
        // slice() copies into a fresh, standalone buffer we can transfer.
        const data = op === 0 ? blob.slice(p, p + dl) : null;
        p += dl;
        if (data) postMessage({ p9change: { op, path, data: data.buffer } }, [data.buffer]);
        else postMessage({ p9change: { op, path, data: null } });
    }
}

/// Load the OPFS share directory into the guest's 9p tree before boot.
///
/// One-way at startup: whatever is in OPFS becomes the guest's view. Directories
/// are created first so an empty one still appears; then every file's bytes are
/// handed to the device. Best-effort — a missing directory just means an empty
/// share.
async function seedShareFromOpfs(vm) {
    let root;
    try {
        const opfs = await navigator.storage.getDirectory();
        root = await opfs.getDirectoryHandle(P9_DIR, { create: true });
    } catch {
        return;
    }
    let files = 0;
    const walk = async (dir, prefix) => {
        for await (const [name, h] of dir.entries()) {
            const path = prefix ? `${prefix}/${name}` : name;
            if (h.kind === "directory") {
                vm.p9_mkdir(path);
                await walk(h, path);
            } else {
                try {
                    const buf = new Uint8Array(await (await h.getFile()).arrayBuffer());
                    vm.p9_put(path, buf);
                    files++;
                } catch {
                    // unreadable entry: skip it rather than fail the whole seed
                }
            }
        }
    };
    await walk(root, "");
    if (files) postMessage({ status: `9p: seeded ${files} file(s) from OPFS` });
}

/// Mirror the guest's 9p tree back to the OPFS share directory.
///
/// Called when the device's dirty counter moves. It rewrites every file rather
/// than diffing — the share is small and this runs at most every second or two.
/// Nested paths create their parent directories on the way down.
let p9Flushing = false;
let p9LastDirty = 0;
async function flushShareToOpfs(vm) {
    if (p9Flushing) return; // never overlap two flushes onto the same handles
    p9Flushing = true;
    try {
        const blob = vm.p9_list();
        const dv = new DataView(blob.buffer, blob.byteOffset, blob.byteLength);
        let o = 0;
        const count = dv.getUint32(o, true); o += 4;
        const opfs = await navigator.storage.getDirectory();
        const root = await opfs.getDirectoryHandle(P9_DIR, { create: true });
        for (let i = 0; i < count; i++) {
            const plen = dv.getUint32(o, true); o += 4;
            const path = new TextDecoder().decode(blob.subarray(o, o + plen)); o += plen;
            const dlen = dv.getUint32(o, true); o += 4;
            const data = blob.subarray(o, o + dlen); o += dlen;

            const parts = path.split("/");
            let dir = root;
            for (const seg of parts.slice(0, -1)) {
                dir = await dir.getDirectoryHandle(seg, { create: true });
            }
            const fh = await dir.getFileHandle(parts[parts.length - 1], { create: true });
            const w = await fh.createWritable();
            await w.write(data);
            await w.close();
        }
    } catch (e) {
        postMessage({ status: `9p flush failed: ${e.message}` });
    } finally {
        p9Flushing = false;
    }
}

/// Report a disk-level failure once per kind, with the storage quota attached.
///
/// The guest can only say "I/O error": every cause — an exhausted origin quota,
/// a revoked handle, a short write — arrives as the same EIO, and a quota that
/// only bites on large writes reads as an intermittently flaky disk. So say what
/// actually happened, and how full the origin's storage is, which is the usual
/// answer.
const _diskFaults = new Set();
function diskFault(what) {
    const key = what.replace(/\d+/g, "#");
    if (_diskFaults.has(key)) return;
    _diskFaults.add(key);
    console.error(`[disk] ${what}`);
    postMessage({ status: `disk fault: ${what}` });
    navigator.storage?.estimate?.().then(e => {
        if (!e) return;
        const pct = e.quota ? ((100 * e.usage) / e.quota).toFixed(1) : "?";
        const msg = `storage ${(e.usage / 1048576).toFixed(0)} MiB of ${(e.quota / 1048576).toFixed(0)} MiB (${pct}%)`;
        console.error(`[disk] ${msg}`);
        postMessage({ status: `disk fault: ${msg}` });
    }).catch(() => {});
}

// OPFS is the only origin-private storage with a *synchronous* read/write API,
// and synchronous is non-negotiable here: a virtio request is serviced inside
// the instruction that kicked the queue, so there is nowhere to await. The
// handle is also Worker-only, which is a second reason the VM lives in one.
async function openDisk() {
    const root = await navigator.storage.getDirectory();
    // Lazy mode needs a disk carrying the 9p modules under /mod. An existing
    // disk.img from before the module seed is already formatted and so is never
    // re-seeded, leaving the guest unable to mount the share. Give lazy mode its
    // own disk file, keyed on the seed version: a fresh name is always
    // unformatted, so it self-seeds the current image — no manual clearing, and
    // the main disk.img is left untouched.
    const name = lazy9p ? `disk-9plazy-v${DISK_SEED_VERSION}.img` : "disk.img";
    const fh = await root.getFileHandle(name, { create: true });

    // A sync access handle is exclusive per file, and the previous page's
    // Worker does not necessarily release it before the new one starts — on
    // reload the two overlap. Losing that race used to leave the disk
    // detached, which the guest reports as "I/O error ... unable to read
    // superblock", i.e. it looks like a corrupt filesystem rather than a
    // handle that was busy for 200 ms. Retry instead.
    let h = null;
    for (let attempt = 0; attempt < 25 && !h; attempt++) {
        try {
            h = await fh.createSyncAccessHandle();
        } catch (e) {
            if (attempt === 24) throw e;
            await new Promise(r => setTimeout(r, 100));
        }
    }
    const want = DISK_MB * 1024 * 1024;
    const fresh = h.getSize() !== want;
    if (fresh) h.truncate(want);

    // Seed an empty ext4 the first time. Formatting in the guest would mean
    // apk-installing e2fsprogs over the network and running mkfs on an
    // emulated CPU, every time someone starts from a clean OPFS — minutes of
    // work for a filesystem that is byte-identical every time. The image is
    // almost all zeros, so it ships as ~255 KiB gzipped.
    const sb = new Uint8Array(2);
    h.read(sb, { at: 1080 }); // ext4 superblock magic lives at offset 0x438
    const formatted = sb[0] === 0x53 && sb[1] === 0xef;
    if (!formatted) {
        postMessage({ status: "formatting persistent disk (first run)" });
        // Cache-busted: the seed image gained the 9p kernel modules under /mod,
        // and a browser holding the old (module-less) copy would re-seed a disk
        // that can never mount the share. Bump DISK_SEED_VERSION when it changes.
        const resp = await fetch(`../kernels/disk-ext4.img.gz?v=${DISK_SEED_VERSION}`);
        if (resp.ok) {
            const raw = await new Response(
                resp.body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer();
            h.write(new Uint8Array(raw), { at: 0 });
            h.flush();
        } else {
            postMessage({ status: "no disk image to seed; /dev/vda stays raw" });
        }
    }
    // One scratch buffer: allocating per request would churn the heap on every
    // block the guest touches.
    let scratch = new Uint8Array(64 * 1024);
    return {
        sectors: want / SECTOR,
        read(sector, count) {
            const len = count * SECTOR;
            if (scratch.length < len) scratch = new Uint8Array(len);
            const view = scratch.subarray(0, len);
            const got = h.read(view, { at: sector * SECTOR });
            if (got < len) view.fill(0, got); // reads past EOF are zeros
            return view;
        },
        write(sector, bytes) {
            // A throw here reaches the guest as a bare EIO ("I/O error" from
            // whatever it was doing) with the real cause swallowed, which is
            // exactly how a storage-quota failure presents: big writes fail while
            // small ones succeed, looking like a flaky disk. Report the actual
            // exception, and note a short write too — that one is worse than an
            // error, because the guest is told the write succeeded.
            try {
                const n = h.write(bytes, { at: sector * SECTOR });
                if (n !== bytes.length) {
                    diskFault(`short write: ${n} of ${bytes.length} B at sector ${sector}`);
                }
            } catch (e) {
                diskFault(`write ${bytes.length} B at sector ${sector} threw ${e.name}: ${e.message}`);
                throw e; // still an error to the guest; it may be retryable
            }
        },
        flush: () => h.flush(),
    };
}

/// Where a locally-booted machine is cached, per RAM size.
///
/// Versioned, so a cache written by an older build is ignored rather than
/// trusted. v1 entries could have been taken by a boot that came up without a
/// disk — virtio-mmio has no hotplug, so restoring one hands back a diskless
/// guest forever, and no amount of later fixing reaches inside a file that is
/// already on disk. Bumping the name retires them without asking anyone to go
/// and delete anything.
// v3: the snapshot now has the 9p modules, the overlay module and the static
// network baked in (see make_snapshot's bake step), so the post-restore setup
// skips them. Bumped to bust the 24 h browser cache of the old v2 snapshot.
const SNAP_VERSION = 5;
// Bumped when kernels/disk-ext4.img.gz changes, to bust a stale browser cache
// on re-seed AND to key a fresh disk file (disk-9plazy-v<N>.img) so existing
// users auto-migrate. v3 = added the 9p kernel modules under /mod; v4 = dropped
// ext4 metadata_csum (~72% of the post-restore setup was software crc32c).
const DISK_SEED_VERSION = 4;
const snapName = mb => `snap-${mb}mb-v${SNAP_VERSION}.gz`;

/// Restore a machine, preferring one that matches `wantMb`.
///
/// Two sources. The shipped snapshot was taken at one size, so it can only
/// answer for that size; anything else has to be cold booted once and is then
/// cached in OPFS under its own name. Without that cache, choosing a non-
/// default RAM size meant watching the kernel boot from zero on every single
/// reload, because the size lives in the URL and the URL survives.
///
/// `wantMb` of 0 means "no preference": take the shipped snapshot as-is.
async function trySnapshot(wantMb) {
    if (wantMb) {
        const local = await readCachedSnapshot(wantMb).catch(() => null);
        if (local) {
            postMessage({ status: `restoring cached ${wantMb} MiB machine` });
            const vm = Vm.restore(local) ?? null;
            if (vm) return vm;
        }
    }

    // ?v tracks the shipped snapshot generation. The file is served with
    // max-age=86400, so a changed snapshot at the same URL would be masked by
    // the browser/CDN cache for a day; bumping this key forces a fresh fetch.
    // v2 = the 256 MiB default (was 1 GiB).
    const resp = await fetch(`../kernels/shell.snap.gz?v=${SNAP_VERSION}`);
    if (!resp.ok) return null;
    bmark("snapshot fetch: headers");
    postMessage({ status: "restoring snapshot" });
    const inflated = resp.body.pipeThrough(new DecompressionStream("gzip"));
    const bytes = new Uint8Array(await new Response(inflated).arrayBuffer());
    bmark(`snapshot downloaded+inflated (${(bytes.length / 1048576).toFixed(0)} MiB)`);
    const vm = Vm.restore(bytes) ?? null; // undefined = format mismatch: reboot
    bmark("Vm.restore done");
    if (!vm) return null;
    // The shipped snapshot has whatever size it was taken at. Asking for that
    // same size is a resume, not a cold boot — which the previous check missed,
    // so ?ram=1024 booted from scratch despite the snapshot being 1024 MiB.
    if (wantMb && vm.dram_mb() !== wantMb) return null;
    return vm;
}

async function readCachedSnapshot(mb) {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle(snapName(mb)); // throws when absent
    const file = await fh.getFile();
    if (!file.size) return null;
    const inflated = file.stream().pipeThrough(new DecompressionStream("gzip"));
    return new Uint8Array(await new Response(inflated).arrayBuffer());
}

/// Cache the running machine so this size resumes next time.
///
/// Gzipped, because the raw form is tens of megabytes and this is written once
/// per size but read on every load. Best-effort throughout: a failure here
/// costs another cold boot next time and nothing else, so it must never take
/// the machine down with it.
async function cacheSnapshot(mb, bytes) {
    try {
        const root = await navigator.storage.getDirectory();
        // Reclaim what earlier versions left behind. These are tens of
        // megabytes each and nothing will ever read them again.
        for (let v = 1; v < SNAP_VERSION; v++) {
            await root.removeEntry(`snap-${mb}mb-v${v}.gz`).catch(() => {});
        }
        await root.removeEntry(`snap-${mb}mb.gz`).catch(() => {}); // unversioned v1
        const fh = await root.getFileHandle(snapName(mb), { create: true });
        const gz = new Blob([bytes]).stream()
            .pipeThrough(new CompressionStream("gzip"));
        const packed = new Uint8Array(await new Response(gz).arrayBuffer());
        const w = await fh.createWritable();
        await w.write(packed);
        await w.close();
        postMessage({ status: `cached ${mb} MiB machine (${(packed.length / 1048576).toFixed(0)} MiB)` });
    } catch (e) {
        postMessage({ status: `snapshot cache failed: ${e.message}` });
    }
}

/// Guest RAM for a cold boot, in MiB, overridden by ?ram=. Allocated up front
/// as one flat Vec, so the tab holds this much browser memory whether or not
/// the guest touches it — it is most of the footprint. 256 keeps a disposable
/// shell light (~4x smaller than the old 1 GiB default); ?ram=1024 cold-boots
/// a roomier machine once, then caches it. Matches the shipped snapshot size,
/// so ?ram=256 and the no-arg default both resume rather than cold boot.
const DEFAULT_RAM_MB = 256;

async function fullBoot(mb = DEFAULT_RAM_MB, disk = null) {
    postMessage({ status: "fetching kernel + initramfs + devicetree" });
    const [kernel, initrd, dtb] = await Promise.all(
        ["../kernels/vmlinuz-lts.raw", "../kernels/boot/initramfs-lts", "../kernels/boot.dtb"]
            .map(u => fetch(u).then(r => {
                // A cold boot needs the kernel, initramfs and devicetree. The
                // resume path needs none of them, so a deployment can carry
                // only the snapshot and look completely healthy until the
                // moment someone picks a RAM size — which is how this first
                // went wrong. Say which file and why it is being asked for.
                if (!r.ok) {
                    throw new Error(
                        `${u.split("/").pop()} missing (HTTP ${r.status}) — a cold boot ` +
                        `needs the kernel, initramfs and devicetree, which the snapshot ` +
                        `does not. Reload without ?ram= to resume instead.`);
                }
                return r.arrayBuffer();
            })),
    );
    const v = new Vm(new Uint8Array(kernel), new Uint8Array(initrd), new Uint8Array(dtb), mb);
    // Disk before net, and both before the guest runs a single instruction:
    // virtio-mmio has no hotplug, so Linux probes these slots once and a device
    // that appears later is a device the guest never sees. Disk first to match
    // the order the snapshot was built with, so the two paths present the same
    // layout.
    if (disk && !v.attach_disk(disk.sectors, disk.read, disk.write)) {
        postMessage({ status: "no disk attached; /mnt/disk will not be available" });
    }
    if (!v.attach_net(new Uint8Array([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]))) {
        throw new Error("attach_net failed");
    }
    // The 9p share, in the next free slot. Must be attached now, before boot,
    // for the same no-hotplug reason. On the restore path it instead comes back
    // from the snapshot, so that path does not call this.
    //
    // On by default. It was briefly opt-in (?p9, 33e2a96) because the attached
    // device appeared to starve the console — the terminal went dead. That was
    // the JIT interrupt-delivery bug (a compiled CSR write unmasking a pending
    // irq was never delivered), not 9p: the guest's rw disk mount hung the same
    // way with 9p off. Fixed in 1ebbf13, so the shared folder is always on.
    if (!v.attach_9p_lazy(P9_TAG)) {
        postMessage({ status: "no slot for 9p; shared folder disabled" });
    }
    return v;
}

// Blocks to accumulate before compiling. A module carries a fixed cost in
// compilation and V8 metadata, so one module per queued block -- which is what
// this did originally -- produces thousands of them and is most of why a long
// session reached 1.7 GB and slowed from ~60 MIPS to ~33.
const JIT_BATCH = 48;
// ...but not forever. Hot code that trickles in slowly would otherwise never
// reach the threshold and never be compiled.
const JIT_BATCH_WAIT = 12;
// Compiled blocks before the cache is discarded wholesale. Blocks are re-formed
// from the guest's instruction stream on demand, so this costs recompiling
// what is still hot; letting it grow without bound costs everything.
const JIT_CACHE_MAX = 200000;

let jitWaited = 0;
/// How many times the code cache has been discarded. Reported, not shown as
/// a status.
let jitResets = 0;
let jitInstances = [];
let jitFirstSlot = 0;

/// Session profiler: where the worker's wall time goes, cumulative since the
/// machine started, in milliseconds of performance.now(). `run` is time inside
/// vm.run (guest execution, compiled or interpreted); `build`/`compile`/
/// `instantiate` split the JIT pipeline (our codegen, V8's module compile,
/// linking); `idle` is time between pumps (setTimeout waits + scheduling); the
/// rest of a pump (console, net, 9p, stats, snapshots) lands in `io`.
const prof = {
    run: 0, build: 0, compile: 0, instantiate: 0, io: 0, idle: 0,
    pumps: 0, compiles: 0, compiledBytes: 0, cacheHits: 0,
    // Deep-idle sleeps taken instead of spinning the WFI cycle, and how many
    // of those were blocking futex waits rather than setTimeout.
    sleeps: 0,
    futexNaps: 0,
    // Wall stamp (s) of each cache discard, with the block count it hit.
    resets: [],
};
let profPumpEnd = 0;

/// Cancels a pending deep-idle sleep and pumps immediately. Assigned inside
/// boot() (it needs the pump channel); called from every path that can make a
/// parked guest runnable while the worker sleeps: input and RX frames
/// (onmessage) and lazy-9p replies (p9_supply's completion). Latency from
/// those events is therefore unchanged by sleeping.
let wakePump = () => {};

/// Futex the page notifies on every message it sends (input, RX frames), so a
/// nap can BLOCK in Atomics.wait — microsecond wake, no timer clamping, no
/// background-tab throttling — instead of scheduling a setTimeout. Null until
/// the page hands it over, or forever when the origin is not cross-origin
/// isolated; the setTimeout path remains as the fallback. ?nofutex forces the
/// fallback for A/B comparison.
let wakeInt = null;

/// Throw away every compiled block and the memory behind it.
///
/// Clearing the table slots is the part that frees anything: a funcref in the
/// table is a live reference to its module, so dropping the instances alone
/// reclaims nothing.
function jitDiscard(vm) {
    prof.resets.push({ at: Math.round(performance.now() / 1000), blocks: vm.jit_installed_count() });
    const table = __wasm.__indirect_function_table;
    vm.jit_flush();
    for (let i = jitFirstSlot; i < table.length; i++) {
        try { table.set(i, null); } catch (e) { break; }
    }
    jitInstances = [];
    jitFirstSlot = table.length;
    // Counted, not announced. This used to overwrite the status line, where it
    // then stayed for the rest of the session — so one reset during a cold
    // boot, which is entirely routine when a whole kernel gets compiled, read
    // as "the cache is always resetting". A count in the stats panel says how
    // often it really happens without pretending to be the machine's state.
    jitResets++;
}

/// Compile and link whatever blocks the JIT has queued.
///
/// Returns how many were linked. Called from the run loop; does nothing when
/// the JIT is off, because nothing ever gets queued.
function jitPump(vm) {
    // jit_pending reports one deterministic batch, not the whole queue, so a
    // formation burst (a boot, a big program's first run) needs several builds
    // per pump — otherwise hot code queued behind the batch boundary runs
    // interpreted for pumps on end, which measured as a 4x throughput hole.
    let linked = 0;
    for (;;) {
        const built = jitPumpOne(vm);
        if (built === 0) return linked;
        linked += built;
    }
}

function jitPumpOne(vm) {
    const count = vm.jit_pending();
    if (count === 0) {
        jitWaited = 0;
        return 0;
    }
    // Wait for a worthwhile batch, but not indefinitely.
    if (count < JIT_BATCH && ++jitWaited < JIT_BATCH_WAIT) return 0;
    jitWaited = 0;

    if (vm.jit_installed_count() > JIT_CACHE_MAX) {
        jitDiscard(vm);
        return 0;
    }

    const table = __wasm.__indirect_function_table;
    if (jitFirstSlot === 0) jitFirstSlot = table.length;
    // Grow before building: the generated module declares a minimum table size
    // of base + count and will not link against a smaller one. Read the count
    // first -- jit_build consumes the queue.
    const base = table.length;
    table.grow(count);

    const tBuild = performance.now();
    const bytes = vm.jit_build(base);
    prof.build += performance.now() - tBuild;
    if (!bytes || bytes.length === 0) return 0;

    try {
        // Content-addressed cache: a hit skips V8's (expensive) module compile
        // and reuses the exact compilation of these exact bytes. A miss compiles
        // and stores. Both paths instantiate identically below.
        const tCompile = performance.now();
        let mod = jitCacheOn ? jitCacheGet(bytes) : null;
        if (mod) {
            prof.cacheHits++;
        } else {
            mod = new WebAssembly.Module(bytes);
            if (jitCacheOn) jitCachePut(bytes, mod);
        }
        prof.compile += performance.now() - tCompile;
        prof.compiles++;
        prof.compiledBytes += bytes.length;
        const tInst = performance.now();
        // Held so the instance is not collected while its blocks are still in
        // the table; released by jitDiscard.
        jitInstances.push(new WebAssembly.Instance(mod, {
            env: {
                mem: __wasm.memory,
                __indirect_function_table: table,
                // The install slot is no longer baked into the module; it reads
                // this immutable global from the element-segment offset, so the
                // same compiled bytes can be installed at any base (the basis for
                // the cross-session block cache).
                table_base: new WebAssembly.Global({ value: "i32", mutable: false }, base),
                load8u: __wasm.load8u, load16u: __wasm.load16u,
                load32u: __wasm.load32u, load64: __wasm.load64,
                store8: __wasm.store8, store16: __wasm.store16,
                store32: __wasm.store32, store64: __wasm.store64,
                csr: __wasm.csr, fp: __wasm.fp,
            },
        }));
        prof.instantiate += performance.now() - tInst;
        // Only now are the blocks callable. If instantiation threw, they stay
        // unrecorded rather than pointing at table slots holding nothing.
        vm.jit_installed(base);
        return count;
    } catch (e) {
        console.error("[jit] link failed, falling back to the interpreter:", e);
        vm.jit_enable(false);
        return 0;
    }
}

/// The commands that turn a booted kernel into a usable machine.
///
/// Shared by both paths. Only the timing differs: a restored snapshot is
/// already sitting at a prompt, while a cold boot has to reach its rescue
/// shell first -- see `coldSetupPending`.
function setupCommands(restore = false) {
    const ovlDirs = "etc usr lib bin sbin var";
    // On the restore path the module loads and the static network are already
    // resident in the snapshot (baked by make_snapshot), so skip them — only the
    // disk-coupled work (mounts) has to run. A cold boot (`restore` false, e.g.
    // the snapshot was unavailable) still does the full sequence.
    const boot = [
        // Console hush. On a restore the snapshot already has echo off + printk
        // hushed (baked by make_snapshot), so the setup is silent from its very
        // first command; nothing to do here unless ?verbose, which undoes it. A
        // cold boot starts loud, so it hushes here instead. Re-enabled at the tty
        // step below.
        verbose
            ? (restore ? "stty -F /dev/ttyS0 echo 2>/dev/null; dmesg -n 4 2>/dev/null" : "true")
            : (restore ? "true" : "stty -F /dev/ttyS0 -echo 2>/dev/null; dmesg -n 1 2>/dev/null"),
        "mkdir -p /mnt/disk; mount -t ext4 /dev/vda /mnt/disk 2>/dev/null" +
            " && echo '[disk] persistent /mnt/disk ready'",
        // The 9p shared folder. On a cold boot the modules are insmod'd from the
        // persistent disk (dependency order: netfs, protocol, virtio transport,
        // filesystem); on a restore they are already loaded, so only the mount runs.
        "mkdir -p /files" +
            (restore
                ? ""
                : "; insmod /mnt/disk/mod/netfs.ko 2>/dev/null" +
                  "; insmod /mnt/disk/mod/9pnet.ko 2>/dev/null" +
                  "; insmod /mnt/disk/mod/9pnet_virtio.ko 2>/dev/null" +
                  "; insmod /mnt/disk/mod/9p.ko 2>/dev/null" +
                  "; echo '[9p] modules loaded'") +
            `; mount -t 9p -o trans=virtio,version=9p2000.L,msize=131072 ${P9_TAG} /files 2>/dev/null` +
            " && echo '[9p] shared folder at /files' || echo '[9p] not mounted'",
        // The overlay layer can end up MASKING the base system, and when it does
        // it takes away the commands needed to repair it: an interrupted package
        // operation deletes a file in the upper layer (leaving a whiteout, a 0:0
        // character device) and never writes the replacement, so the lower
        // /bin/mkdir vanishes from the merged view. The guest then has no mkdir,
        // no grep, no setsid -- so no working shell to unmount anything with.
        // Recovering needed the browser to delete the disk, which the VM's own
        // worker holds open. So this repairs itself instead, in three parts:
        // busybox is stashed on tmpfs (never overlaid) before mounting, leftover
        // whiteouts are cleared while /bin is still intact, and the merged view
        // is checked afterwards -- dropping the overlays if the base is hidden.
        (restore ? "" : "modprobe overlay 2>/dev/null; ") +
            "cp /bin/busybox /tmp/bb 2>/dev/null" +
            "; if find /mnt/disk/ovl -type c 2>/dev/null | grep -q .; then" +
            "   rm -rf /mnt/disk/ovl;" +
            "   echo '[persist] stale overlay layer was masking the base system, reset';" +
            " fi" +
            `; for d in ${ovlDirs}; do` +
            "   mkdir -p /mnt/disk/ovl/$d/u /mnt/disk/ovl/$d/w;" +
            "   mount -t overlay ovl-$d -o" +
            "     lowerdir=/$d,upperdir=/mnt/disk/ovl/$d/u,workdir=/mnt/disk/ovl/$d/w" +
            "     /$d 2>/dev/null;" +
            " done" +
            // Actually RUN something rather than just resolve it. `command -v`
            // only checks that the path exists, which stays true in the worst
            // failure mode: a purged musl leaves /bin/mkdir present but its
            // loader gone, so every binary is unrunnable while every path still
            // looks fine. Executing mkdir catches both that and a missing file.
            "; if ! mkdir -p /tmp/.ovlcheck 2>/dev/null; then" +
            `   for d in ${ovlDirs}; do /tmp/bb umount /$d 2>/dev/null || umount /$d 2>/dev/null; done;` +
            "   /tmp/bb rm -rf /mnt/disk/ovl 2>/dev/null || rm -rf /mnt/disk/ovl 2>/dev/null;" +
            "   echo '[persist] overlay hid the base system: dropped, reset for next boot';" +
            " else" +
            "   echo \"[persist] overlay on $(grep -c ' overlay ' /proc/mounts) of 6 dirs\";" +
            " fi",
        // eth0 up + address + route AND resolv.conf are all baked into the
        // snapshot now; a restore just reports it. A cold boot still does it.
        (restore
            ? ""
            : "ip link set eth0 up" +
              "; ip addr add 192.168.86.100/24 dev eth0 2>/dev/null" +
              "; ip route add default via 192.168.86.1 2>/dev/null" +
              "; echo nameserver 192.168.86.1 > /etc/resolv.conf; ") +
            "echo '[net] eth0 192.168.86.100/24 via 192.168.86.1 (TCP only)'",
        // apk needs a database before it will do anything, and apk-tools 3
        // has no --initdb: without these files it fails with "Unable to
        // lock database" and then "Unable to read database", which read
        // like a network problem and are not. Runs after the overlay, so
        // the result lands on disk and this is a first-boot cost only.
        //
        // The repositories file is only written when absent, so an edited
        // one survives. http rather than https on purpose: apk verifies
        // package signatures against /etc/apk/keys, and the guest has no
        // libssl.
        // The database starts empty, so apk believes NOTHING is installed --
        // including musl and busybox, which came from the initramfs and which it
        // cannot see. It will happily install its own musl as a dependency and
        // then, because nothing in its database claims to need it, purge it again
        // on the next `apk del`. That deletion writes a whiteout over the
        // initramfs copy of the loader, so every dynamically linked binary in the
        // guest -- i.e. all of busybox -- stops running: `apk del curl` bricked
        // the shell exactly this way. Listing them in `world` marks them as
        // explicitly wanted, so they are never removed as orphans.
        "mkdir -p /etc/apk /lib/apk/db /var/cache/apk" +
            "; touch /lib/apk/db/installed" +
            "; [ -s /etc/apk/world ] || printf 'musl\\nbusybox\\n' > /etc/apk/world" +
            "; [ -s /etc/apk/repositories ] ||" +
            "   echo http://dl-cdn.alpinelinux.org/alpine/v3.22/main" +
            "     > /etc/apk/repositories" +
            "; echo \"[apk] ready ($(wc -l < /etc/apk/world) in world)\"",
        // Last, because it replaces this shell. The initramfs rescue shell
        // is started without a controlling terminal — `tty` reports "not a
        // tty" and the boot log says "job control turned off" — so the tty
        // layer has no foreground process group to deliver SIGINT to and
        // Ctrl-C does nothing. The keystroke was always arriving; there
        // was nothing to receive it.
        //
        // setsid makes the child a session leader, and a session leader's
        // first opened terminal becomes its controlling terminal, which is
        // what gives it a foreground process group. Not `exec`ed, so if
        // this ever fails the original shell is still there rather than
        // the machine ending up with no prompt at all.
        // The terminal's size, set on the tty rather than typed at a shell.
        //
        // A serial console carries no size out of band, so `stty` is the
        // only way to set it — and typing that at a prompt is a guess about
        // what is reading stdin at that moment. Sent too early it lands in
        // the middle of this very script; sent while a program is running
        // it goes to that program. Doing it HERE removes the guess for the
        // common path: it is applied to /dev/ttyS0 before the interactive
        // shell exists, so the shell and everything it runs inherit it.
        //
        // Getting this wrong is not subtle. A guest that believes it is
        // wider than the terminal draws past the edge, the line wraps, and
        // every carriage return then lands at the start of the WRAPPED
        // line — which is what turned apk's progress bar into a staircase
        // of hashes marching down the screen.
        // Re-enable echo, which the snapshot deliberately saved OFF so the setup
        // types silently. /tmp/bb first: if the overlay layer has masked /bin,
        // `stty` is gone and echo would stay off forever — the terminal then
        // looks completely dead even though keystrokes are arriving, which is
        // exactly how a masked base system presents to the user.
        (verbose ? "" : "/tmp/bb stty -F /dev/ttyS0 echo 2>/dev/null || stty -F /dev/ttyS0 echo 2>/dev/null; dmesg -n 4 2>/dev/null; ") +
            (winsize
                ? `stty -F /dev/ttyS0 rows ${winsize.rows} cols ${winsize.cols} 2>/dev/null` +
                  `; echo '[tty] size ${winsize.cols}x${winsize.rows}'`
                : "true"),
        // (the page is told below which size this applied, so it does not
        //  type the same command again at the prompt)
        "echo '[tty] Ctrl-C, Ctrl-Z and job control enabled'" +
            "; setsid sh -c 'exec sh </dev/ttyS0 >/dev/ttyS0 2>&1'",
    ].join("\n") + "\n";
    return boot;
}

async function boot() {
    postMessage({ status: "loading wasm" });
    await init();
    bmark("wasm ready");

    const disk = await openDisk().catch(err => {
        // Reported to the panel as well as the status line. A status is
        // transient — the next one overwrites it — and this failure is silent
        // afterwards: the guest simply has no /mnt/disk and nothing installed
        // survives a reload, which looks like the guest's fault rather than a
        // storage handle that could not be taken.
        postMessage({ status: `disk unavailable: ${err.message}`, disk: `unavailable` });
        return null;
    });
    if (disk) postMessage({ disk: `${DISK_MB} MiB` });
    bmark("disk open");

    // RAM is fixed at 256 MiB: wantMb = 0 means "take the shipped snapshot as
    // is", which was captured at 256, so the machine always resumes fast and
    // never cold boots for a size that has no snapshot.
    const wantMb = 0;
    vm = await trySnapshot(wantMb).catch(() => null);
    if (vm) {
        // Rebind the bytes. The device itself came back with the snapshot —
        // it had to, because Linux probes virtio-mmio slots once at boot and
        // there is no hotplug.
        if (disk && !vm.attach_disk(disk.sectors, disk.read, disk.write)) {
            postMessage({ status: "FAILED: disk size does not match the snapshot" });
            return;
        }
        // The 9p device came back from the snapshot in seeded mode. Flip it to
        // lazy now — before the setup script mounts it — so the on-demand OPFS
        // root works on the fast restore path, not only on a cold boot.
        vm.p9_set_lazy();
        // Mount the disk here rather than baking a mounted filesystem into the
        // snapshot. A snapshot taken while mounted also captures the guest's
        // page cache — superblock, bitmaps, inode tables — and that cache has
        // to match the disk exactly on restore. The disk is persistent and
        // changes between sessions, so a mounted snapshot would eventually
        // restore a stale cache over a modified filesystem and corrupt it
        // silently. Mounting now costs a second of emulated time and is always
        // coherent, because the guest reads the disk as it actually is.
        //
        // /mnt does not exist in the initramfs, hence the mkdir. Re-running it
        // is harmless if something already mounted the disk.
        // Networking is configured here for the same reason, plus one of its
        // own: the guest cannot DHCP. udhcpc needs a packet socket and this
        // kernel has neither AF_PACKET built in nor an af_packet module, so
        // the addresses have to be assigned statically. They must match
        // RiscvNetAdapter's defaults in net-adapter.js — fake_network answers
        // ARP and DNS for the router address, and routing to anything else
        // goes nowhere.
        //
        // Each step is separated by ';' and swallows its error: re-running is
        // harmless, and one already-applied step must not abort the rest.
        // Overlay the directories a package manager writes to, so installs
        // survive a reload. Putting only apk's database on the disk would not
        // work: apk scatters files across /usr, /lib, /etc, /bin, /sbin and
        // /var, and every one of those is tmpfs here. With an overlay the
        // lower (tmpfs) content stays visible and every write lands in the
        // upper directory, which is on the disk.
        //
        // Mounted before the network is configured so that resolv.conf and
        // anything else written to /etc persists too. Each mount swallows its
        // error: if the overlay module is missing the guest simply carries on
        // without persistence rather than failing to boot.
        const boot = setupCommands(true); // restore: baked steps are already in
        bmark("restore ready, typing setup");
        restoreSetupPending = true; // virtual time until the setup finishes
        _setupBaseSteps = Number(vm.steps());
        _setupBaseBlocks = vm.jit_installed_count ? vm.jit_installed_count() : 0;
        vm.input(new TextEncoder().encode(boot));
        // So the page does not type the same stty at the prompt a moment later.
        if (winsize) postMessage({ ttyApplied: winsize });
    } else {
        vm = await fullBoot(wantMb || DEFAULT_RAM_MB, disk);
        // Nothing can be typed yet: the kernel has not booted, let alone
        // reached a shell. The run loop watches for the prompt.
        coldSetupPending = true;
        coldMb = wantMb || DEFAULT_RAM_MB;
        coldHadDisk = !!disk;
    }
    for (const m of pending) apply(m);
    pending.length = 0;

    // Seed the share from OPFS before the guest can read it. Both paths: on a
    // cold boot the device was just attached empty; on a restore it came back
    // from the snapshot empty (its tree is host-side, like the disk's bytes).
    // The setup script mounts it only after this, so the first `ls` is honest.
    //
    // Lazy mode seeds nothing — the tree is faulted in on demand (see
    // serve9pFaults), so both the boot-time copy and the write-back flush poll
    // are skipped.
    if (!lazy9p) {
        await seedShareFromOpfs(vm);
        // Baseline for the flush poll, so seeding does not itself trigger a flush.
        p9LastDirty = vm.p9_dirty();
    }

    // Block compilation is ON by default, on both paths. ?jit=0 falls back to
    // the interpreter, which is the switch to reach for if a guest ever
    // misbehaves in a way that smells like codegen.
    //
    // This was briefly restricted to restored machines, because a cold boot
    // hung under the JIT just after "Mountpoint-cache hash table entries". The
    // cause was real and is fixed: compiled blocks are keyed on physical
    // address and nothing invalidated them on fence.i, so Linux patched its own
    // text during early boot and the JIT kept running the pre-patch version.
    // jit-coldboot-test.js covers that path now.
    // The JIT is always on — it is the whole point of this engine.
    const jitOn = true;
    vm.jit_enable(true);
    // Diagnostic bins for the session profiler (interpreter histogram, TLB-
    // and chain-miss classification). One integer bump on already-slow paths;
    // measured harmless in the node harness at full load.
    if (vm.interp_hist_enable) vm.interp_hist_enable(true);
    // Load the cross-session compiled-block cache before the pump can miss on
    // blocks it already has. Best-effort; a failure just disables it.
    if (jitCacheOn) {
        const s = await jitCacheInit();
        console.log(`[jit] cache init: ${JSON.stringify(s)}`);
        postMessage({ status: s.ok ? `jit cache: ${s.entries} blocks, ${(s.bytes / 1048576).toFixed(1)} MiB` : "jit cache: off" });
    }
    // Both facts the page reports, sourced from the machine rather than from
    // the query string: a restore ignores ?ram= and the page should say the
    // size that actually exists.
    postMessage({ jit: jitOn, ram: vm.dram_mb() });

    postMessage({ status: "running" });
    let lastStats = 0;
    // Held to real time unless ?realtime=0. Without a host clock the emulator
    // cannot tell it is racing: it parks on WFI, jumps the guest clock to the
    // next deadline, and nothing bounds how OFTEN it jumps -- so guest timers
    // fired about 5x early (a 3s sleep took 0.6s) and an idle guest kept
    // retiring instructions, emulating timer ticks the fast-forward had
    // invented. Measured in this page: a warm workload 2024ms -> 1426ms, and an
    // idle prompt 17.3 MIPS -> 0.1.
    //
    // But NOT WHILE COLD BOOTING. A boot spends most of its guest time waiting
    // on device completions at the 10ms kernel tick, and holding those to real
    // time took a cold boot from 68s to 118s. Nothing user-visible depends on
    // the guest's clock being right before it reaches a prompt -- the console is
    // byte-identical either way -- whereas everything interactive depends on it
    // afterwards. So the boot races ahead as it always did, and the clock starts
    // being honest once there is someone to notice. A snapshot restore, which is
    // the common path, gets it immediately.
    // OPT-IN (?realtime=1) until the restore path is fixed: holding the clock
    // to real time during the post-restore setup script (disk, 9p, overlays)
    // regressed restore from ~5s to 40-50s, because that phase was never
    // exempted the way cold boot is. Off by default, nothing here runs.
    // REAL TIME BY DEFAULT (2026-08-14). The guest's idle waits are held to
    // the host clock, so `sleep 5` takes five seconds, cron fires when it
    // says, and timers behave — instead of the free-running virtual clock
    // where an idle guest ran ~117x real time. What made this safe to
    // default now, when it regressed twice before: the boot and restore-
    // setup phases are exempted (they race on the virtual clock exactly as
    // before), sub-5ms waits are still skipped virtually (SHORT_WAIT — the
    // browser cannot time them anyway, and honoring them cost 31% on pipes),
    // and naps go through the wakePump path so input latency is unchanged.
    //
    // ?turbo restores the free-running clock for batch jobs and benchmarks.
    //
    // Honest limit: only IDLE time is paced. While computing, the guest
    // clock still advances one tick per instruction (~10 MHz nominal), so a
    // busy guest's clock can run ahead of real time. Full busy-time pacing
    // would mean sampling the host clock in the tick path — a different,
    // riskier change.
    const realtimeWanted = !new URLSearchParams(self.location.search).has("turbo");
    const nofutex = new URLSearchParams(self.location.search).has("nofutex");
    /// Sleep for `ms`, preferring a blocking Atomics.wait on the page's wake
    /// futex: precise (no 4ms timer clamp), immune to background-tab timer
    /// throttling, and woken in microseconds by the page's notify when input
    /// or an RX frame arrives. Falls back to setTimeout when the futex is
    /// unavailable (?nofutex, or an origin without cross-origin isolation) —
    /// and whenever a lazy-9p fetch is in flight, because a BLOCKED worker
    /// cannot run the fetch completion that would supply the guest's read.
    const nap = (ms) => {
        if (wakeInt && !nofutex && p9Inflight.size === 0) {
            prof.futexNaps++;
            const v = Atomics.load(wakeInt, 0);
            Atomics.wait(wakeInt, 0, v, ms);
            // Yield through the channel rather than pumping here: messages
            // that queued during the block (the very input that woke us) must
            // be applied by onmessage before the next slice runs.
            pumpChannel.port1.postMessage(0);
            return;
        }
        pumpTimer = setTimeout(() => { pumpTimer = null; pump(); }, ms);
    };
    // Deep-idle sleep state: consecutive quiet deep-idle pumps, and the
    // pending sleep timer (null while pumping or self-posted).
    let idleStreak = 0;
    let pumpTimer = null;
    // Whether the engine's deep-idle handback is currently enabled; toggled
    // off during network activity (see the pump), on when traffic goes quiet.
    let handbackOn = true;
    const pump = () => {
        // performance.now() is milliseconds with sub-microsecond resolution;
        // only differences are used, so any monotonic source would do.
        if (restoreSetupPending) _setupPumps++;
        const netActive = performance.now() - lastNetMs < NET_ACTIVE_MS;
        // The realtime clock stays on during network traffic — protocol
        // timeouts NEED honest time: under a fast-forwarded clock the
        // resolver's 5s window burns in ~5ms of real time and every first
        // lookup dies with "DNS: transient error" before the DoH answer
        // (100-300ms real) can arrive. Naps are safe here because RX frames
        // notify the futex and wake the worker instantly.
        const realtime = realtimeWanted && !coldSetupPending && !restoreSetupPending;
        if (realtime) vm.set_host_ns(performance.now() * 1e6);
        else vm.set_host_ns(0);
        // What network traffic DOES disable is the deep-idle handback: a
        // guest waiting on a TCP segment parks against its own protocol
        // TIMEOUT (apk's 10s fetch timer, the resolver's 5s), and the
        // handback's uncapped jump lands the clock exactly on it — apk
        // reported "Operation timed out" mid-download while the data was
        // milliseconds away. The handback only matters on the virtual-clock
        // paths (?turbo and the boot phases), where capped hops let real
        // replies win the race, as they always did.
        const wantHandback = !netActive;
        if (wantHandback !== handbackOn && vm.set_idle_handback) {
            vm.set_idle_handback(wantHandback);
            handbackOn = wantHandback;
        }
        const tPump = performance.now();
        if (profPumpEnd) prof.idle += tPump - profPumpEnd;
        prof.pumps++;
        const accounted0 = prof.run + prof.build + prof.compile + prof.instantiate;
        // Anything this pump does that means the guest (or the JIT) is active.
        // Console bytes, TX frames and freshly linked blocks all count; a pump
        // where all three are zero AND the engine reported a deep-idle park is
        // the only state allowed to sleep.
        let activity = 0;
        vm.run(SLICE);
        prof.run += performance.now() - tPump;
        if (jitOn) activity += jitPump(vm);

        // Satisfy any on-demand 9p faults the guest raised this slice. Fired,
        // not awaited: the completion lands a few pumps later via p9_supply,
        // which is what unblocks the guest's read/readdir.
        if (lazy9p) {
            serve9pFaults(vm);
            if (p9write) drain9pChanges(vm);
        }

        const out = vm.console();
        activity += out.length;
        if (out.length) {
            // The last line the setup script prints; once it shows, the guest is
            // at an interactive prompt. One-shot, so it times the whole boot.
            if (!_interactiveMarked) {
                _bootTail = (_bootTail + String.fromCharCode(...out.subarray(0, 512))).slice(-400);
                if (_bootTail.includes("job control enabled")) {
                    _interactiveMarked = true;
                    _bootTail = "";
                    restoreSetupPending = false; // setup done — real time can resume
                    const dSteps = Math.round((Number(vm.steps()) - _setupBaseSteps) / 1e6);
                    const dBlocks = (vm.jit_installed_count ? vm.jit_installed_count() : 0) - _setupBaseBlocks;
                    bmark(`setup: ${_setupPumps} pumps, ${dBlocks} jit blocks, ${dSteps}M steps`);
                    if (jitCacheOn) console.log(`[jit] cache stats: ${JSON.stringify(jitCacheStats())}`);
                    bmark("INTERACTIVE (setup done)");
                }
            }
            // A cold boot's setup waits for the rescue shell to exist. The
            // prompt is the signal that something is finally reading stdin;
            // typed before it, the commands go into the kernel's console with
            // nothing to run them, which is precisely how a cold-booted guest
            // ended up with no network and no persistence.
            if (coldSetupPending) {
                coldTail = (coldTail + String.fromCharCode(...out.subarray(0, 512))).slice(-256);
                if (coldTail.includes(COLD_PROMPT)) {
                    coldSetupPending = false;
                    coldTail = "";
                    // Cache the machine BEFORE the setup script runs, while the
                    // disk is still unmounted. A snapshot taken with it mounted
                    // also captures the guest's page cache — superblock,
                    // bitmaps, inode tables — describing a filesystem the OPFS
                    // file will not match on the next restore. That is the same
                    // reason the shipped snapshot mounts after resuming rather
                    // than before saving.
                    //
                    // And never cache a machine that came up without a disk at
                    // all. virtio-mmio has no hotplug, so a snapshot taken with
                    // no block device can never gain one — restoring it would
                    // hand back a diskless guest for that RAM size forever,
                    // long after whatever briefly held the storage handle had
                    // gone. One slow boot beats a permanently poisoned cache.
                    if (coldMb && coldHadDisk) {
                        const snap = vm.save();
                        if (snap) cacheSnapshot(coldMb, snap);
                    }
                    vm.input(new TextEncoder().encode(setupCommands(false))); // cold: full setup
                    if (winsize) postMessage({ ttyApplied: winsize });
                }
            }
            postMessage({ console: out }, [out.buffer]);
        }

        // Frames arrive length-prefixed in one buffer; split here so the main
        // thread's adapter sees plain Ethernet frames.
        const tx = vm.net_take();
        for (let o = 0; o + 2 <= tx.length;) {
            const len = (tx[o] << 8) | tx[o + 1];
            o += 2;
            if (o + len > tx.length) break;
            postMessage({ tx: tx.slice(o, o + len) });
            activity++;
            lastNetMs = performance.now();
            o += len;
        }

        const now = Date.now();
        if (now - lastStats > 1000) {
            lastStats = now;
            // Mirror guest writes to OPFS when the share changed. Polled at the
            // stats cadence rather than per instruction: the counter is one
            // integer read, and a second of lag before a file appears in OPFS
            // is invisible next to the cost of rewriting it. flushShareToOpfs
            // guards against overlapping runs, so a slow flush cannot pile up.
            if (!lazy9p) {
                const d = vm.p9_dirty();
                if (d !== p9LastDirty) {
                    p9LastDirty = d;
                    flushShareToOpfs(vm);
                }
            }
            // steps() is a u64 and arrives as BigInt; arithmetic on the page
            // side wants a plain number, and 2^53 steps is not a real risk.
            //
            // jit_stats is [entries, chains, insnsInCompiled, rejected]. Sent
            // as the raw counters rather than pre-digested percentages: this is
            // the only place the numbers exist, and the page decides what is
            // worth showing.
            const js = jitOn ? Array.from(vm.jit_stats()) : null;
            postMessage({
                stats: {
                    steps: Number(vm.steps()),
                    // Guest mtime in ms (10 MHz timebase): how fast guest
                    // time itself is advancing — the number that exposes a
                    // stalled or crawling virtual clock.
                    mtimeMs: vm.mtime ? vm.mtime() / 10_000 : 0,
                    // Instructions the engine attributes to the idle cycle
                    // (WFI -> clock jump -> timer tick -> WFI). The page
                    // subtracts these so MIPS means work, not wakefulness.
                    idleInsns: vm.prof_idle_insns ? vm.prof_idle_insns() : 0,
                    blocks: jitOn ? vm.jit_installed_count() : 0,
                    resets: jitResets,
                    compiled: js ? js[2] : 0,
                    chains: js ? js[1] : 0,
                },
                // Session profile: worker time split plus the engine's own
                // cumulative diagnostic counters. Arrays go raw; the page
                // labels them (same bin orders as the node harness).
                prof: {
                    ...prof,
                    resets: prof.resets.slice(-8),
                    entries: js ? js[0] : 0,
                    rejected: js ? js[3] : 0,
                    chainMiss: jitOn && vm.chain_miss ? Array.from(vm.chain_miss()) : null,
                    tlbMiss: jitOn && vm.tlb_miss ? Array.from(vm.tlb_miss()) : null,
                    interp: jitOn && vm.interp_hist ? Array.from(vm.interp_hist()) : null,
                    priv: vm.prof_priv ? Array.from(vm.prof_priv()) : null,
                    // Why the translation generation moved: [satp write,
                    // sfence.vma global, sfence.vma one page, other]. Each
                    // global bump voids ALL block chaining, so this is the
                    // denominator behind the gentrans chain stops.
                    genBump: vm.gen_bump ? Array.from(vm.gen_bump()) : null,
                },
            });
        }
        // Yield rather than a tight loop: onmessage must get a turn, or input
        // and RX frames would starve exactly like the RX storm did.
        //
        // When the guest is idle, sleep for as long as it is actually waiting
        // rather than coming straight back. This is what stops a page sitting
        // at a prompt from burning a core, and it is bounded: a keystroke or an
        // RX frame arrives as a message, which wakes this worker anyway, so
        // sleeping cannot delay input. Capped so a long guest sleep still lets
        // the stats panel and the 9p flush poll run.
        const idle =
            realtime && vm.idle_ms
                ? vm.idle_ms()
                : 0;
        // The fast path yields via a MessageChannel self-post, not setTimeout(0):
        // browsers clamp nested setTimeout(0) to 4ms, capping the pump at ~250/s
        // and wasting ~4ms per slice. Message tasks are not clamped, so
        // back-to-back slices run at full engine speed. A keystroke or RX frame
        // still arrives as a message and wakes this worker.
        profPumpEnd = performance.now();
        // Whatever this pump spent that run/build/compile/instantiate did not
        // claim: console, net frames, 9p service, stats, snapshot writes.
        const accounted = prof.run + prof.build + prof.compile + prof.instantiate;
        prof.io += Math.max(0, profPumpEnd - tPump - (accounted - accounted0));

        // Deep-idle sleep. The engine only sets idle_until (read via idle_ms)
        // when the GUEST KERNEL parked with nothing runnable, its next
        // deadline >= 50ms of guest time away, and no device interrupt pending
        // after the completion flush — so nothing here can starve real work by
        // construction. The gates on top: never during boot/setup phases, and
        // never on a pump that produced output, frames, or compiled blocks.
        // Exponential backoff (4 -> 50ms) keeps guest sleep/retry scripts
        // cheap while a genuinely idle prompt converges to ~20 wakes/s.
        // Input, RX frames and 9p replies cancel the timer via wakePump, so
        // their latency is unchanged. The engine reports each park ONCE; if we
        // decline to sleep, the next pump jumps the clock exactly as before.
        // Deep-idle backoff naps apply only in turbo mode, and never during a
        // transfer: while net traffic is recent the pump must spin so the
        // guest's TCP timers fast-forward the way they always did (the engine
        // still hands parks back, and the immediate re-entry jumps the clock).
        const deepIdle =
            !realtimeWanted && !coldSetupPending && !restoreSetupPending && !netActive
                && vm.idle_ms
                ? vm.idle_ms()
                : 0;
        if (idle > 0 && !netActive) {
            // Realtime nap: the engine says the guest's deadline is `idle` ms
            // of real time away. Woken early by input/RX/9p replies either
            // way (futex notify or wakePump) — latency unchanged.
            //
            // Suppressed while netActive: a guest waiting mid-transfer parks
            // exactly like an idle one (far TCP-timer deadline, reply not yet
            // arrived), so this path would nap through a download and pace the
            // engine down — the "engine sleeps mid-apk" bug. The realtime clock
            // stays ON above (DNS/protocol timeouts keep honest time); we only
            // decline to SLEEP, spinning at full engine speed so each RX/TX is
            // serviced with zero nap latency until traffic goes quiet (1s).
            prof.sleeps++;
            nap(Math.min(idle, 50));
        } else if (deepIdle > 0 && activity === 0) {
            idleStreak++;
            const quantum = Math.min(4 << Math.min(idleStreak - 1, 4), 50);
            prof.sleeps++;
            nap(quantum);
        } else {
            idleStreak = 0;
            pumpChannel.port1.postMessage(0);
        }
    };
    // The channel exists before the first pump runs. Declared after `pump` is
    // defined, so the handler assignment is not a temporal-dead-zone error.
    const pumpChannel = new MessageChannel();
    pumpChannel.port2.onmessage = pump;
    // Arm the wake path: cancel a pending deep-idle sleep and pump now. A
    // no-op while the pump is running or self-posted (worker is single-
    // threaded, so pumpTimer is only non-null while genuinely asleep).
    wakePump = () => {
        if (pumpTimer !== null) {
            clearTimeout(pumpTimer);
            pumpTimer = null;
            idleStreak = 0;
            pumpChannel.port1.postMessage(0);
        }
    };
    pump();
}

boot().catch(err => postMessage({ status: `FAILED: ${err.message}` }));
