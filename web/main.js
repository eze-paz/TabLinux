// Page glue: a real terminal on one side, the network adapter on the other.

import { RiscvNetAdapter } from "./net-adapter.js";
import { WispEgress } from "./wisp-egress.js";
import { Terminal } from "./term.js";
import { createSyncCore } from "./sync-core.js";

const $ = id => document.getElementById(id);
const dot = $("dot"), statusEl = $("status"), netinfo = $("netinfo"), stats = $("stats");

/// 1234567 -> "1.2M". Instruction counts reach the billions and the exact
/// digits carry no information at a glance.
const compact = n =>
    n >= 1e9 ? `${(n / 1e9).toFixed(2)}G`
        : n >= 1e6 ? `${(n / 1e6).toFixed(1)}M`
            : n >= 1e3 ? `${(n / 1e3).toFixed(1)}k`
                : String(Math.round(n));

/// Elapsed milliseconds as h/m/s.
function duration(ms) {
    const s = Math.floor(ms / 1000);
    if (s < 60) return `${s}s`;
    if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
    return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

// ── Machine controls ───────────────────────────────────────────────────────
// Both are URL state rather than page state, because both are decided while
// the machine is being built: RAM is fixed at construction and the JIT is
// enabled before the run loop starts. Changing either reloads, which is honest
// about what is happening — you are getting a new machine, not reconfiguring
// this one.

/// Offered sizes, in MiB. 256 runs a shell and apk comfortably; the larger
/// ones are for compiling in the guest. Guest RAM is one flat allocation held
/// for the life of the tab whether or not the guest touches it.
// Guest RAM is fixed at 256 MiB — the size the shipped snapshot was taken at,
// so the machine always resumes fast rather than cold booting. The JIT is
// always on. Neither is user-selectable.

// ?debug=1 turns on packet-level logging to the console. The interesting
// failures only reproduce on the deployed origin, where attaching a debugger
// is not something I can do from here — so the page has to be able to explain
// itself from the outside.
// The page's query string rides on the worker URL so the worker can read
// flags (?turbo, ?nofutex) from self.location — a Worker cannot see the
// page's URL.
const worker = new Worker("./vm-worker.js" + location.search, { type: "module" });

// Futex the worker naps on (COI required for SharedArrayBuffer; production
// sets COOP/COEP globally). Every message we send bumps and notifies it, so a
// worker blocked in Atomics.wait wakes in microseconds on input or RX — the
// blocking nap costs keystrokes nothing. Without COI the worker falls back to
// setTimeout naps and this stays null.
const wakeSab = (typeof SharedArrayBuffer !== "undefined" && crossOriginIsolated)
    ? new SharedArrayBuffer(4) : null;
const wakeInt = wakeSab ? new Int32Array(wakeSab) : null;
if (wakeSab) worker.postMessage({ wake: wakeSab });
const post = (msg, transfer) => {
    worker.postMessage(msg, transfer);
    if (wakeInt) {
        Atomics.add(wakeInt, 0, 1);
        Atomics.notify(wakeInt, 0);
    }
};

// Hand the lazy-9p worker the dehydrated-Dropbox cloud index. It lives in this
// origin's localStorage, which the page can read but a Worker cannot, and it is
// how cloud-only placeholders (files present in Dropbox but not yet in OPFS)
// appear in the guest's directory listings. Same key sandpie's own sync uses.
// Harmless when absent (no Dropbox / not dehydrated) — the worker just sees
// null and lists local entries only.
try {
    const idx = JSON.parse(localStorage.getItem("dbxfull-cloud-index") || "null");
    if (idx) post({ p9index: idx });
} catch (_) {}

// Terminate before the page goes away so the Worker drops its exclusive OPFS
// sync access handle promptly; the next load otherwise races the old one and
// can come up with no disk.
addEventListener("pagehide", () => worker.terminate());

const term = new Terminal($("term"), {
    cols: 100,
    rows: 30,
    onInput: bytes => post({ input: bytes }, [bytes.buffer]),
});
term.focus();

// ── Fitting the terminal to the window ─────────────────────────────────────
// The grid is fixed-width by construction — that is what keeps cursor
// addressing honest — so "responsive" here means recomputing how many cells
// fit and rebuilding, not letting anything reflow.

/// Width and height of one character cell, measured rather than assumed: it
/// depends on the font the browser actually resolved, which varies by platform
/// and is not knowable from the CSS.
function cellSize() {
    const probe = document.createElement("div");
    probe.className = "row";
    probe.style.cssText = "position:absolute;visibility:hidden;white-space:pre;left:-9999px";
    probe.textContent = "M".repeat(100);
    $("term").append(probe);
    const r = probe.getBoundingClientRect();
    probe.remove();
    // Fall back to something sane if the font has not loaded yet and the
    // measurement comes back as zero; a resize will correct it.
    return { w: r.width / 100 || 8.4, h: r.height || 17.5 };
}

/// The size the guest has been told, "" until it has been told anything.
let sizeSent = "";
/// True once the worker has typed the boot setup and applied the tty size; only
/// then may a resize type stty, or it collides with the running setup script.
let ttyInteractive = false;
/// The size the guest still needs to be told, or null when it is up to date.
let sizeWanted = null;
/// When console output last arrived. `stty` is typed at the guest's stdin, so
/// it only lands if something is reading — see sendStty.
let lastOutputAt = 0;

function fit() {
    const wrap = $("wrap");
    // Do nothing unless the page is really laid out and on screen.
    //
    // A hidden or not-yet-laid-out page reports a box that is just its own
    // padding, and clamping that to the 20x4 minimum is worse than doing
    // nothing: it shrinks the terminal to something absurd and then tells the
    // guest to match. The guest obliges — measured it setting itself to 4x20 —
    // and every program that sizes output to the terminal is wrong from then
    // on. A backgrounded tab must not be able to do that to a running guest.
    //
    // 240x120 is well below any usable window and well above a box that is
    // only padding.
    //
    // Keyed on the box rather than on visibilityState, which sounds like the
    // right signal and is not: a hidden page already reports a degenerate box,
    // so the size check covers that case, while some perfectly usable embedded
    // contexts report hidden and would be locked out for no reason. Measured
    // one — a page with a real 1200x867 layout reporting visibilityState
    // "hidden" throughout.
    if (wrap.clientWidth < 240 || wrap.clientHeight < 120) return;

    const cs = getComputedStyle(wrap);
    const padX = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight);
    const padY = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom);
    const { w, h } = cellSize();
    if (!(w > 0) || !(h > 0)) return;

    const cols = Math.max(20, Math.floor((wrap.clientWidth - padX) / w));
    const rows = Math.max(4, Math.floor((wrap.clientHeight - padY) / h));
    term.resize(cols, rows);
    $("p-size").textContent = `${cols} x ${rows}`;

    const want = `${rows} ${cols}`;
    if (want !== sizeSent) sizeWanted = { rows, cols, want };
    // Also give it to the worker, which applies it to the tty from the boot
    // script — the only delivery that does not depend on guessing what is
    // reading stdin. Harmless if the machine is already up; the typed path
    // below covers that case.
    post({ winsize: { rows, cols } });
}

