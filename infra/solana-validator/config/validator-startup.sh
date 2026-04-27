#!/usr/bin/env bash
# config/validator-startup.sh
#
# Launch script for the Solana Agave non-voting RPC validator with the
# Yellowstone gRPC Geyser plugin.
#
# Called by: /etc/systemd/system/agave-validator.service ExecStart=
# Must be executable: chmod +x /home/sol/config/validator-startup.sh
# Must be owned by sol: chown sol:sol /home/sol/config/validator-startup.sh
#
# Pinned Agave version: v3.1.13
# Pinned yellowstone-grpc: v12.2.0+solana.3.1.13
#
# SUBSTITUTIONS (replace before first run):
#   <DATA_DIR>          — base home dir, e.g. /home/sol
#   <LEDGER_DIR>        — NVMe mount for ledger, e.g. /mnt/ledger
#   <ACCOUNTS_DIR>      — NVMe mount for accounts-db, e.g. /mnt/accounts
#   <SNAPSHOTS_DIR>     — NVMe/SSD mount for snapshot staging, e.g. /mnt/snapshots
#   <IDENTITY_KEYPAIR>  — full path to identity keypair JSON

set -eo pipefail

# ---------------------------------------------------------------------------
# Config — edit these paths, do not change variable names
# ---------------------------------------------------------------------------

DATA_DIR="/home/sol"
LEDGER_DIR="/mnt/ledger"
ACCOUNTS_DIR="/mnt/accounts"
SNAPSHOTS_DIR="/mnt/snapshots"
IDENTITY_KEYPAIR="${DATA_DIR}/keypairs/identity-keypair.json"
LOG_FILE="${DATA_DIR}/logs/agave-validator.log"
PLUGIN_CONFIG="${DATA_DIR}/config/yellowstone-grpc-config.json"
PLUGIN_LIB="${DATA_DIR}/yellowstone-grpc/target/release/libyellowstone_grpc_geyser.so"

# Agave install path (set by the agave installer; adjust if custom install)
AGAVE_BIN="${DATA_DIR}/.local/share/solana/install/active_release/bin"

# ---------------------------------------------------------------------------
# Mainnet-beta constants — DO NOT CHANGE without verifying on-chain
# ---------------------------------------------------------------------------

GENESIS_HASH="5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d"

# Foundation-operated known validators used as snapshot trust anchors.
# Source: https://github.com/CryptoManufaktur-io/solana-rpc/blob/main/start-validator.sh
KNOWN_VALIDATORS=(
  "7Np41oeYqPefeNQEHSv1UDhYrehxin3NStELsSKCT4K2"
  "GdnSyH3YtwcxFvQrVVJMm1JhTS4QVX7MFsX56uJLUfiZ"
  "DE1bawNcRJB9rVm3buyMVfr8mBEoyyu73NBovf2oXJsJ"
  "CakcnaRDHka2gXyfbEd2d3xsvkJkqsLw2akB3zsN1D2S"
)

# Gossip entrypoints — 5 mainnet-beta endpoints for redundancy.
ENTRYPOINTS=(
  "entrypoint.mainnet-beta.solana.com:8001"
  "entrypoint2.mainnet-beta.solana.com:8001"
  "entrypoint3.mainnet-beta.solana.com:8001"
  "entrypoint4.mainnet-beta.solana.com:8001"
  "entrypoint5.mainnet-beta.solana.com:8001"
)

# ---------------------------------------------------------------------------
# Pre-flight validation — fail fast with a clear error, never silently
# ---------------------------------------------------------------------------

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

[[ -f "${IDENTITY_KEYPAIR}" ]] \
  || fail "Identity keypair not found at ${IDENTITY_KEYPAIR}. Run: solana-keygen new --outfile ${IDENTITY_KEYPAIR}"

[[ -f "${PLUGIN_CONFIG}" ]] \
  || fail "Yellowstone plugin config not found at ${PLUGIN_CONFIG}. Copy and edit the .example file."

[[ -f "${PLUGIN_LIB}" ]] \
  || fail "Yellowstone plugin .so not found at ${PLUGIN_LIB}. Run: cargo build --release -p yellowstone-grpc-geyser"

