// Content-addressed cache of compiled JIT block modules, persisted in IndexedDB
// so a block compiled in one session is reused — WITHOUT recompilation — in the
// next. This is the "AOT" win: skipping V8's WebAssembly.Module compilation
// across sessions, and (for repeated code) running JIT-compiled from the start.
//
// Correct BY CONSTRUCTION: the key is a hash of the module's OWN wasm bytes, so
// a hit returns exactly the compilation of those exact bytes. Different guest
// code -> different blocks -> different bytes -> different key -> automatic miss
// and a fresh compile. There is no staleness to invalidate. The relocatable
// codegen (table_base is an imported global, not a baked const) makes the bytes
// base-independent, which is the reason the same blocks hit across sessions.
//
// Budget + LRU, because the store shares the origin's quota with the user's
// OPFS/Dropbox data. ENGINE_ID clears the store when the codegen changes (old
// entries would just never hit, but this reclaims their space immediately).

const DB_NAME = "vm-jitcache";
const STORE = "modules";
const META = "meta";
// Bump when the JIT codegen changes shape (new imports, different emit). Content
// addressing already prevents *wrong* hits; this reclaims now-dead entries.
//
// reloc-2: group-CSE codegen, and the store now holds BYTES rather than
// compiled Modules. Modern Chrome removed WebAssembly.Module structured
// serialization from IndexedDB, so every Module `put` threw into the
// fire-and-forget catch and the store stayed empty — the console announced it
// on every load ("cache init: entries:0" after sessions that stored hundreds).
// Bytes always serialize; init compiles them in the background instead.
const ENGINE_ID = "reloc-2";
const BUDGET_BYTES = 48 * 1024 * 1024;

// Cross-session persistence, measured 2026-08-13 and turned OFF:
// - With production batching (whatever queued when the pump ran), a warm
//   second session hit 0 of 46 boot modules — batch composition is timing-
//   dependent, so the same blocks land in different modules and every key
//   misses. Persisting+background-compiling ~45 MB of modules per load bought
//   nothing.
// - Deterministic batching (sort by paddr + content-defined cuts) fixed the
//   keys (9/44 hits) but measured 32% SLOWER steady-state: smaller, sorted
//   modules multiply cross-instance tail-calls, which pay V8's instance
//   switch. The chain wants flow-order, single-big-module locality.
// - The profiler bounds the whole prize at ~0.3s of V8 compile per session
//   (1.3% of boot wall) — warmup cost is block FORMATION, which no module
//   cache can skip.
// The in-memory half stays on: within one session identical batches DO recur
// (measured 176/485 during a reset storm), and a Module in a Map is free.
const PERSIST = false;

