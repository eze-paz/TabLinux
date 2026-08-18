//! Boot the real Alpine guest with a LAZY virtio-9p share and prove the
//! defer/supply path: nothing is seeded, and every directory listing and file
//! read the guest performs is faulted in from a host directory through
//! `p9_take_reqs`/`p9_supply` — exactly what vm-worker.js does with
//! `fetch('/files/...')` in the browser, but driven from a local tree here.
//!
//!   cargo run --release -p riscv-machine --example test_9p_lazy
//!
//! argv[1] = disk image with the 9p modules under /mod (default
//! kernels/disk-ext4.img); argv[2] = host root to serve (default a temp tree
//! this example creates).

use riscv_machine::{BootImages, Machine};
use riscv_devices::VirtioBlk;
use riscv_devices::virtio_blk::BlockBackend;

struct FileDisk {
    data: Vec<u8>,
}
impl BlockBackend for FileDisk {
    fn capacity_sectors(&self) -> u64 {
        (self.data.len() / 512) as u64
    }
    fn read(&mut self, sector: u64, buf: &mut [u8]) -> bool {
        let off = sector as usize * 512;
        if off + buf.len() > self.data.len() {
            return false;
        }
        buf.copy_from_slice(&self.data[off..off + buf.len()]);
        true
    }
    fn write(&mut self, sector: u64, buf: &[u8]) -> bool {
        let off = sector as usize * 512;
        if off + buf.len() > self.data.len() {
            return false;
        }
        self.data[off..off + buf.len()].copy_from_slice(buf);
        true
    }
}

/// Serialise a directory listing the way `Virtio9p::apply_listing` parses it:
/// repeated `namelen[u16 LE] | name | flags[u8] (bit0 = is_dir) | size[u64 LE]`.
fn list_dir(root: &str, rel: &str) -> Vec<u8> {
    let dir = if rel.is_empty() { root.to_string() } else { format!("{root}/{rel}") };
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else { return out };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let md = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = md.is_dir();
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(if is_dir { 1 } else { 0 });
        out.extend_from_slice(&md.len().to_le_bytes());
    }
    out
}

