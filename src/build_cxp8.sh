#!/usr/bin/env bash
#
# Build the Kasli-SoC CoaXPress-SFP gateware + firmware and assemble boot.bin — ARTIQ 8.
#
# This is the ARTIQ 8 counterpart of the ARTIQ 10 coaxpress_flash/build_cxp.sh. It MUST
# be run from artiq-zynq/src INSIDE the overridden `nix develop` shell (see the wrapper
# `flash_cxp8.sh`), because the ARTIQ 8 flake pins:
#   * artiq  -> remote release-8 (we override it to the LOCAL artiq8/artiq tree), and
#   * misoc  -> a pre-multichannel-CXP revision (we override misoc+migen to the v10 revs)
# Without those overrides the CXP gateware cannot be elaborated/synthesised.

source /tools/Xilinx/Vivado/2022.2/settings64.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DESC="${SCRIPT_DIR}/amethyst-cxp.json"

if [[ ! -f "$DESC" ]]; then
    echo "ERROR: description not found: $DESC" >&2
    exit 1
fi
if [[ ! -f "gateware/kasli_soc.py" ]]; then
    echo "ERROR: run this from artiq-zynq/src inside 'nix develop'." >&2
    echo "       (gateware/kasli_soc.py not found in \$PWD=$PWD)" >&2
    exit 1
fi

echo "==> Using description: $DESC"

echo "==> [1/4] Building gateware (bitstream) with Vivado..."
gateware/kasli_soc.py -g ../build/gateware "$DESC"

echo "==> [2/4] Building firmware (runtime, standalone)..."
make TARGET=kasli_soc GWARGS="$DESC" runtime

echo "==> [3/4] Fetching second-stage loader (szl.elf) for release-8..."
nix build "git+https://git.m-labs.hk/m-labs/zynq-rs?ref=release-8#kasli_soc-szl"

echo "==> [4/4] Assembling boot.bin..."
cat > boot.bif <<'EOF'
the_ROM_image:
{
    [bootloader]result/szl.elf
    ../build/gateware/top.bit
    [elf_use_ph] ../build/firmware/armv7-none-eabihf/release/runtime
}
EOF
mkbootimage boot.bif boot.bin

echo ""
echo "==> Done. boot.bin created at: $PWD/boot.bin"