// MurmurHash3 x86 128-bit. 32-bit ops (fast in JS), 128-bit output as hex — a
// collision would return the wrong module (= silent corruption), so this is
// wide enough that it never realistically happens.
function murmur128(data) {
    const c1 = 0x239b961b, c2 = 0xab0e9789, c3 = 0x38b34ae5, c4 = 0xa1e38b93;
    let h1 = 0, h2 = 0, h3 = 0, h4 = 0;
    const len = data.length;
    const blocks = len & ~15;
    const dv = new DataView(data.buffer, data.byteOffset, data.byteLength);
    const rotl = (x, r) => (x << r) | (x >>> (32 - r));
    const mul = (a, b) => Math.imul(a, b) | 0;
    for (let i = 0; i < blocks; i += 16) {
        let k1 = dv.getUint32(i, true), k2 = dv.getUint32(i + 4, true);
        let k3 = dv.getUint32(i + 8, true), k4 = dv.getUint32(i + 12, true);
        k1 = mul(k1, c1); k1 = rotl(k1, 15); k1 = mul(k1, c2); h1 ^= k1;
        h1 = rotl(h1, 19); h1 = (h1 + h2) | 0; h1 = (mul(h1, 5) + 0x561ccd1b) | 0;
        k2 = mul(k2, c2); k2 = rotl(k2, 16); k2 = mul(k2, c3); h2 ^= k2;
        h2 = rotl(h2, 17); h2 = (h2 + h3) | 0; h2 = (mul(h2, 5) + 0x0bcaa747) | 0;
        k3 = mul(k3, c3); k3 = rotl(k3, 17); k3 = mul(k3, c4); h3 ^= k3;
        h3 = rotl(h3, 15); h3 = (h3 + h4) | 0; h3 = (mul(h3, 5) + 0x96cd1c35) | 0;
        k4 = mul(k4, c4); k4 = rotl(k4, 18); k4 = mul(k4, c1); h4 ^= k4;
        h4 = rotl(h4, 13); h4 = (h4 + h1) | 0; h4 = (mul(h4, 5) + 0x32ac3b17) | 0;
    }
    let k1 = 0, k2 = 0, k3 = 0, k4 = 0;
    const tail = blocks;
    const rem = len & 15;
    const b = data;
    if (rem >= 15) k4 ^= b[tail + 14] << 16;
    if (rem >= 14) k4 ^= b[tail + 13] << 8;
    if (rem >= 13) { k4 ^= b[tail + 12]; k4 = mul(k4, c4); k4 = rotl(k4, 18); k4 = mul(k4, c1); h4 ^= k4; }
    if (rem >= 12) k3 ^= b[tail + 11] << 24;
    if (rem >= 11) k3 ^= b[tail + 10] << 16;
    if (rem >= 10) k3 ^= b[tail + 9] << 8;
    if (rem >= 9) { k3 ^= b[tail + 8]; k3 = mul(k3, c3); k3 = rotl(k3, 17); k3 = mul(k3, c4); h3 ^= k3; }
    if (rem >= 8) k2 ^= b[tail + 7] << 24;
    if (rem >= 7) k2 ^= b[tail + 6] << 16;
    if (rem >= 6) k2 ^= b[tail + 5] << 8;
    if (rem >= 5) { k2 ^= b[tail + 4]; k2 = mul(k2, c2); k2 = rotl(k2, 16); k2 = mul(k2, c3); h2 ^= k2; }
    if (rem >= 4) k1 ^= b[tail + 3] << 24;
    if (rem >= 3) k1 ^= b[tail + 2] << 16;
    if (rem >= 2) k1 ^= b[tail + 1] << 8;
    if (rem >= 1) { k1 ^= b[tail]; k1 = mul(k1, c1); k1 = rotl(k1, 15); k1 = mul(k1, c2); h1 ^= k1; }
    h1 ^= len; h2 ^= len; h3 ^= len; h4 ^= len;
    h1 = (h1 + h2) | 0; h1 = (h1 + h3) | 0; h1 = (h1 + h4) | 0;
    h2 = (h2 + h1) | 0; h3 = (h3 + h1) | 0; h4 = (h4 + h1) | 0;
    const fmix = h => {
        h ^= h >>> 16; h = mul(h, 0x85ebca6b); h ^= h >>> 13;
        h = mul(h, 0xc2b2ae35); h ^= h >>> 16; return h >>> 0;
    };
    h1 = fmix(h1); h2 = fmix(h2); h3 = fmix(h3); h4 = fmix(h4);
    h1 = (h1 + h2) | 0; h1 = (h1 + h3) | 0; h1 = (h1 + h4) | 0;
    h2 = (h2 + h1) | 0; h3 = (h3 + h1) | 0; h4 = (h4 + h1) | 0;
    const hex = x => (x >>> 0).toString(16).padStart(8, "0");
    return hex(h1) + hex(h2) + hex(h3) + hex(h4);
}

export { murmur128 };

// key -> { mod: WebAssembly.Module, size: number, atime: number }
let _map = null;
let _total = 0;
let _seq = 0;
let _db = null;
let _hits = 0;
let _misses = 0;

function _open() {
    return new Promise((resolve, reject) => {
        const r = indexedDB.open(DB_NAME, 1);
        r.onupgradeneeded = () => {
            const db = r.result;
            if (!db.objectStoreNames.contains(STORE)) db.createObjectStore(STORE);
            if (!db.objectStoreNames.contains(META)) db.createObjectStore(META);
        };
        r.onsuccess = () => resolve(r.result);
        r.onerror = () => reject(r.error);
    });
}
function _tx(db, store, mode) {
    return db.transaction(store, mode).objectStore(store);
}
function _req(req) {
    return new Promise((res, rej) => { req.onsuccess = () => res(req.result); req.onerror = () => rej(req.error); });
}

