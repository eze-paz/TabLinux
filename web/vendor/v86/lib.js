// Stub for v86's src/lib.js — only what fake_network.js imports.
// `h` and `pad0` are copied byte-for-byte so hex in log lines matches v86's.

export function pad0(str, len)
{
    str = (str || str === 0) ? str + "" : "";
    return str.padStart(len, "0");
}

export function h(n, len)
{
    if(!n)
    {
        var str = "";
    }
    else
    {
        var str = n.toString(16);
    }

    return "0x" + pad0(str.toUpperCase(), len || 1);
}
