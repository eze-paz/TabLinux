//! Ping the host stack from inside a real Alpine guest, and price the round
//! trip in emulated time.
//!
//! This exists because the same experiment used to require the interactive
//! front end: boot for ~110 s of wall clock, type at a terminal, wait, read the
//! numbers off the screen. Eight minutes a run, and two of my measurements were
//! silently truncated by a too-short timeout, which is how a bad comparison got
//! believed. Here the whole thing is scripted and takes about a minute.
//!
//! The intent is to pin down the discrepancy in docs/networking.md — a host
//! round trip of a fraction of a millisecond against a guest-reported RTT of
//! over a second — with both numbers from one run, so they cannot drift apart
//! the way two separate terminal sessions can.
//!
//!   cargo test --release -p riscv-harness --test net_roundtrip -- --nocapture
//!
//! When this test first existed it could not complete the script at all, and
//! the earlier revision of this header blamed the console driver — UART input
//! overruns, prompt pacing. Both wrong. The per-window PC histogram below is
//! what found the truth: after `ip link set eth0 up` the guest was 100% busy
//! in virtio_net module code (`__pecoff_data_virt_size+…` = above the last
//! kernel symbol) because the transport retired RX buffer *offers* as
//! zero-length receives — an infinite refill storm. The instrument stays in
//! the test because the next regression of this kind will look identical: a
//! console that stops while steps burn.

use riscv_core::execute::Bus as _;
use riscv_machine::{BootImages, Machine};
use std::collections::BTreeMap;

const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// Same System.map symbolizer boot_to_userspace uses: when the budget goes
/// somewhere unexpected, a histogram of *named* PCs is the instrument that
/// says where, and raw addresses say nothing across boots.
struct Symbols {
    addrs: Vec<u64>,
    names: Vec<String>,
}

impl Symbols {
    fn load(path: &str) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut pairs: Vec<(u64, String)> = text
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let addr = u64::from_str_radix(f.next()?, 16).ok()?;
                let _kind = f.next()?;
                Some((addr, f.next()?.to_string()))
            })
            .collect();
        pairs.sort_by_key(|p| p.0);
        Some(Self {
            addrs: pairs.iter().map(|p| p.0).collect(),
            names: pairs.into_iter().map(|p| p.1).collect(),
        })
    }

    fn lookup(&self, pc: u64) -> String {
        match self.addrs.binary_search(&pc) {
            Ok(i) => self.names[i].clone(),
            Err(0) => format!("{pc:#x}"),
            Err(i) => format!("{}+{:#x}", self.names[i - 1], pc - self.addrs[i - 1]),
        }
    }
}

/// Emulated milliseconds. mtime runs at the 10 MHz the devicetree declares.
fn ms(mtime: u64) -> f64 {
    mtime as f64 / 10_000.0
}