/**
 * Tell the guest its size, once something is listening.
 *
 * There is no in-band way to do this. A real terminal carries the size out of
 * band, through the pty; a serial console has no such channel, so the size can
 * only be set by running `stty` in the guest — which means typing it at
 * whatever is reading stdin at that moment.
 *
 * So the timing is the whole problem. Sent too early it disappears into the
 * boot script, and the guest is left with an unset winsize; programs that size
 * their output to the terminal then compute a width from nothing. That is what
 * turned `apk`'s progress bar into a screenful of wrapped hashes: the bar was
 * drawn far wider than the terminal, so every carriage return landed on a
 * wrapped line instead of the start of the bar.
 *
 * Waiting for quiet is the closest thing to a signal available: the guest has
 * stopped printing, which means the boot script has finished and a shell is
 * sitting at a prompt. Not provable, but it is retried until the size sticks
 * rather than assumed to have worked.
 */
const QUIET_MS = 1200;

function sendStty() {
    if (!sizeWanted || !t0) return;
    // Not until the worker says setup is done. The setup script sets the size
    // itself (stty -F /dev/ttyS0) and launches the shell; typing another stty
    // before that finishes drops it into the middle of the still-running boot
    // script — which is what produced "ty rows ... sh: ty: not found" and, worse,
    // could corrupt a setup command into an infinite loop. Later user resizes
    // arrive after this and type cleanly at the prompt.
    if (!ttyInteractive) return;
    if (Date.now() - lastOutputAt < QUIET_MS) return;
    const { rows, cols, want } = sizeWanted;
    sizeSent = want;
    sizeWanted = null;
    const cmd = new TextEncoder().encode(`stty rows ${rows} cols ${cols}\r`);
    post({ input: cmd }, [cmd.buffer]);
}

