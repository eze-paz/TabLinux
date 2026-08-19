// Minimal Dropbox sync engine for the VM page — enough to push the guest's
// file changes to Dropbox and to hydrate cloud-only files on demand, WITHOUT a
// host app tab open.
//
// It reuses the host app's exact transport patterns (modules/dropbox.js):
// direct CORS fetches to Dropbox (no proxy — dbxRoute there is identity), PKCE
// refresh tokens from localStorage, the `\uXXXX` Dropbox-API-Arg escaping, the
// get-temporary-link download path that survives COOP/COEP, and the same
// localStorage bookkeeping keys — so a value it writes (sync-state, cloud-index)
// is read back consistently by the app. Host I/O (OPFS) is injected so this can
// later move into the host app's modules/ and be shared by dropbox.js (de-dup),
// which is the clean follow-up to this VM-local first cut.

const TOKENS_KEY = "dbxfull-tokens";
const ROOT_KEY = "dbxfull-working-root";
const STATE_KEY = "dbxfull-sync-state";
const INDEX_KEY = "dbxfull-cloud-index";

function tokens() {
    try {
        return JSON.parse(localStorage.getItem(TOKENS_KEY) || "null");
    } catch {
        return null;
    }
}
function workingRoot() {
    return localStorage.getItem(ROOT_KEY) || "";
}
function relToCloud(rel) {
    return workingRoot() + "/" + String(rel).replace(/^\/+/, "");
}
function syncState() {
    try {
        return JSON.parse(localStorage.getItem(STATE_KEY) || "{}");
    } catch {
        return {};
    }
}
function setSyncState(s) {
    localStorage.setItem(STATE_KEY, JSON.stringify(s));
}
function cloudIndex() {
    try {
        return JSON.parse(localStorage.getItem(INDEX_KEY) || "{}");
    } catch {
        return {};
    }
}
function setCloudIndex(i) {
    localStorage.setItem(INDEX_KEY, JSON.stringify(i));
}

// A short-lived access token, refreshed from the stored PKCE refresh token when
// it is within a minute of expiry (matches the app's threshold).
async function accessToken() {
    const s = tokens();
    if (!s) throw new Error("Dropbox not connected");
    if (Date.now() < s.expires_at - 60000) return s.access_token;
    const body = new URLSearchParams({
        grant_type: "refresh_token",
        refresh_token: s.refresh_token,
        client_id: s.app_key,
    });
    const res = await fetch("https://api.dropboxapi.com/oauth2/token", {
        method: "POST",
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: body.toString(),
    });
    if (!res.ok) throw new Error("Token refresh failed: " + (await res.text()));
    const d = await res.json();
    s.access_token = d.access_token;
    s.expires_at = Date.now() + d.expires_in * 1000;
    localStorage.setItem(TOKENS_KEY, JSON.stringify(s));
    return d.access_token;
}

// Dropbox-API-Arg rides in an HTTP header, which must be ASCII; escape every
// non-ASCII char as \uXXXX or accented paths 401 at the edge.
function apiArg(obj) {
    return JSON.stringify(obj).replace(
        /[^\x00-\x7F]/g,
        c => "\\u" + c.charCodeAt(0).toString(16).padStart(4, "0"),
    );
}

async function api(path, body) {
    const t = await accessToken();
    const res = await fetch("https://api.dropboxapi.com" + path, {
        method: "POST",
        headers: { Authorization: "Bearer " + t, "Content-Type": "application/json" },
        body: JSON.stringify(body),
    });
    if (!res.ok) throw new Error(`Dropbox ${path}: ${res.status} ${await res.text()}`);
    return res.json();
}

// get_temporary_link is a normal CORS RPC; the returned link is a plain GET with
// no custom headers, so it has no preflight and works cross-origin under the
// prod COOP/COEP isolation (a direct /2/files/download 200 would fail CORS).
async function download(cloudPath) {
    const tl = await api("/2/files/get_temporary_link", { path: cloudPath });
    const res = await fetch(tl.link);
    if (!res.ok) throw new Error("Download " + cloudPath + ": " + res.status);
    return new Uint8Array(await res.arrayBuffer());
}

async function upload(cloudPath, bytes) {
    const t = await accessToken();
    const res = await fetch("https://content.dropboxapi.com/2/files/upload", {
        method: "POST",
        headers: {
            Authorization: "Bearer " + t,
            "Content-Type": "application/octet-stream",
            "Dropbox-API-Arg": apiArg({
                path: cloudPath,
                mode: "overwrite",
                mute: true,
                autorename: false,
            }),
        },
        body: bytes,
    });
    if (!res.ok) {
        throw new Error("Upload " + cloudPath + ": " + res.status + " " + (await res.text()).slice(0, 200));
    }
    return res.json();
}