#[test]
fn ping_round_trip() {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
    let kernel = std::fs::read(format!("{root}/kernels/vmlinuz-lts.raw")).expect("kernel");
    // RISCV_INITRD overrides the image, so boot-cost work (signature
    // stripping, compression changes) can be A/B measured with this same test.
    let initrd_path = std::env::var("RISCV_INITRD")
        .unwrap_or_else(|_| format!("{root}/kernels/boot/initramfs-lts"));
    let initrd = std::fs::read(&initrd_path).expect("initrd");
    let dtb = std::fs::read(format!("{root}/kernels/boot.dtb")).expect("boot.dtb");

    let mut m = Machine::new(BootImages {
        kernel: &kernel,
        initrd: &initrd,
        dtb: &dtb,
        dram_bytes: 1 << 30,
    });
    m.bus.attach_virtio_net(MAC).expect("a free virtio slot");

    // Pushed in one write, same as boot_to_userspace: the UART RX queue is
    // unbounded and the shell drains it line by line. (An earlier revision
    // paced this on prompt counts, chasing an input-overrun theory for a stall
    // whose real cause was the RX refill storm — see the header.)
    const SCRIPT: &str = concat!(
        "ip link set eth0 up\n",
        "ip addr add 10.0.2.15/24 dev eth0\n",
        "ping -c 1 10.0.2.2\n",
        "echo PING-DONE\n",
    );

    let mut console = String::new();
    let mut typed = false;
    let mut done = false;
    // (what, emulated ms) for every frame in either direction.
    let mut timeline: Vec<(String, f64)> = Vec::new();
    let started = std::time::Instant::now();
    let budget: u64 = std::env::var("MAX_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3_000_000_000);

    let syms = Symbols::load(&format!("{root}/kernels/boot/System.map-6.18.35-0-lts"));
    let sym = |pc: u64| syms.as_ref().map(|s| s.lookup(pc)).unwrap_or_else(|| format!("{pc:#x}"));
    let mut hot: BTreeMap<String, u64> = BTreeMap::new();
    let mut window: BTreeMap<String, u64> = BTreeMap::new();
    let mut next_report: u64 = 100_000_000;

    while m.steps < budget && !done {
        // Small slices so the host answers promptly. `run` also returns early
        // the moment the guest idles with a frame outstanding, so the slice
        // size bounds latency only while the guest is busy.
        m.run(200_000);

        // Where is the budget going? One sample per slice, aggregated by
        // FUNCTION — offsets split one hot loop into a dozen small entries and
        // make 5% look like the whole story, which is exactly the misreading
        // that sent the first run of this investigation down the wrong path.
        let name = sym(m.cpu.cpu.pc);
        let func = name.split('+').next().unwrap_or(&name).to_string();
        *hot.entry(func.clone()).or_insert(0) += 1;
        *window.entry(func).or_insert(0) += 1;
        if m.steps >= next_report {
            next_report += 100_000_000;
            let mut w: Vec<_> = window.iter().collect();
            w.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
            let top: Vec<String> =
                w.into_iter().take(3).map(|(f, n)| format!("{f} x{n}")).collect();
            eprintln!(
                "[progress] steps={}M mtime={:.0}ms console={}B frames={} | {}",
                m.steps / 1_000_000,
                ms(m.bus.read_mtime()),
                console.len(),
                timeline.len(),
                top.join(", ")
            );
            window.clear();
        }

        let out = m.take_console();
        if !out.is_empty() {
            console.push_str(&String::from_utf8_lossy(&out));
            if !typed && console.contains("emergency recovery shell") {
                typed = true;
                m.console_input(SCRIPT.as_bytes());
            }
            if typed && console.lines().any(|l| l.trim_end_matches('\r') == "PING-DONE") {
                done = true;
            }
        }

        // The host stack. Exactly what riscv-vm does, and what v86's
        // fake_network.js will do in the browser.
        let Some(q) = m.bus.net.clone() else { continue };
        loop {
            let f = q.borrow_mut().to_host.pop_front();
            let Some(f) = f else { break };
            // Name the ethertype: "the host answered nothing" means something
            // very different if the guest is only sending IPv6 multicast than
            // if it is sending ARP the responder should have matched.
            let et = if f.len() >= 14 { ((f[12] as u16) << 8) | f[13] as u16 } else { 0 };
            let kind = match et {
                0x0806 => "ARP",
                0x0800 => "IPv4",
                0x86DD => "IPv6",
                _ => "?",
            };
            timeline.push((format!("guest -> host  {kind}"), ms(m.bus.read_mtime())));
            if let Some(reply) = riscv_hostnet::respond(&f) {
                q.borrow_mut().to_guest.push_back(reply);
                timeline.push((format!("host  -> guest {kind}"), ms(m.bus.read_mtime())));
            }
        }
    }

    let mut hotv: Vec<_> = hot.iter().collect();
    hotv.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    eprintln!("\nhot PCs (one sample per 200k-step slice, by function):");
    for (name, n) in hotv.into_iter().take(12) {
        eprintln!("  {n:>6} {name}");
    }

    eprintln!("\n--- frames, in emulated time ---");
    for (what, at) in &timeline {
        eprintln!("  {what} at {at:.3} ms");
    }
    // Each request/reply pair is adjacent, so the gap between them is what the
    // emulator actually spends turning one around.
    let mut worst: f64 = 0.0;
    for w in timeline.windows(2) {
        if w[0].0.starts_with("guest") && w[1].0.starts_with("host") {
            worst = worst.max(w[1].1 - w[0].1);
        }
    }
    let reported = console
        .lines()
        .find_map(|l| l.split("time=").nth(1))
        .and_then(|t| t.split_whitespace().next())
        .and_then(|t| t.parse::<f64>().ok());

    eprintln!("\nhost turnaround   : {worst:.3} ms emulated (worst pair)");
    match reported {
        Some(r) => eprintln!("guest reports RTT : {r:.3} ms"),
        None => eprintln!("guest reports RTT : (no reply parsed)"),
    }
    eprintln!("steps             : {}", m.steps);
    eprintln!("wall clock        : {:.1} s", started.elapsed().as_secs_f64());

    assert!(done, "guest never finished the script; console:\n{console}");
    assert!(
        console.contains("1 packets received") || console.contains("1 received"),
        "no ICMP reply reached the guest; console tail:\n{}",
        console.chars().rev().take(1200).collect::<String>().chars().rev().collect::<String>()
    );
    // The emulator's own turnaround is the part this crate controls, and it is
    // sub-millisecond. Guarding it keeps a future change from quietly making
    // the host path slow while the guest-side number stays bad for its own
    // unrelated reason.
    assert!(
        worst < 5.0,
        "host turnaround regressed to {worst:.3} ms emulated (was ~0.3 ms)"
    );
}