// Three triggers, because none of them alone is reliable.
//
// The direct call is what actually sizes the terminal at startup: a
// ResizeObserver's first callback is delivered through the rendering
// lifecycle, so a page that is not compositing — a background tab, a hidden
// pane — never gets it, and the terminal would sit at its constructed size
// forever. Observed exactly that while testing.
//
// The observer then catches the changes `resize` cannot see: the panel wraps
// underneath on a narrow screen and the bar can grow a line, both of which
// change the terminal's box while the window stays put.
//
// And fonts: metrics measured before the monospace face resolves are the
// fallback's, not the real ones, so re-fit once it settles.
// After layout, not during module evaluation: at this point the page has not
// been laid out, so #wrap measures zero and fit would have nothing to work
// with. rAF is the first moment the box is real.
// A mobile soft keyboard opening fires a burst of `resize` events (Android
// resizes the layout viewport). Running fit()->paint() synchronously in the
// middle of the keyboard's open animation reflows the page and the browser
// dismisses the keyboard the instant it appears — it flashes and closes. So
// coalesce resize-driven fits, and while the input is focused (keyboard up)
// ignore a change that is height-only: that is the keyboard itself, and
// repainting for it is exactly what closes it. A width change is a real
// rotation/resize and still refits.
let fitTimer = 0;
let lastFitW = 0;
function keyboardHeightOnly(w) {
    return document.activeElement === $("kbd") && w === lastFitW;
}
function fitSoon() {
    if (keyboardHeightOnly($("wrap").clientWidth)) return;
    clearTimeout(fitTimer);
    fitTimer = setTimeout(() => {
        lastFitW = $("wrap").clientWidth;
        fit();
    }, 150);
}

requestAnimationFrame(() => { lastFitW = $("wrap").clientWidth; fit(); });
new ResizeObserver(fitSoon).observe($("wrap"));
addEventListener("resize", fitSoon);
document.fonts?.ready.then(fit);

/// Last box `fit` was run for, so the periodic check below can tell whether
/// anything actually moved.
let fitBox = "";

/// Re-fit if the terminal's box changed, called from the once-a-second stats
/// tick.
///
/// Belt and braces over the observer, and not paranoia: both the observer and
/// the resize event are delivered through the rendering lifecycle, and a
/// throttled or non-compositing page gets neither. Measured that directly —
/// a hand-rolled ResizeObserver on the same element fired zero times across a
/// real viewport change. Two cheap layout reads, and it only does work when
/// the numbers differ.
function fitIfMoved() {
    const wrap = $("wrap");
    const box = `${wrap.clientWidth}x${wrap.clientHeight}`;
    if (box === fitBox) return;
    // Don't let the poll refit for the keyboard opening either (height-only
    // change while the input is focused) — same dismissal as the resize path.
    if (keyboardHeightOnly(wrap.clientWidth)) return;
    fitBox = box;
    lastFitW = wrap.clientWidth;
    fit();
}

// fake_network.js does the thinking; this just moves its answers into the VM.
const net = new RiscvNetAdapter(frame => post({ rx: frame }, [frame.buffer]));

// Egress. Without a relay fake_network answers ARP/DHCP/ICMP/DNS but resets
// every TCP connection, so `apk` cannot work. Default: this origin's /wisp —
// the only arrangement where the relay's SSO gate can see the session cookie.
// `?wisp=wss://host/path` overrides it with an explicit relay (e.g. a public
// one on GitHub Pages, where no /wisp endpoint exists); `?wisp=1` forces the
// same-origin /wisp.
{
    const params = new URLSearchParams(location.search);
    const wispParam = params.get("wisp");
    const url = (wispParam && wispParam !== "1")
        ? wispParam
        : `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/wisp`;
    const wisp = new WispEgress(url, {
        onStatus: s => {
            netinfo.textContent = s.replace(/^wisp: /, "");
            $("p-net").textContent = s.replace(/^wisp: /, "");
            dot.classList.toggle("err", s.includes("error") || s.includes("disconnect"));
        },
    });
    wisp.resolveName = ip => net.hostnameFor(ip);
    net.on_tcp_connection = (conn, packet) => wisp.onTcpConnection(conn, packet);
    // The browser cannot emit ICMP, but the relay can, and every other packet
    // already goes through it -- so ping asks the relay whether *it* can reach
    // the address, and the guest's own request comes back as the reply.
    net.on_icmp_echo = frame => wisp.sendEcho(frame, () => net.deliverEchoReply(frame));
}

