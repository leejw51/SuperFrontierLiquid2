"""Compute the deterministic trader addresses derived by the simulator.

Useful when a verification script needs to query the dashboard for trader balances
without launching the Rust cb-sim binary just for `traders` mode. Mirrors the
derivation in cb-crypto::keypair_from_seed (keccak256(seed)) and
cb-crypto::address_from_pubkey (keccak256(uncompressed_xy)[12..]).

Implementation note: secp256k1 pubkey derivation in Python requires an external
library (coincurve / ecdsa). We avoid the dep by shelling out to
`target/release/cb-sim traders --num-traders N --seed-prefix P` which already
prints `index  addr` on stdout. Cheap, deterministic, no extra Python deps.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
SIM_BIN = REPO_ROOT / "target" / "release" / "cb-sim"


def trader_addresses(seed_prefix: str, count: int) -> list[str]:
    """Returns a list of 0x-prefixed 40-char hex addresses for traders 0..count-1."""
    if not SIM_BIN.exists():
        raise FileNotFoundError(
            f"cb-sim binary missing — run `cargo build --release -p cb-sim` first ({SIM_BIN})"
        )
    out = subprocess.check_output(
        [
            str(SIM_BIN),
            "traders",
            "--num-traders",
            str(count),
            "--seed-prefix",
            seed_prefix,
        ],
        text=True,
    )
    addrs: list[str] = []
    for line in out.splitlines():
        line = line.strip()
        if not line:
            continue
        # cb-sim prints "  N  0xADDR"
        parts = line.split()
        if len(parts) >= 2 and parts[-1].startswith("0x"):
            addrs.append(parts[-1])
    if len(addrs) != count:
        raise RuntimeError(
            f"expected {count} trader addresses from cb-sim, parsed {len(addrs)}: {out!r}"
        )
    return addrs
