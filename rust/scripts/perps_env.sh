#!/usr/bin/env bash
# Print the env-var assignments that `genesis-perps.jsonl` references,
# deriving every address from a deterministic (BIP-39 mnemonic, BIP-44 index)
# pair. The pairs below are pinned dev fixtures — anyone with these
# mnemonics can sign as the corresponding identity on a chain booted from
# this file. Do NOT reuse them on a real deployment.
#
# Usage from the Makefile (see `startperps`):
#   set -a; source <(scripts/perps_env.sh); set +a
#   $(NODE_BIN) --data-dir $(DATA_DIR_PERPS) init --genesis genesis-perps.jsonl
#
# Usage from a shell:
#   eval "$(scripts/perps_env.sh)"
#   cb-client login --label oracle-btc --mnemonic "${ORACLE_BTC_MNEMONIC}" --index 10
#
# Picking the same address two ways:
#   - Re-derive: `cb-client addr --mnemonic <M> --index <N>` ⇒ same hex.
#   - Read here: this script's output is stable as long as cb-client's
#     BIP-39/BIP-44 deriver doesn't change.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLIENT_BIN="${REPO_ROOT}/target/release/cb-client"

if [[ ! -x "${CLIENT_BIN}" ]]; then
  echo "perps_env.sh: ${CLIENT_BIN} missing — run 'make build-client' first" >&2
  exit 1
fi

# Two real BIP-39 phrases (test vectors from BIP-39 spec). Each role gets a
# (mnemonic, index) pair; same mnemonic across roles is fine because the
# BIP-44 path differs per index. Mnemonics MUST satisfy the BIP-39 checksum
# or `cb-client addr` will reject them — these two do.
M_ABANDON="abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"
M_LEGAL="legal winner thank year wave sausage worth useful legal winner thank yellow"
M_LETTER="letter advice cage absurd amount doctor acoustic avoid letter advice cage above"
M_ZOO="zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong"

# (var, mnemonic_var, index, mnemonic_string) — printed both as exports and
# as `*_MNEMONIC` / `*_INDEX` companions so the runbook can `cb-client login`
# straight from the env without re-typing.
derive() {
  "${CLIENT_BIN}" addr --mnemonic "$1" --index "$2"
}

cat <<EOF
# --- chain-level ---
export MYCHAINID=${MYCHAINID:-1}
export MYDENOM=${MYDENOM:-ULD}
export INITIAL_AMOUNT=${INITIAL_AMOUNT:-1000000000000}

# --- validator + community wallets (M0..M3 above, index 0) ---
export MYWALLET0=$(derive "${M_ABANDON}" 0)
export MYWALLET0_MNEMONIC="${M_ABANDON}"
export MYWALLET0_INDEX=0
export MYWALLET1=$(derive "${M_LEGAL}" 0)
export MYWALLET1_MNEMONIC="${M_LEGAL}"
export MYWALLET1_INDEX=0
export MYWALLET2=$(derive "${M_LETTER}" 0)
export MYWALLET2_MNEMONIC="${M_LETTER}"
export MYWALLET2_INDEX=0
export MYWALLET3=$(derive "${M_ZOO}" 0)
export MYWALLET3_MNEMONIC="${M_ZOO}"
export MYWALLET3_INDEX=0

# --- oracle authorities (M_ABANDON, indices 10/11) ---
export ORACLE_BTC=$(derive "${M_ABANDON}" 10)
export ORACLE_BTC_MNEMONIC="${M_ABANDON}"
export ORACLE_BTC_INDEX=10
export ORACLE_ETH=$(derive "${M_ABANDON}" 11)
export ORACLE_ETH_MNEMONIC="${M_ABANDON}"
export ORACLE_ETH_INDEX=11

# --- demo traders (M_ABANDON, indices 20..22) ---
export DEMO_TRADER0=$(derive "${M_ABANDON}" 20)
export DEMO_TRADER0_MNEMONIC="${M_ABANDON}"
export DEMO_TRADER0_INDEX=20
export DEMO_TRADER1=$(derive "${M_ABANDON}" 21)
export DEMO_TRADER1_MNEMONIC="${M_ABANDON}"
export DEMO_TRADER1_INDEX=21
export DEMO_TRADER2=$(derive "${M_ABANDON}" 22)
export DEMO_TRADER2_MNEMONIC="${M_ABANDON}"
export DEMO_TRADER2_INDEX=22
EOF
