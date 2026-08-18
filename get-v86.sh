#!/bin/bash
# Fetch what v86 needs to boot, so its throughput can be measured against ours.
#
# The npm package ships only the engine (libv86.js + v86.wasm). A BIOS and an OS
# image live elsewhere, and both are needed before v86 executes a single guest
# instruction. v86's own Node example boots images/linux4.iso.
set -e
cd /tmp/v86cmp

for f in seabios.bin vgabios.bin; do
    if [ ! -s "$f" ] || [ "$(stat -c%s "$f")" -lt 1000 ]; then
        curl -sSL -o "$f" "https://raw.githubusercontent.com/copy/v86/master/bios/$f"
    fi
done
ls -la seabios.bin vgabios.bin

if [ ! -s linux4.iso ] || [ "$(stat -c%s linux4.iso)" -lt 100000 ]; then
    for u in \
        "https://k.copy.sh/images/linux4.iso" \
        "https://copy.sh/v86/images/linux4.iso" \
        "https://raw.githubusercontent.com/copy/v86/master/images/linux4.iso"
    do
        echo "trying $u"
        if curl -sSL --max-time 240 -o linux4.iso "$u" && \
           [ "$(stat -c%s linux4.iso)" -gt 100000 ]; then
            echo "got it from $u"
            break
        fi
    done
fi
ls -la linux4.iso 2>/dev/null && file linux4.iso | head -1