/// Open the store, drop it if the engine changed, and load every cached module
/// into memory so the pump can look up synchronously. Best-effort: any failure
/// leaves the cache disabled (null map) and the JIT compiles fresh as before.
export async function jitCacheInit() {
    if (!PERSIST) {
        // In-memory only. Also clear any store a previous build left behind —
        // it was never going to hit (see PERSIST above) and holds real quota.
        _map = new Map();
        _total = 0;
        try {
            _db = await _open();
            _tx(_db, STORE, "readwrite").clear();
        } catch { _db = null; }
        _db = null;
        return { ok: true, entries: 0, bytes: 0 };
    }
    try {
        _db = await _open();
        const engine = await _req(_tx(_db, META, "readonly").get("engine"));
        if (engine !== ENGINE_ID) {
            await new Promise((res, rej) => {
                const t = _db.transaction([STORE, META], "readwrite");
                t.objectStore(STORE).clear();
                t.objectStore(META).put(ENGINE_ID, "engine");
                t.oncomplete = res; t.onerror = () => rej(t.error);
            });
        }
        _map = new Map();
        _total = 0;
        // Collect the persisted BYTES first, then compile them off this await:
        // boot must not wait on compiling last session's whole working set, and
        // WebAssembly.compile runs on V8's background threads anyway. A lookup
        // that races a still-compiling entry just misses and compiles fresh —
        // same behavior as before the entry existed.
        const persisted = [];
        const store = _tx(_db, STORE, "readonly");
        await new Promise((res, rej) => {
            const cur = store.openCursor();
            cur.onerror = () => rej(cur.error);
            cur.onsuccess = () => {
                const c = cur.result;
                if (!c) return res();
                const v = c.value; // { bytes, size }
                if (v && v.bytes) persisted.push([c.key, v.bytes]);
                c.continue();
            };
        });
        (async () => {
            for (const [key, bytes] of persisted) {
                try {
                    const mod = await WebAssembly.compile(
                        bytes.buffer ? bytes : new Uint8Array(bytes));
                    if (!_map.has(key)) {
                        _map.set(key, { mod, size: bytes.length ?? bytes.byteLength, atime: ++_seq });
                        _total += bytes.length ?? bytes.byteLength;
                    }
                } catch { /* stale/corrupt entry: it will just miss */ }
            }
        })();
        return { ok: true, entries: persisted.length, bytes: persisted.reduce((a, [, b]) => a + (b.length ?? b.byteLength), 0) };
    } catch {
        _map = null;
        return { ok: false };
    }
}

/// Look up the compiled module for these exact wasm bytes, or null on a miss.
/// Synchronous, so the pump stays synchronous.
export function jitCacheGet(bytes) {
    if (!_map) return null;
    const e = _map.get(murmur128(bytes));
    if (!e) { _misses++; return null; }
    e.atime = ++_seq;
    _hits++;
    return e.mod;
}

/// Record a freshly compiled module. The in-memory side keeps the Module (so
/// repeat batches this session skip V8); IndexedDB gets the BYTES — Modules do
/// not survive structured serialization to IDB in modern browsers, and when
/// this stored the Module the store silently stayed empty forever.
export function jitCachePut(bytes, mod) {
    if (!_map) return;
    const key = murmur128(bytes);
    if (_map.has(key)) return;
    const size = bytes.length;
    _map.set(key, { mod, size, atime: ++_seq });
    _total += size;
    if (_db) {
        // Copy: `bytes` is a view into wasm linear memory that jit_build may
        // reuse; IDB serializes lazily enough that aliasing is not worth risking.
        try { _tx(_db, STORE, "readwrite").put({ bytes: bytes.slice(), size }, key); } catch {}
    }
    _evict();
}

function _evict() {
    if (_total <= BUDGET_BYTES) return;
    // Oldest first. Rebuilt rarely (only over budget), so a sort is fine.
    const byAge = [...(_map.entries())].sort((a, b) => a[1].atime - b[1].atime);
    for (const [key, e] of byAge) {
        if (_total <= BUDGET_BYTES) break;
        _map.delete(key);
        _total -= e.size;
        if (_db) { try { _tx(_db, STORE, "readwrite").delete(key); } catch {} }
    }
}

export function jitCacheStats() {
    return { on: !!_map, entries: _map ? _map.size : 0, bytes: _total, hits: _hits, misses: _misses };
}
