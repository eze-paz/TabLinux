// Stub for v86's src/log.js — only what fake_network.js imports.
// Set `window.RISCV_NET_DEBUG = true` (or self. in a worker) to see the
// adapter narrate every DHCP/ARP/DNS/TCP decision it makes.

export function dbg_log(msg, _level)
{
    if(globalThis.RISCV_NET_DEBUG)
    {
        console.debug("[fake_network]", msg);
    }
}

export function dbg_assert(cond, msg, _level)
{
    if(!cond)
    {
        console.error("[fake_network] assertion failed:", msg || "");
    }
}