fn main() {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));
    let disk_path =
        std::env::args().nth(1).unwrap_or_else(|| format!("{root}/kernels/disk-ext4.img"));
    let host_root = std::env::args().nth(2).unwrap_or_else(|| {
        // Build a small tree to serve.
        let base = "/tmp/p9lazy".to_string();
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(format!("{base}/sandpie/conversations")).unwrap();
        std::fs::write(format!("{base}/README"), b"lazy 9p host root\n").unwrap();
        std::fs::write(
            format!("{base}/sandpie/conversations/first.md"),
            b"# hello from a lazily-faulted file\nthis text lives only on the host\n",
        )
        .unwrap();
        base
    });
    eprintln!("[serving host root: {host_root}]");

    let kernel = std::fs::read(format!("{root}/kernels/vmlinuz-lts.raw")).expect("kernel");
    let initrd = std::fs::read(format!("{root}/kernels/boot/initramfs-lts")).expect("initrd");
    let dtb = std::fs::read(format!("{root}/kernels/boot.dtb")).expect("boot.dtb");
    let disk = std::fs::read(&disk_path).unwrap_or_else(|_| panic!("disk image {disk_path}"));

    let mut m = Machine::new(BootImages {
        kernel: &kernel,
        initrd: &initrd,
        dtb: &dtb,
        dram_bytes: 1 << 30,
    });
    m.bus.attach_virtio_net([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]).expect("net slot");
    m.bus
        .attach_virtio(Box::new(VirtioBlk::new(Box::new(FileDisk { data: disk }))))
        .expect("blk slot");
    m.bus.attach_9p_lazy("shared").expect("9p slot");

    // Service any faults the guest raised: fetch from the host tree and supply.
    let serve = |m: &mut Machine| {
        for r in m.bus.p9_take_reqs() {
            let payload = match r.kind {
                0 => std::fs::read(if r.path.is_empty() {
                    host_root.clone()
                } else {
                    format!("{host_root}/{}", r.path)
                })
                .unwrap_or_default(),
                1 => list_dir(&host_root, &r.path),
                _ => Vec::new(),
            };
            eprintln!("[fault kind={} path='{}' -> {} bytes]", r.kind, r.path, payload.len());
            m.bus.p9_supply(r.id, &payload);
        }
        // Write-back: replay guest mutations onto the host tree (the browser
        // does this into OPFS, then sync-core pushes to Dropbox).
        for ch in m.bus.p9_take_changes() {
            let abs = format!("{host_root}/{}", ch.path);
            match ch.op {
                0 => {
                    if let Some(p) = std::path::Path::new(&abs).parent() {
                        let _ = std::fs::create_dir_all(p);
                    }
                    let _ = std::fs::write(&abs, &ch.data);
                    eprintln!("[change WRITE '{}' {} bytes]", ch.path, ch.data.len());
                }
                1 => {
                    let _ = std::fs::remove_file(&abs).or_else(|_| std::fs::remove_dir_all(&abs));
                    eprintln!("[change DELETE '{}']", ch.path);
                }
                2 => {
                    let _ = std::fs::create_dir_all(&abs);
                    eprintln!("[change MKDIR '{}']", ch.path);
                }
                _ => {}
            }
        }
        m.bus.flush_virtio_completions();
    };

    let mut console = String::new();
    while !console.contains("recovery shell") {
        m.run(50_000_000);
        console.push_str(&String::from_utf8_lossy(&m.take_console()));
        serve(&mut m);
        assert!(m.steps < 5_000_000_000, "never reached the shell:\n{console}");
    }
    eprintln!("[booted at {}M steps]", m.steps / 1_000_000);

    let script = concat!(
        "mkdir -p /mnt/disk && mount -t ext4 /dev/vda /mnt/disk 2>&1\n",
        "insmod /mnt/disk/mod/netfs.ko 2>&1\n",
        "insmod /mnt/disk/mod/9pnet.ko 2>&1\n",
        "insmod /mnt/disk/mod/9pnet_virtio.ko 2>&1\n",
        "insmod /mnt/disk/mod/9p.ko 2>&1\n",
        "echo MODS_LOADED $?\n",
        "mkdir -p /files\n",
        "mount -t 9p -o trans=virtio,version=9p2000.L,msize=131072 shared /files 2>&1\n",
        "echo MOUNTED $?\n",
        "echo --- LS ROOT ---; ls -la /files 2>&1\n",
        "echo --- LS SUB ---; ls -la /files/sandpie/conversations 2>&1\n",
        "echo --- CAT ---; cat /files/sandpie/conversations/first.md 2>&1\n",
        // Write-back exercises: create+write, mkdir, and delete.
        "echo guest-authored > /files/sandpie/conversations/reply.txt 2>&1\n",
        "mkdir /files/newdir 2>&1\n",
        "rm /files/sandpie/conversations/first.md 2>&1\n",
        "echo ===9P_DONE===\n",
    );
    m.console_input(script.as_bytes());

    let start = m.steps;
    while !console.contains("===9P_DONE===") {
        m.run(20_000_000);
        console.push_str(&String::from_utf8_lossy(&m.take_console()));
        serve(&mut m);
        if m.steps - start > 8_000_000_000 {
            eprintln!("--- console so far ---\n{console}");
            panic!("lazy 9p script did not finish");
        }
    }

    let tail: String = console.chars().rev().take(1400).collect::<String>().chars().rev().collect();
    eprintln!("--- console tail ---\n{tail}\n--- end ---");

    // Give the write-back a couple more service turns to drain.
    for _ in 0..20 {
        m.run(20_000_000);
        console.push_str(&String::from_utf8_lossy(&m.take_console()));
        serve(&mut m);
    }

    let ok_mount = console.contains("MOUNTED 0");
    let ok_ls = console.contains("sandpie");
    let ok_sub = console.contains("first.md");
    let ok_cat = console.contains("lazily-faulted file");
    // Write-back: the host tree should now reflect the guest's mutations.
    let wrote = std::fs::read_to_string(format!("{host_root}/sandpie/conversations/reply.txt"))
        .map(|s| s.contains("guest-authored"))
        .unwrap_or(false);
    let mkdired = std::path::Path::new(&format!("{host_root}/newdir")).is_dir();
    let deleted = !std::path::Path::new(&format!("{host_root}/sandpie/conversations/first.md")).exists();
    eprintln!("mount:{ok_mount} ls:{ok_ls} sub:{ok_sub} cat:{ok_cat}");
    eprintln!("write-back  write:{wrote} mkdir:{mkdired} delete:{deleted}");
    assert!(ok_mount, "lazy 9p mount failed");
    assert!(ok_ls, "root listing did not fault in");
    assert!(ok_sub, "subdir listing did not fault in");
    assert!(ok_cat, "file read did not fault in");
    assert!(wrote, "guest write did not propagate to the host");
    assert!(mkdired, "guest mkdir did not propagate to the host");
    assert!(deleted, "guest delete did not propagate to the host");
    eprintln!("LAZY 9P + WRITE-BACK WORKS");
}