async function del(cloudPath) {
    try {
        return await api("/2/files/delete_v2", { path: cloudPath });
    } catch (e) {
        if (String(e.message).includes("not_found")) return null;
        throw e;
    }
}

// Drop a path and its subtree from both the sync state and the cloud index, so a
// deleted placeholder stops showing and is not re-listed.
function forgetFromStateAndIndex(rel) {
    const lk = String(rel).toLowerCase();
    const st = syncState();
    let sChanged = false;
    for (const k of Object.keys(st)) {
        const kk = k.toLowerCase();
        if (kk === lk || kk.startsWith(lk + "/")) {
            delete st[k];
            sChanged = true;
        }
    }
    if (sChanged) setSyncState(st);
    const idx = cloudIndex();
    let iChanged = false;
    for (const k of Object.keys(idx)) {
        const kk = k.toLowerCase();
        if (kk === lk || kk.startsWith(lk + "/")) {
            delete idx[k];
            iChanged = true;
        }
    }
    if (iChanged) setCloudIndex(idx);
}

/// Create a sync engine. `host.opfs` provides { read, write, exists } over
/// OPFS-relative paths. Returns { notify, hydrate, wireServiceWorker, connected }.
export function createSyncCore(host) {
    // Push a created/edited file: upload its current OPFS bytes to Dropbox and
    // record it as synced (rev + now), so the app's own sync sees it as clean
    // rather than a conflict to re-download.
    async function pushChanged(rel) {
        rel = String(rel).replace(/^\/+/, "");
        if (!rel || !tokens()) return;
        const bytes = await host.opfs.read(rel);
        if (bytes == null) return;
        const cloud = relToCloud(rel);
        const meta = await upload(cloud, bytes);
        const size = meta.size != null ? meta.size : bytes.length;
        const st = syncState();
        st[rel] = { rev: meta.rev || "", size, syncedMtime: Date.now() };
        setSyncState(st);
        const idx = cloudIndex();
        idx[rel] = { name: rel.split("/").pop(), kind: "file", path: cloud, size, rev: meta.rev || "" };
        setCloudIndex(idx);
    }

    async function pushDeleted(rel) {
        rel = String(rel).replace(/^\/+/, "");
        if (!rel) return;
        forgetFromStateAndIndex(rel);
        if (tokens()) await del(relToCloud(rel)).catch(() => {});
    }

    // Fetch one cloud-only file into OPFS (the SW's /files/ fault-in path).
    async function hydrate(rel) {
        rel = String(rel).replace(/^\/+/, "");
        const e = cloudIndex()[rel];
        if (!e || e.kind !== "file") return false;
        if (await host.opfs.exists(rel)) return true;
        const bytes = await download(e.path || relToCloud(rel));
        await host.opfs.write(rel, bytes);
        try {
            const st = syncState();
            st[rel] = { rev: e.rev || "", size: e.size || bytes.length, syncedMtime: Date.now() };
            setSyncState(st);
        } catch {}
        return true;
    }

    // Answer the service worker's hydrate request so `/files/<rel>` content
    // faults in with no host app tab — this page is a window client too.
    function wireServiceWorker() {
        if (!("serviceWorker" in navigator)) return;
        navigator.serviceWorker.addEventListener("message", ev => {
            const d = ev.data;
            if (!d || d.type !== "sw-hydrate" || !d.rel) return;
            const port = ev.ports && ev.ports[0];
            (async () => {
                let ok = false;
                try {
                    ok = await hydrate(d.rel);
                } catch (e) {
                    console.warn("[sync-core] hydrate failed", d.rel, e);
                }
                if (port) {
                    try {
                        port.postMessage({ ok });
                    } catch {}
                }
            })();
        });
    }

    // Serialise pushes so overlapping guest changes don't race each other on
    // Dropbox or on the shared localStorage bookkeeping.
    let chain = Promise.resolve();
    function notify(kind, rel) {
        chain = chain
            .then(() => (kind === "deleted" ? pushDeleted(rel) : pushChanged(rel)))
            .catch(e => console.warn("[sync-core] push failed", kind, rel, e));
        return chain;
    }

    return { notify, hydrate, pushChanged, pushDeleted, wireServiceWorker, connected: () => !!tokens() };
}