let steps0 = null, t0 = null;
// For the instantaneous rate: the counter and clock at the last sample.
let lastSteps = 0, lastStepsAt = null, nowMips = 0;
// Idle-cycle instruction counter, same windowing as the raw counter. Busy
// MIPS = (steps - idle) / wall: under the virtual clock an idle guest spins
// its timer loop at full speed, so RAW MIPS reads highest when the machine is
// doing nothing — exactly inverted from what a throughput number should do.
let idle0 = null, lastIdle = 0, nowBusy = 0;

// ── Lazy-9p write-back ──────────────────────────────────────────────────────
// The worker relays each guest mutation here; the page owns OPFS writes (so the
// sync engine and the worker's reads see one coherent tree) and hands the change
// to sync-core, which propagates it to Dropbox via the same file:changed /
// file:deleted path the app's own edits use.
async function opfsWrite(path, data) {
    const parts = path.split("/").filter(Boolean);
    let dir = await navigator.storage.getDirectory();
    for (const seg of parts.slice(0, -1)) dir = await dir.getDirectoryHandle(seg, { create: true });
    const fh = await dir.getFileHandle(parts[parts.length - 1], { create: true });
    const w = await fh.createWritable();
    await w.write(data);
    await w.close();
}
async function opfsRemove(path) {
    const parts = path.split("/").filter(Boolean);
    let dir = await navigator.storage.getDirectory();
    for (const seg of parts.slice(0, -1)) dir = await dir.getDirectoryHandle(seg);
    await dir.removeEntry(parts[parts.length - 1], { recursive: true });
}
async function opfsMkdir(path) {
    let dir = await navigator.storage.getDirectory();
    for (const seg of path.split("/").filter(Boolean)) dir = await dir.getDirectoryHandle(seg, { create: true });
}
function notifySync(kind, path) {
    // Bridge installed by sync-core when it is loaded on this page. Absent =>
    // OPFS is updated but nothing is pushed to Dropbox (e.g. Dropbox not set up).
    try {
        if (window.__spSyncNotify) window.__spSyncNotify(kind, path);
    } catch (_) {}
}

// Read an OPFS file's bytes / test existence, for sync-core's uploads.
async function opfsFileHandle(rel) {
    const parts = rel.split("/").filter(Boolean);
    let dir = await navigator.storage.getDirectory();
    for (const seg of parts.slice(0, -1)) dir = await dir.getDirectoryHandle(seg);
    return dir.getFileHandle(parts[parts.length - 1]);
}

// Bring up the Dropbox sync engine on this page. It gives the VM two things a
// sandpie app tab used to be required for: pushing guest changes to Dropbox
// (via window.__spSyncNotify, which the write-back path calls) and answering the
// service worker's hydrate requests so cloud-only file *content* faults in on
// read. Only when Dropbox is actually connected — otherwise the VM stays
// OPFS-local.
const _sync = createSyncCore({
    opfs: {
        async read(rel) {
            try {
                const fh = await opfsFileHandle(rel);
                return new Uint8Array(await (await fh.getFile()).arrayBuffer());
            } catch {
                return null;
            }
        },
        write: opfsWrite,
        async exists(rel) {
            try {
                await opfsFileHandle(rel);
                return true;
            } catch {
                return false;
            }
        },
    },
});
if (_sync.connected()) {
    window.__spSyncNotify = (kind, path) => _sync.notify(kind, path);
    _sync.wireServiceWorker();
    console.log("[vm] sync-core active — Dropbox write-back and hydrate enabled");
}
async function applyVmChange(ch) {
    try {
        if (ch.op === 0) {
            await opfsWrite(ch.path, new Uint8Array(ch.data || new ArrayBuffer(0)));
            notifySync("changed", ch.path);
        } else if (ch.op === 1) {
            await opfsRemove(ch.path).catch(() => {});
            notifySync("deleted", ch.path);
        } else if (ch.op === 2) {
            await opfsMkdir(ch.path);
        }
    } catch (err) {
        console.warn("[9p write-back] failed for", ch.path, err);
    }
}

