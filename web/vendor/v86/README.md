# Vendored from v86

`browser/fake_network.js` is copied **verbatim** from
[v86](https://github.com/copy/v86) (`src/browser/fake_network.js`), under the
BSD-2-Clause license in `LICENSE`. It provides ARP, DHCP, ICMP, NTP,
DNS-over-HTTPS and a full TCP state machine over raw Ethernet frames.

Keeping the file unmodified is the point: updates are a plain re-copy, and
nothing in our tree forks its logic. The rest of this directory exists only to
satisfy its three relative imports:

| stub | replaces | contents |
|---|---|---|
| `const.js` | `src/const.js` | the one constant it names (`LOG_FETCH`) |
| `lib.js` | `src/lib.js` | `h()` hex formatter, byte-for-byte |
| `log.js` | `src/log.js` | `dbg_log`/`dbg_assert` routed to the console |

The adapter that feeds it frames lives outside this directory, in
`web/net-adapter.js` — that part is ours.