# Validate JSON config is well-formed
command -v jq >/dev/null 2>&1 \
  || fail "jq is not installed. Run: apt-get install -y jq"
jq . "${PLUGIN_CONFIG}" > /dev/null \
  || fail "Yellowstone plugin config is not valid JSON: ${PLUGIN_CONFIG}"

[[ -d "${LEDGER_DIR}" ]] \
  || fail "Ledger directory does not exist: ${LEDGER_DIR}. Mount the NVMe first."

[[ -d "${ACCOUNTS_DIR}" ]] \
  || fail "Accounts directory does not exist: ${ACCOUNTS_DIR}. Mount the NVMe first."

[[ -d "${SNAPSHOTS_DIR}" ]] \
  || fail "Snapshots directory does not exist: ${SNAPSHOTS_DIR}. Mount the disk first."

[[ -x "${AGAVE_BIN}/agave-validator" ]] \
  || fail "agave-validator binary not found at ${AGAVE_BIN}/agave-validator. Run the Agave installer."

mkdir -p "${DATA_DIR}/logs"

# ---------------------------------------------------------------------------
# PATH and environment
# ---------------------------------------------------------------------------

export PATH="${AGAVE_BIN}:${HOME}/.cargo/bin:${PATH}"
export RUST_LOG="${RUST_LOG:-warn,solana_core=info,solana_rpc=info,yellowstone_grpc=info}"

# Set default ulimits — belt-and-suspenders in case systemd limits are not applied.
ulimit -n 1000000 2>/dev/null || true
ulimit -l unlimited 2>/dev/null || true

# ---------------------------------------------------------------------------
# Build --known-validator and --entrypoint flags from arrays
# ---------------------------------------------------------------------------

KNOWN_VALIDATOR_FLAGS=()
for pubkey in "${KNOWN_VALIDATORS[@]}"; do
  KNOWN_VALIDATOR_FLAGS+=(--known-validator "${pubkey}")
done

ENTRYPOINT_FLAGS=()
for ep in "${ENTRYPOINTS[@]}"; do
  ENTRYPOINT_FLAGS+=(--entrypoint "${ep}")
done

# ---------------------------------------------------------------------------
# Exec — replace this process with agave-validator
# Never use a subshell here; exec lets systemd track the PID directly.
# ---------------------------------------------------------------------------

exec "${AGAVE_BIN}/agave-validator" \
  --identity "${IDENTITY_KEYPAIR}" \
  \
  `# --- Cluster ---` \
  --expected-genesis-hash "${GENESIS_HASH}" \
  "${KNOWN_VALIDATOR_FLAGS[@]}" \
  --only-known-rpc \
  "${ENTRYPOINT_FLAGS[@]}" \
  \
  `# --- RPC mode (non-voting) ---` \
  --no-voting \
  --full-rpc-api \
  --rpc-port 8899 \
  --rpc-bind-address 0.0.0.0 \
  --private-rpc \
  --enable-rpc-transaction-history \
  \
  `# --- Account indexes (used by chain-adapter getProgram queries) ---` \
  --account-index program-id \
  --account-index spl-token-mint \
  \
  `# --- Storage paths ---` \
  --ledger "${LEDGER_DIR}" \
  --accounts "${ACCOUNTS_DIR}" \
  --snapshots "${SNAPSHOTS_DIR}" \
  \
  `# --- Yellowstone gRPC Geyser plugin ---` \
  --geyser-plugin-config "${PLUGIN_CONFIG}" \
  \
  `# --- Snapshot and ledger management ---` \
  --wal-recovery-mode skip_any_corrupted_record \
  --limit-ledger-size \
  --snapshot-interval-slots 300 \
  --maximum-snapshots-to-retain 2 \
  \
  `# --- Network ---` \
  --dynamic-port-range 8000-8020 \
  --gossip-port 8001 \
  \
  `# --- Performance ---` \
  --unified-scheduler-handler-threads 12 \
  \
  `# --- Log ---` \
  --log "${LOG_FILE}"