worker.onmessage = e => {
    const m = e.data;
    if (m.p9change) {
        applyVmChange(m.p9change);
        return;
    }
    if (m.console) {
        lastOutputAt = Date.now();
        term.write(m.console);
    }
    if (m.tx) net.send(m.tx);
    // The machine reports the size it actually came up at (always 256 MiB now).
    if (m.ram) {
        $("p-ram").textContent = m.ram >= 1024 ? `${m.ram / 1024} GiB` : `${m.ram} MiB`;
    }
    // The boot script already set this size on the tty, so there is nothing to
    // type. Only a later change needs the typed path, and that is what a
    // mismatch here means.
    if (m.ttyApplied) {
        const applied = `${m.ttyApplied.rows} ${m.ttyApplied.cols}`;
        sizeSent = applied;
        if (sizeWanted?.want === applied) sizeWanted = null;
        // Setup has been typed and the size applied on the tty; from here a
        // resize may safely type stty at the prompt.
        ttyInteractive = true;
    }
    if (m.disk) $("p-disk").textContent = m.disk;
    if (m.jit !== undefined) {
        $("p-jit").textContent = m.jit ? "on" : "interpreter";
    }
    if (m.status) {
        statusEl.textContent = m.status;
        dot.classList.toggle("on", m.status === "running");
        dot.classList.toggle("err", m.status.startsWith("FAILED"));
        if (m.status === "running") t0 = Date.now();
    }
    if (m.stats) {
        fitIfMoved();
        sendStty();
        // A restored machine inherits the snapshot's step counter, so measure
        // only what this page executed.
        if (steps0 === null) steps0 = m.stats.steps;
        const now = Date.now();
        const done = m.stats.steps - steps0;

        const idleNow = m.stats.idleInsns || 0;
        if (idle0 === null) idle0 = idleNow;

        // Instantaneous first. A cumulative average can only fall once the
        // fast early phase is over, whatever the emulator is actually doing,
        // which makes it exactly the wrong thing to watch while working.
        if (lastStepsAt === null || now - lastStepsAt >= 1000) {
            if (lastStepsAt !== null) {
                const dt = (now - lastStepsAt) / 1000;
                const d = m.stats.steps - lastSteps;
                nowMips = d / 1e6 / dt;
                nowBusy = Math.max(0, d - (idleNow - lastIdle)) / 1e6 / dt;
            }
            lastSteps = m.stats.steps;
            lastIdle = idleNow;
            lastStepsAt = now;
        }
        const doneBusy = Math.max(0, done - (idleNow - idle0));
        const avg = t0 ? doneBusy / 1e6 / ((now - t0) / 1000) : 0;
        stats.textContent =
            `${nowBusy.toFixed(1)} MIPS busy (raw ${nowMips.toFixed(1)}) · tx ${net.stats.tx} rx ${net.stats.rx}`;

        // The headline is BUSY MIPS — instructions doing something, per wall
        // second. The raw counter (which counts the idle loop's spin too, and
        // therefore peaks when the machine is idlest) is the small row below.
        $("p-mips").textContent = nowBusy.toFixed(1);
        // Scaled against a ceiling a little above what this engine reaches, so
        // a healthy run sits high without pinning and a stall is obvious.
        $("p-meter").style.width = `${Math.min(100, nowBusy / 1.4)}%`;
        $("p-raw").textContent = `${nowMips.toFixed(1)} MIPS`;
        $("p-avg").textContent = `${avg.toFixed(1)} MIPS`;
        $("p-idleshare").textContent = done > 0
            ? `${(100 * (idleNow - idle0) / done).toFixed(1)}%` : "—";
        $("p-steps").textContent = compact(done);
        $("p-up").textContent = t0 ? duration(now - t0) : "—";
        $("p-frames").textContent = `${net.stats.tx} / ${net.stats.rx}`;

        if (m.stats.blocks !== undefined) {
            // Blocks currently held, and how many times the cache has been
            // thrown away. A reset or two during a cold boot is routine — a
            // whole kernel gets compiled — so this is information, not alarm.
            $("p-blocks").textContent = m.stats.resets
                ? `${compact(m.stats.blocks)} (${m.stats.resets} reset${m.stats.resets > 1 ? "s" : ""})`
                : compact(m.stats.blocks);
            // Share of retired instructions that ran as compiled code. The
            // counters are cumulative and survive a cache discard, so this is
            // for the whole session rather than the last second.
            const cov = m.stats.steps ? (100 * m.stats.compiled / m.stats.steps) : 0;
            $("p-cov").textContent = m.stats.compiled ? `${cov.toFixed(1)}%` : "—";
        }
        // Chrome only, and coarse-grained — but this is the number that goes
        // wrong when the code cache misbehaves, so it is worth a line.
        const mem = performance.memory;
        $("p-heap").textContent = mem ? `${(mem.usedJSHeapSize / 1048576).toFixed(0)} MiB` : "n/a";
    }
    if (m.prof) renderProf(m.prof, m.stats);
};

// ---- session profile panel -------------------------------------------------

let lastProf = null;

/// The percentages are of the worker's total wall time since the machine
/// started (run + compile pipeline + io + idle), so they answer "where did
/// this session actually go" rather than "how busy was the last second".
function renderProf(p, stats) {
    lastProf = p;
    // Console-accessible for debugging sessions; not rendered.
    window.__prof = p;
    window.__stats = stats;
    const pipeline = p.build + p.compile + p.instantiate;
    const total = p.run + pipeline + p.io + p.idle;
    if (!total) return;
    const pct = (x) => `${(100 * x / total).toFixed(1)}%`;
    $("pr-run").textContent = pct(p.run);
    $("pr-compile").textContent =
        `${pct(pipeline)} (gen ${(p.build / 1000).toFixed(1)}s · v8 ${(p.compile / 1000).toFixed(1)}s · link ${(p.instantiate / 1000).toFixed(1)}s)`;
    $("pr-io").textContent = pct(p.io);
    $("pr-idle").textContent = pct(p.idle);

    // Kernel vs user share of retired instructions. This is the number that
    // says whether a slow session is the workload or the kernel serving it
    // (reclaim under a small RAM cap, fs, interrupts).
    if (p.priv) {
        const [u, s, , mm] = p.priv;
        const t = u + s + mm;
        if (t) $("pr-priv").textContent = `${(100 * (s + mm) / t).toFixed(1)}% / ${(100 * u / t).toFixed(1)}%`;
    }

    // Interpreted share of retired instructions, and what it consists of.
    // Bin order matches riscv_machine::INTERP_BINS.
    if (p.interp && stats) {
        const NAMES = ["mul", "div", "atomic", "csr", "fence", "system", "fp",
                       "cold", "other", "fence.i"];
        const it = p.interp.reduce((a, c) => a + c, 0);
        $("pr-interp").textContent = stats.steps ? `${(100 * it / stats.steps).toFixed(2)}%` : "—";
        const top = p.interp.map((v, i) => [NAMES[i], v]).sort((a, b) => b[1] - a[1])[0];
        if (top && top[1]) $("pr-itop").textContent = `${top[0]} ${(100 * top[1] / (it || 1)).toFixed(0)}%`;
    }

    // Why compiled chains returned to the host. Bin order matches
    // riscv_machine::CHAIN_MISS_BINS; "noblock" is a sub-count of "budget".
    if (p.chainMiss) {
        const NAMES = ["empty", "evict", "genpriv", "gentrans", "genboth",
                       "budget", "noblock", "fault", "cap"];
        const top = p.chainMiss.slice(0, 6).concat(p.chainMiss.slice(7))
            .map((v, i) => [NAMES[i < 6 ? i : i + 1], v])
            .sort((a, b) => b[1] - a[1]).slice(0, 2);
        $("pr-chain").textContent = top.map(([n, v]) => `${n} ${compact(v)}`).join(" · ");
    }

    // What moved the translation generation. Each satp/global-sfence bump
    // voids all block chaining, so the dominant bin names the fix (ASIDs vs
    // honoring sfence operands vs allocator behavior).
    if (p.genBump) {
        const NAMES = ["satp", "sfence-all", "sfence-page", "other"];
        $("pr-gen").textContent = p.genBump
            .map((v, i) => [NAMES[i], v]).filter(([, v]) => v > 0)
            .map(([n, v]) => `${n} ${compact(v)}`).join(" · ") || "none";
    }

    $("pr-cvol").textContent =
        `${compact(p.compiles)} mod · ${(p.compiledBytes / 1048576).toFixed(1)} MiB` +
        (p.cacheHits ? ` · ${compact(p.cacheHits)} cached` : "");
    $("pr-resets").textContent = p.resets.length
        ? p.resets.map(r => `@${r.at}s (${compact(r.blocks)})`).join(", ")
        : "none";
}

$("pr-copy").onclick = (e) => {
    e.preventDefault();
    if (!lastProf) return;
    navigator.clipboard.writeText(JSON.stringify(lastProf, null, 1)).then(() => {
        $("pr-copied").textContent = "copied";
        setTimeout(() => { $("pr-copied").textContent = ""; }, 1500);
    });
};
