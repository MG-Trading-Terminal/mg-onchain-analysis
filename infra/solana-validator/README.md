# Solana Validator Runbook

**Scope:** Stand up a non-voting Agave RPC node with the Yellowstone gRPC Geyser plugin,
exposing `grpc://0.0.0.0:10000` for local consumers and `http://0.0.0.0:8899` for
JSON-RPC — exactly what `config/adapters.toml.example` points at by default.

**Time to first stream:** 24–48 hours (dominated by snapshot download and account-db replay
on gigabit ingress). The machine is usable for other tasks while sync proceeds.

**Who this is for:** A single operator with root access to the target machine, following
this document top-to-bottom without needing to consult external docs mid-procedure.
External references are linked at the bottom as background reading.

---

## Table of Contents

1. [What This Delivers](#1-what-this-delivers)
2. [Hardware BOM](#2-hardware-bom)
3. [Client Choice](#3-client-choice-agave-vs-firedancer)
4. [Pinned Versions](#4-pinned-versions)
5. [OS Preparation](#5-os-preparation)
6. [System Tuning](#6-system-tuning)
7. [User + Directory Layout](#7-user--directory-layout)
8. [Rust Toolchain](#8-rust-toolchain)
9. [Install Agave Validator Binary](#9-install-agave-validator-binary)
10. [Generate Identity Keypair](#10-generate-identity-keypair)
11. [Build Yellowstone-gRPC Plugin](#11-build-yellowstone-grpc-plugin)
12. [Configure the Plugin](#12-configure-the-plugin)
13. [Snapshot Sync](#13-snapshot-sync)
14. [Validator Startup](#14-validator-startup)
15. [Install systemd Unit](#15-install-systemd-unit)
16. [Health Checks](#16-health-checks)
17. [Monitoring](#17-monitoring)
18. [Troubleshooting](#18-troubleshooting)
19. [References](#19-references)

---

## 1. What This Delivers

A **non-voting RPC node** on Solana mainnet-beta that:

- Follows chain tip via gossip + turbine (receives blocks, does NOT produce or vote on them).
- Exposes `agave-validator` JSON-RPC on port **8899** for backfill (`getBlock`,
  `getSignaturesForAddress`, `getAccountInfo`).
- Loads the **Yellowstone gRPC Geyser plugin** and streams account updates, transactions,
  and slot metadata on port **10000** over plain gRPC — the endpoint `chain-adapter` connects to.
- Emits Prometheus metrics (validator: port **1234**, plugin: port **8999**, node-exporter:
  port **9100**) for Grafana dashboards and alerting.

This node does NOT:

- Hold a vote account or earn staking rewards.
- Run a `solana-keygen` derived vote keypair (none needed).
- Archive historical ledger beyond the prune window (configurable; default keeps ~2 days).

---

## 2. Hardware BOM

Requirements below come from `docs/adr/0003-self-sovereign-infrastructure.md` refined against
the Agave operations documentation (see §19).

| Resource | Minimum | Recommended |
|---|---|---|
| CPU | 16 cores / 32 threads, 2.8 GHz+, AVX2 | 32+ cores EPYC or Xeon Scalable |
| RAM | 128 GB ECC (restricted account indexes) | 512 GB ECC |
| Disk — accounts | 1 TB NVMe PCIe Gen3 x4 | 2 TB NVMe PCIe Gen4 (own mount) |
| Disk — ledger | 1 TB NVMe (own mount) | 2 TB NVMe (own mount) |
| Disk — snapshots | 500 GB NVMe or SATA SSD | 1 TB NVMe (own mount) |
| Disk — OS | 100 GB SATA SSD | 500 GB SATA SSD |
| Network | 1 Gbps symmetric | 10 Gbps symmetric |

> **RAM note:** Anza's official requirements page specifies 512 GB for full account indexes.
> 256 GB is sufficient if you skip `--account-index spl-token-owner` and
> `--account-index spl-token-mint` — this is acceptable for the mg-onchain-analysis use case
> because the chain-adapter queries by program ID, not by SPL token owner index. With 256 GB
> you can run all three account indexes but will be close to limit; monitor RSS closely.
> ECC RAM is strongly preferred — silent memory corruption in account-db produces un-diagnosable
> consensus failures.

### CPU chip classes

| Class | Example SKU | Core count | Notes |
|---|---|---|---|
| AMD EPYC 7xx3 (Milan) | EPYC 7443P | 24c/48t | Single-socket, good per-core perf, widely available in colo |
| AMD EPYC 9xx4 (Genoa) | EPYC 9354P | 32c/64t | PCIe Gen5, higher memory bandwidth, pricier |
| Intel Xeon Scalable Gen3 (Ice Lake) | Xeon Gold 6338 | 32c/64t | Meets AVX-512; SHA-NI from Ice Lake |
| AMD Ryzen 9 7950X | desktop, 16c/32t | Cheapest path | No ECC, thermal constraints, not advised for 24/7 production |

Solana requires SHA extensions (SHA-NI) for fast Proof-of-History. AMD EPYC Milan/Genoa and
Intel Ice Lake+ include these natively. Ryzen desktop chips also have SHA-NI but lack ECC.

### NVMe models known to sustain Solana's write pattern

Solana's accounts-db generates high sustained random-write I/O (4KB–64KB random writes at
100K–200K IOPS sustained). Enterprise NVMe drives rated for high Total Bytes Written (TBW)
and high sustained random-write IOPS are required. Consumer NVMe drives throttle severely.

| Drive | Form factor | Sustained rand-write IOPS | TBW (3.84 TB) | Notes |
|---|---|---|---|---|
| Samsung PM9A3 3.84 TB | U.2 / E1.S PCIe Gen4 | ~180K | ~7 PB | Community-confirmed on Solana HCL |
| Kioxia CD8-R 3.84 TB | U.2 PCIe Gen4 | ~200K | ~7 PB | Gen4, read-optimised variant; CD8P-V for write-heavy |
| Kioxia CD8P-V 3.2 TB | U.2 PCIe Gen5 | ~400K | ~17.5 PB | Best-in-class, expensive |
| Solidigm (Intel) D7-P5520 3.84 TB | U.2 PCIe Gen4 | ~170K | ~14 PB | High TBW, good value |
| Micron 7450 Pro 3.84 TB | U.2 PCIe Gen4 | ~150K | ~21.9 PB | Highest TBW in class |

The Solana Hardware Compatibility List (https://github.com/1500256797/solanahcl) is the
community reference for confirmed-working hardware.

### Purchase paths

#### Path A — Bare-metal colo, $200–$400/mo

Self-source the hardware (buy or used), co-locate in a DC with a 1U/2U cage or shared rack.
You own the machine; you pay for rack space, power, and uplink.

**Example providers:** Hivelocity, Shock Hosting, Datacenters.com marketplace, or local
DC in your region. Budget $150–250/mo for 1U colo (1 Gbps burstable or 10 Mbps dedicated)
+ power. Hardware amortised over 3 years brings total-cost-of-ownership to $200–400/mo for
a mid-spec machine (32-core EPYC 7443P, 256 GB ECC, 2x 3.84 TB NVMe U.2).

**Trade-off:** Upfront hardware cost ($4000–8000), your own RMA process, more ops burden.
Best unit economics at 2+ year horizon.

#### Path B — Dedicated cloud server (managed hardware), $600–1000/mo

No hardware purchase. Provider handles RMA, DC, power. You get a dedicated bare-metal server
billed monthly.

**Example vendors and configs (as of 2026-04):**

- **OVHcloud** — AMD EPYC configs with up to 256 GB RAM starting ~$400/mo for 64-core Genoa
  variants; 256 GB DDR5 ECC, NVMe RAID, 1 Gbps public; available in multiple regions.
  (https://us.ovhcloud.com/bare-metal/)
- **Cherry Servers** — AMD EPYC 7443P 24-core with 256 GB ECC, multiple NVMe configs;
  ~$550–700/mo. Used by Everstake for production Agave nodes.
  (https://www.cherryservers.com/bare-metal-dedicated-servers)
- **Latitude.sh** — gen 3 `m3.large.x86` AMD EPYC 7543P 32-core, 1 TB RAM (overspec),
  dual 3.8 TB NVMe, ~$938/mo. Pricey but plug-and-play.
  (https://www.latitude.sh/pricing)

**Trade-off:** No upfront cost, 1-month commitment, provider handles hardware failures.
Higher monthly rate; bandwidth often capped (check included transfer — OVHcloud includes
public bandwidth, some others charge overages on Solana's peering traffic volume).

#### Path C — Hyperscaler bare-metal or large instance, $1500+/mo

AWS `i4i.8xlarge` (32 vCPU, 245 GB RAM, 2x 3.75 TB NVMe, 18.75 Gbps network) runs ~$3/hr
on-demand (~$2200/mo); Reserved 1-year brings it to ~$1400/mo. Google Cloud `n2-standard-32`
with attached local SSD is a similar story. Not recommended — you pay a 3–5x premium over
Path B for equivalent hardware, and hyperscaler ephemeral SSDs have unpredictable I/O
consistency under Solana's write pattern.

Use Path C only if organisational policy requires hyperscaler, or if you need geographic
diversity in a specific cloud region.

---

## 3. Client Choice: Agave vs Firedancer

**Use Agave.** Specifically Agave — the successor to the original Solana Labs validator
client, maintained by Anza. Reasons:

1. **Yellowstone-grpc maturity.** The `rpcpool/yellowstone-grpc` plugin is built and
   released against Agave. The version suffix on every release tag
   (`v12.2.0+solana.3.1.13`) is the Agave version it was compiled against. Firedancer
   has its own plugin interface (Frankendancer); yellowstone-grpc does not yet publish
   Firedancer-native releases.
2. **Stability.** Agave v3.1.x is the current mainnet-stable branch (April 2026).
   Firedancer is advancing rapidly but is still validator-focused; its RPC surface is
   not equivalent to Agave's full JSON-RPC API.
3. **Community tooling.** solana-exporter, grpcurl test suites, all community runbooks —
   written against Agave.

**Future note:** Firedancer is the long-term performance direction. When `rpcpool/yellowstone-grpc`
publishes a Firedancer-compatible plugin release, re-evaluate. Track https://github.com/firedancer-io/firedancer/issues
for geyser plugin support.

---

## 4. Pinned Versions

Every version below is explicitly pinned. Update only after verifying compatibility.

| Component | Version | Notes |
|---|---|---|
| Agave validator | **v3.1.13** | Latest stable mainnet release as of 2026-04-10 |
| Yellowstone-grpc plugin | **v12.2.0+solana.3.1.13** | Suffix must match Agave version |
| Rust toolchain | **1.86.0** | From `rust-toolchain.toml` in yellowstone-grpc repo at pinned tag |
| OS | Ubuntu 22.04 LTS or Debian 12 (bookworm) | Ubuntu 24.04 supported by Anza but Ubuntu 22.04 is more widely validated in community |
| grpcurl | latest via `go install` | Used only for health checks; no pinning required |

To check for newer Agave stable releases:
```
https://github.com/anza-xyz/agave/releases
```
Only use tags without `-rc`, `-beta`, or `-alpha` suffixes for mainnet.

To check for newer yellowstone-grpc releases:
```
https://github.com/rpcpool/yellowstone-grpc/releases
```
The version suffix (`+solana.X.Y.Z`) must match your Agave version exactly. Using a
mismatched plugin version against a different Agave binary will cause the validator to
panic on startup when loading the shared library.

---

## 5. OS Preparation

All commands below run as `root` unless otherwise noted. Later sections switch to the
`sol` user.

### Ubuntu 22.04 LTS

```bash
# Update base system
apt-get update && apt-get upgrade -y

# Install required packages
apt-get install -y \
  build-essential \
  pkg-config \
  libssl-dev \
  libudev-dev \
  libclang-dev \
  cmake \
  protobuf-compiler \
  curl \
  git \
  jq \
  htop \
  iotop \
  nvme-cli \
  grpcurl \
  net-tools \
  lsof \
  screen \
  tmux

# Enable HWE kernel for better hardware support (optional but recommended on 22.04)
# WARNING: upgrading to kernel 6.5+ may rename network interfaces.
# Verify your Netplan config uses MAC address matching before running this.
# apt-get install -y linux-generic-hwe-22.04
```

### Debian 12 (bookworm)

```bash
apt-get update && apt-get upgrade -y

apt-get install -y \
  build-essential \
  pkg-config \
  libssl-dev \
  libudev-dev \
  libclang-dev \
  cmake \
  protobuf-compiler \
  curl \
  git \
  jq \
  htop \
  iotop \
  nvme-cli \
  golang-go \
  net-tools \
  lsof \
  screen \
  tmux

# grpcurl — install from Go on Debian (package may be outdated)
go install github.com/fullstorydev/grpcurl/cmd/grpcurl@latest
export PATH=$PATH:$(go env GOPATH)/bin
```

### Format and mount NVMe drives

Verify your NVMe devices first:
```bash
lsblk -d -o NAME,SIZE,TRAN,MODEL | grep nvme
nvme list
```

Format and mount (adjust device names — do NOT blindly copy these paths):
```bash
# Example: /dev/nvme0n1 for accounts, /dev/nvme1n1 for ledger, /dev/nvme2n1 for snapshots
# DOUBLE-CHECK device names before mkfs — this is destructive

mkfs.ext4 -F /dev/nvme0n1
mkfs.ext4 -F /dev/nvme1n1
mkfs.ext4 -F /dev/nvme2n1

mkdir -p /mnt/accounts /mnt/ledger /mnt/snapshots

# Add to /etc/fstab — get UUIDs with: blkid /dev/nvme0n1
# /dev/nvme0n1 /mnt/accounts ext4 noatime,nodiratime,discard 0 2
# /dev/nvme1n1 /mnt/ledger   ext4 noatime,nodiratime,discard 0 2
# /dev/nvme2n1 /mnt/snapshots ext4 noatime,nodiratime,discard 0 2

# Mount all:
mount -a
```

> **noatime:** Critical. Eliminates inode access-time writes on every read, which are
> extremely frequent in accounts-db. On Solana validators, `atime` updates have been
> measured to reduce accounts-db throughput by 10–15%.
>
> **discard (TRIM):** Enables continuous TRIM for NVMe wear levelling. Some NVMe firmware
> implementations perform better with periodic batch TRIM via `fstrim` cron instead;
> benchmark with `iozoneor` `fio` if you observe write-cliff stalls.

---

## 6. System Tuning

These settings are required. The validator will perform poorly or fail to start without them.

### sysctl — network and VM parameters

```bash
cat > /etc/sysctl.d/21-agave-validator.conf << 'EOF'
# Solana Agave validator tuning
# Source: https://docs.anza.xyz/operations/setup-a-validator/

# UDP receive/send buffer (Solana uses QUIC/UDP heavily)
net.core.rmem_max = 134217728
net.core.wmem_max = 134217728

# Memory-mapped files (accounts-db opens many mmap regions)
vm.max_map_count = 1000000

# Open file descriptors (kernel side)
fs.nr_open = 1000000

# TCP tuning for RPC and gossip
net.core.somaxconn = 65535
net.ipv4.tcp_max_syn_backlog = 65535

# Transparent huge pages — disable; THP causes latency spikes in accounts-db
# Set to madvise instead of always
kernel.numa_balancing = 0
EOF

sysctl -p /etc/sysctl.d/21-agave-validator.conf
```

### ulimits — per-process file descriptor limit

```bash
cat > /etc/security/limits.d/90-solana-nofiles.conf << 'EOF'
# Required for Solana Agave validator
# Source: https://docs.anza.xyz/operations/setup-a-validator/
sol soft nofile 1000000
sol hard nofile 1000000
sol soft memlock 2000000
sol hard memlock 2000000
root soft nofile 1000000
root hard nofile 1000000
EOF
```

Log out and back in after writing this file (or set via systemd `LimitNOFILE` — the
service unit in `systemd/solana-validator.service` already includes these).

### Transparent Huge Pages — disable

```bash
# Disable THP immediately
echo madvise > /sys/kernel/mm/transparent_hugepage/enabled
echo defer+madvise > /sys/kernel/mm/transparent_hugepage/defrag

# Persist across reboots via rc.local or a systemd unit
cat > /etc/systemd/system/disable-thp.service << 'EOF'
[Unit]
Description=Disable Transparent Huge Pages
After=sysinit.target local-fs.target
Before=agave-validator.service

[Service]
Type=oneshot
ExecStart=/bin/sh -c 'echo madvise > /sys/kernel/mm/transparent_hugepage/enabled'
ExecStart=/bin/sh -c 'echo defer+madvise > /sys/kernel/mm/transparent_hugepage/defrag'
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now disable-thp.service
```

### CPU governor — performance mode

```bash
# Install cpufrequtils if not present
apt-get install -y cpufrequtils

# Set to performance for all cores immediately
for cpu in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
  echo performance > "$cpu"
done

# Persist: add to GRUB_CMDLINE_LINUX_DEFAULT in /etc/default/grub
# For AMD EPYC (amd_pstate driver, kernel 5.17+):
#   GRUB_CMDLINE_LINUX_DEFAULT="quiet amd_pstate=active cpufreq.default_governor=performance"
# For Intel:
#   GRUB_CMDLINE_LINUX_DEFAULT="quiet intel_pstate=active cpufreq.default_governor=performance"
# Then: update-grub && reboot
```

Verify after setting:
```bash
cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor
# Expected: performance
```

---

## 7. User + Directory Layout

```bash
# Create dedicated user — no login shell, no sudo
useradd -m -d /home/sol -s /usr/sbin/nologin sol

# Create directory structure
mkdir -p \
  /home/sol/bin \
  /home/sol/keypairs \
  /home/sol/config \
  /home/sol/logs \
  /home/sol/checkpoints

# Mount points are already chowned from §5; set sol as owner
chown -R sol:sol /mnt/accounts /mnt/ledger /mnt/snapshots
chown -R sol:sol /home/sol

# Plugin binary will live here
mkdir -p /home/sol/yellowstone-grpc/target/release
```

Directory map:

| Path | Purpose |
|---|---|
| `/home/sol/bin/` | Agave binaries (symlinked from install) |
| `/home/sol/keypairs/` | `identity-keypair.json` (no vote keypair needed) |
| `/home/sol/config/` | Plugin config, startup script |
| `/home/sol/logs/` | `agave-validator.log` (rotate with logrotate) |
| `/home/sol/checkpoints/` | mg-onchain-analysis `solana.json` checkpoint |
| `/mnt/accounts/` | accounts-db (dedicated NVMe) |
| `/mnt/ledger/` | ledger (dedicated NVMe) |
| `/mnt/snapshots/` | snapshot downloads and staging |

---

## 8. Rust Toolchain

The Yellowstone-grpc plugin must be compiled with the exact Rust version it pins.
As of the pinned tag `v12.2.0+solana.3.1.13`, the required toolchain is **1.86.0**.

Install rustup as the `sol` user (or a build user — do NOT install rustup as root):

```bash
su - sol -s /bin/bash << 'EOF'
set -eo pipefail
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
source "$HOME/.cargo/env"
rustup toolchain install 1.86.0 --component rustfmt clippy
rustup default 1.86.0
rustc --version  # must print: rustc 1.86.0 (...)
EOF
```

The `rust-toolchain.toml` in the yellowstone-grpc repository pins the version automatically
when you run `cargo build` from inside the cloned directory. The manual `rustup default` step
above ensures the correct toolchain is available before clone.

---

## 9. Install Agave Validator Binary

Agave provides prebuilt binaries for Ubuntu x86_64. Compile from source only if you are on
a non-x86_64 architecture or need custom flags.

```bash
# Run as root or sol — installs to /home/sol/.local/share/solana/install/active_release/bin/
su - sol -s /bin/bash << 'EOF'
set -eo pipefail
source "$HOME/.cargo/env"

AGAVE_VERSION="v3.1.13"

# Install via the official install script
curl -sSfL https://release.anza.xyz/${AGAVE_VERSION}/install | sh

# The installer modifies ~/.profile; source it
source "$HOME/.profile"

# Verify
agave-validator --version
# Expected: agave-validator 3.1.13 (src:XXXXXXXX; feat:XXXXXXXXX, client:Agave)

solana --version
# Expected: solana-cli 3.1.13 (src:XXXXXXXX)
EOF
```

Symlink for convenience (optional):
```bash
AGAVE_BIN="/home/sol/.local/share/solana/install/active_release/bin"
ln -sf "$AGAVE_BIN/agave-validator"   /home/sol/bin/agave-validator
ln -sf "$AGAVE_BIN/solana"            /home/sol/bin/solana
ln -sf "$AGAVE_BIN/solana-keygen"     /home/sol/bin/solana-keygen
ln -sf "$AGAVE_BIN/agave-ledger-tool" /home/sol/bin/agave-ledger-tool
```

---

## 10. Generate Identity Keypair

The identity keypair identifies this node on gossip. It does NOT need SOL balance for a
non-voting RPC node. Keep it backed up — losing it means gossip peers will treat you as
a new unknown node after regeneration.

```bash
su - sol -s /bin/bash << 'EOF'
set -eo pipefail
source "$HOME/.profile"

solana-keygen new \
  --no-bip39-passphrase \
  --outfile "$HOME/keypairs/identity-keypair.json"

# Print the public key — record this
solana-keygen pubkey "$HOME/keypairs/identity-keypair.json"
EOF

# Lock down the file
chmod 600 /home/sol/keypairs/identity-keypair.json
```

---

## 11. Build Yellowstone-gRPC Plugin

The plugin is a Rust shared library (`libyellowstone_grpc_geyser.so`) loaded at validator
startup via `--geyser-plugin-config`. It must be compiled against the same Agave version
as the running validator binary. Use the release tag that matches.

```bash
su - sol -s /bin/bash << 'EOF'
set -eo pipefail
source "$HOME/.cargo/env"
source "$HOME/.profile"

YELLOWSTONE_TAG="v12.2.0+solana.3.1.13"
CLONE_DIR="$HOME/yellowstone-grpc"

# Clone at pinned tag
git clone \
  --depth 1 \
  --branch "${YELLOWSTONE_TAG}" \
  https://github.com/rpcpool/yellowstone-grpc.git \
  "${CLONE_DIR}"

cd "${CLONE_DIR}"

# Verify toolchain matches (rust-toolchain.toml pins 1.86.0)
rustc --version

# Build only the geyser plugin crate in release mode
# This will take 5–15 minutes depending on CPU core count
cargo build --release -p yellowstone-grpc-geyser

# Confirm the .so was produced
ls -lh target/release/libyellowstone_grpc_geyser.so
EOF
```

The resulting `.so` path is:
```
/home/sol/yellowstone-grpc/target/release/libyellowstone_grpc_geyser.so
```

This path is referenced in the plugin config and in `config/validator-startup.sh`.

---

## 12. Configure the Plugin

Copy the example config and edit it:

```bash
cp /path/to/this/repo/infra/solana-validator/config/yellowstone-grpc-config.json.example \
   /home/sol/config/yellowstone-grpc-config.json

chown sol:sol /home/sol/config/yellowstone-grpc-config.json
chmod 640 /home/sol/config/yellowstone-grpc-config.json
```

Review `config/yellowstone-grpc-config.json.example` in this directory for the full
annotated template. Key parameters to verify before starting:

| Parameter | Expected value | Why |
|---|---|---|
| `grpc.address` | `"0.0.0.0:10000"` | Binds on all interfaces; firewall externally |
| `grpc.tls` | `null` | Plaintext; trusted private network |
| `prometheus.address` | `"0.0.0.0:8999"` | Scrape target for monitoring |
| `libpath` | Absolute path to `.so` | Must be exact or validator panics at startup |

---

## 13. Snapshot Sync

Initial sync requires downloading a recent snapshot (~100–400 GB compressed) and then
replaying account updates to catch up to tip. This is the slowest step.

### Download snapshot

Agave downloads a snapshot automatically on first start when given `--known-validator`
flags. However, pre-downloading via `solana-ledger-tool` gives more control and visibility.

**Method A — Let the validator download automatically (simpler)**

Set `--snapshot-interval-slots 0` to disable local snapshot creation (saves disk on
initial sync), start the validator with the startup script, and watch the log:

```bash
# Start and tail logs — see §14 and §15 for the full startup
journalctl -fu agave-validator | grep -E 'snapshot|Downloading|slot|error'
```

The validator prints download progress as it fetches from known validators. On gigabit,
expect 30 minutes to 4 hours for the compressed download, then 4–24 hours for
account-db replay.

**Method B — Manual snapshot download and ledger-tool verification (recommended)**

```bash
su - sol -s /bin/bash << 'EOF'
set -eo pipefail
source "$HOME/.profile"

# Fetch the list of snapshot download URLs from a known validator
# This contacts gossip to find peers willing to serve snapshots
solana catchup --our-localhost 8899 --follow 2>/dev/null || true

# Use agave-ledger-tool to verify a downloaded snapshot
# (run after validator has downloaded snapshot to /mnt/snapshots/)
agave-ledger-tool \
  --ledger /mnt/ledger \
  verify \
  --snapshot-archive-path /mnt/snapshots

echo "Snapshot verification complete"
EOF
```

### Expected sync duration

| Phase | Gigabit | 10 Gbps |
|---|---|---|
| Snapshot download (compressed ~200 GB) | 30 min – 4 hr | 5–30 min |
| Account-db replay (decompression + verify) | 4–12 hr | 4–8 hr |
| Catch-up to tip (slot replay) | 6–24 hr | 2–6 hr |
| **Total first-start** | **~24–48 hr** | **~8–16 hr** |

Replay time is CPU and NVMe bound, not network bound. A 32-core EPYC will replay faster
than a 16-core; NVMe sustained write throughput is the other bottleneck.

### Resume on failure

If the validator is interrupted during sync:

```bash
# Restart the systemd service — it picks up where it left off
systemctl restart agave-validator

# Check if ledger is intact
su - sol -s /bin/bash -c \
  "agave-ledger-tool --ledger /mnt/ledger bounds"
```

If the ledger is corrupted (common if killed during write):
```bash
# Use wal-recovery-mode to skip corrupted records
# Add --wal-recovery-mode skip_any_corrupted_record to startup flags
# Already included in config/validator-startup.sh
systemctl restart agave-validator
```

---

## 14. Validator Startup

The complete startup command is in `config/validator-startup.sh`. Review it before
first run. Key flags explained:

| Flag | Value | Explanation |
|---|---|---|
| `--no-voting` | (present) | RPC-only; no consensus participation |
| `--full-rpc-api` | (present) | Enables all RPC methods |
| `--rpc-port` | `8899` | Standard Solana JSON-RPC port |
| `--rpc-bind-address` | `0.0.0.0` | Bind on all interfaces; firewall externally |
| `--private-rpc` | (present) | Suppresses gossip advertisement of RPC port |
| `--expected-genesis-hash` | `5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d` | Mainnet-beta genesis; refuses to start against wrong cluster |
| `--known-validator` | 4 pubkeys | Bootstrap trust anchors; refuses malicious snapshots |
| `--only-known-rpc` | (present) | Only downloads snapshots from known validators |
| `--entrypoint` | 5 mainnet endpoints | Gossip bootstrap |
| `--ledger` | `/mnt/ledger` | Ledger NVMe mount |
| `--accounts` | `/mnt/accounts` | Accounts-db NVMe mount |
| `--snapshots` | `/mnt/snapshots` | Snapshot staging |
| `--geyser-plugin-config` | `/home/sol/config/yellowstone-grpc-config.json` | Loads Yellowstone plugin |
| `--wal-recovery-mode` | `skip_any_corrupted_record` | Recovers from unclean shutdown |
| `--limit-ledger-size` | (present) | Prunes ledger to recent slots; controls disk growth |
| `--account-index` | `program-id` | Enables fast program-ID queries (used by chain-adapter) |
| `--log` | `/home/sol/logs/agave-validator.log` | Log file; rotated by logrotate |

### Mainnet-beta known validators

These are the bootstrap trust anchors used in `config/validator-startup.sh`:

```
7Np41oeYqPefeNQEHSv1UDhYrehxin3NStELsSKCT4K2   # Solana Foundation
GdnSyH3YtwcxFvQrVVJMm1JhTS4QVX7MFsX56uJLUfiZ   # Solana Foundation
DE1bawNcRJB9rVm3buyMVfr8mBEoyyu73NBovf2oXJsJ    # Solana Foundation
CakcnaRDHka2gXyfbEd2d3xsvkJkqsLw2akB3zsN1D2S    # Solana Foundation
```

These are the current foundation-operated validators. They are subject to change on
cluster upgrades. Verify against `solana gossip` output or the official cluster page
after any Agave version upgrade.

### Genesis hash (mainnet-beta)

```
5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d
```

This is a permanent constant for mainnet-beta. It will never change for the mainnet-beta cluster.

---

## 15. Install systemd Unit

```bash
# Copy the service file
cp /path/to/this/repo/infra/solana-validator/systemd/solana-validator.service \
   /etc/systemd/system/agave-validator.service

# Copy the startup script
cp /path/to/this/repo/infra/solana-validator/config/validator-startup.sh \
   /home/sol/config/validator-startup.sh

chmod +x /home/sol/config/validator-startup.sh
chown sol:sol /home/sol/config/validator-startup.sh

# Copy the plugin config
cp /path/to/this/repo/infra/solana-validator/config/yellowstone-grpc-config.json.example \
   /home/sol/config/yellowstone-grpc-config.json
# EDIT the config before enabling (see §12)

# Enable and start
systemctl daemon-reload
systemctl enable agave-validator
systemctl start agave-validator

# Watch startup
journalctl -fu agave-validator
```

### logrotate for validator log

```bash
cat > /etc/logrotate.d/agave-validator << 'EOF'
/home/sol/logs/agave-validator.log {
    daily
    rotate 7
    compress
    delaycompress
    missingok
    notifempty
    copytruncate
}
EOF
```

---

## 16. Health Checks

Run these after the validator has been online for at least 30 minutes post-snapshot-sync.

### 16.1 — Gossip visibility

```bash
# Confirm this node appears in gossip network
# Replace <IDENTITY_PUBKEY> with output of: solana-keygen pubkey /home/sol/keypairs/identity-keypair.json
IDENTITY_PUBKEY="$(su - sol -s /bin/bash -c 'source ~/.profile && solana-keygen pubkey ~/keypairs/identity-keypair.json')"

solana gossip --url http://localhost:8899 | grep "${IDENTITY_PUBKEY}"
# Expected: one line showing your node's IP, ports, and version
```

### 16.2 — RPC slot responsiveness

```bash
curl -s http://localhost:8899 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getSlot","params":[{"commitment":"confirmed"}]}' \
  | jq .result

# Expected: a slot number close to current tip (within 50 slots)
# Check current tip at: https://explorer.solana.com/
```

### 16.3 — gRPC Yellowstone stream

Install grpcurl if not present: `go install github.com/fullstorydev/grpcurl/cmd/grpcurl@latest`

```bash
# List available gRPC services (reflection must be enabled — it is by default in yellowstone-grpc)
grpcurl --plaintext localhost:10000 list
# Expected: geyser.Geyser (and possibly other services)

# Subscribe and verify one slot update arrives within 5 seconds
grpcurl \
  --plaintext \
  -d '{"slots":{"all_slots":{"filter_by_commitment":false}},"commitment":1}' \
  localhost:10000 \
  geyser.Geyser/Subscribe
# Expected: streaming JSON lines with slot updates; Ctrl+C to exit
```

If the gRPC call hangs with no response, the plugin did not load. Check:
```bash
journalctl -u agave-validator | grep -i 'geyser\|plugin\|yellowstone\|error' | tail -50
```

### 16.4 — Slot lag check

```bash
# Compare local slot to network tip
LOCAL_SLOT=$(curl -s http://localhost:8899 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getSlot","params":[{"commitment":"confirmed"}]}' \
  | jq -r .result)

NETWORK_SLOT=$(curl -s https://api.mainnet-beta.solana.com \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getSlot","params":[{"commitment":"confirmed"}]}' \
  | jq -r .result)

LAG=$((NETWORK_SLOT - LOCAL_SLOT))
echo "Local: ${LOCAL_SLOT}, Network: ${NETWORK_SLOT}, Lag: ${LAG} slots"

# Healthy: lag < 50 slots (at 0.4s/slot, 50 slots = ~20 seconds)
# Concern: lag 50-150 slots — possible I/O or CPU bottleneck
# Critical: lag > 150 slots — validator is falling behind; investigate
```

---

## 17. Monitoring

See `monitoring/` directory for:
- `prometheus.yml.example` — scrape config
- `grafana-dashboard.json` — pre-built dashboard with critical panels
- `alerts.yml.example` — Prometheus alert rules

### Metrics ports

| Service | Port | Endpoint |
|---|---|---|
| agave-validator (built-in) | `1234` | `/metrics` (Prometheus format) |
| yellowstone-grpc plugin | `8999` | `/metrics` (Prometheus format) |
| node_exporter | `9100` | `/metrics` |

### Quick Prometheus install (standalone binary)

```bash
# Install prometheus
PROMETHEUS_VERSION="2.53.0"
wget -qO- https://github.com/prometheus/prometheus/releases/download/v${PROMETHEUS_VERSION}/prometheus-${PROMETHEUS_VERSION}.linux-amd64.tar.gz \
  | tar -xz -C /opt/
ln -sf /opt/prometheus-${PROMETHEUS_VERSION}.linux-amd64 /opt/prometheus

# Install node_exporter
NODE_EXPORTER_VERSION="1.8.2"
wget -qO- https://github.com/prometheus/node_exporter/releases/download/v${NODE_EXPORTER_VERSION}/node_exporter-${NODE_EXPORTER_VERSION}.linux-amd64.tar.gz \
  | tar -xz -C /opt/
ln -sf /opt/node_exporter-${NODE_EXPORTER_VERSION}.linux-amd64 /opt/node_exporter

# Copy and edit the prometheus config
cp /path/to/this/repo/infra/solana-validator/monitoring/prometheus.yml.example \
   /opt/prometheus/prometheus.yml
```

### Grafana

Install Grafana OSS via APT (Ubuntu/Debian):
```bash
apt-get install -y apt-transport-https software-properties-common
wget -q -O - https://apt.grafana.com/gpg.key | gpg --dearmor > /usr/share/keyrings/grafana.gpg
echo "deb [signed-by=/usr/share/keyrings/grafana.gpg] https://apt.grafana.com stable main" \
  > /etc/apt/sources.list.d/grafana.list
apt-get update && apt-get install -y grafana
systemctl enable --now grafana-server
```

Import `monitoring/grafana-dashboard.json` via Grafana UI:
Settings → Dashboards → Import → Upload JSON file.

---

## 18. Troubleshooting

### Slot lag growing indefinitely

**Symptom:** `getSlot` falls further and further behind network tip.

**Diagnose:**
```bash
# CPU saturation
top -b -n 1 -p $(pgrep agave-validator) | head -20

# Disk I/O saturation — look at /mnt/accounts and /mnt/ledger
iostat -x 2 5

# Memory pressure
free -h
# If "available" < 10 GB, the system is swapping — fatal for accounts-db
```

**Fixes:**
- CPU bottleneck: check CPU governor is `performance`, not `powersave`
- NVMe saturated: verify accounts and ledger are on separate drives; check `nvme list` for throttled drives
- Memory: reduce `--account-index` flags to free RAM; add more RAM if possible

### OOM killer events

**Symptom:** Validator process disappears; `dmesg | grep -i 'oom\|killed'` shows OOM kill.

**Diagnose:**
```bash
dmesg | grep -E 'oom|killed|Out of memory' | tail -20
journalctl -u agave-validator --since "1 hour ago" | grep -i 'error\|killed'
```

**Fixes:**
- Reduce `--account-index` flags (each index consumes ~10–30 GB RAM)
- Add swap as emergency relief (NVMe swap — NOT a substitute for RAM, but prevents hard OOM):
  ```bash
  fallocate -l 64G /mnt/ledger/swapfile
  chmod 600 /mnt/ledger/swapfile
  mkswap /mnt/ledger/swapfile
  swapon /mnt/ledger/swapfile
  echo '/mnt/ledger/swapfile none swap sw 0 0' >> /etc/fstab
  ```
- Increase physical RAM (root fix)

### Snapshot download fails

**Symptom:** Log shows `SnapshotDownload failed` or validator exits at startup.

**Diagnose:**
```bash
journalctl -u agave-validator | grep -i 'snapshot\|download\|failed' | tail -30
```

**Fixes:**
- Verify `/mnt/snapshots` has sufficient free space (at least 500 GB)
- Check network connectivity to known validators via gossip: `solana gossip --url https://api.mainnet-beta.solana.com | grep 8899 | head -5`
- Remove partial snapshots: `rm -f /mnt/snapshots/snapshot-*`
- Retry by restarting the service: `systemctl restart agave-validator`

### Yellowstone plugin fails to load

**Symptom:** gRPC health check returns "connection refused" or no slot events arrive.
Log shows `failed to load plugin` or `libpath not found`.

**Diagnose:**
```bash
journalctl -u agave-validator | grep -i 'geyser\|plugin\|libpath\|yellowstone' | tail -30

# Verify the .so exists
ls -lh /home/sol/yellowstone-grpc/target/release/libyellowstone_grpc_geyser.so

# Verify JSON is valid
jq . /home/sol/config/yellowstone-grpc-config.json > /dev/null && echo "JSON valid"
```

**Fixes:**
- `libpath` in the JSON config must be an **absolute path** — never relative
- Recompile if Agave version was upgraded and plugin `.so` version mismatches:
  ```bash
  cd /home/sol/yellowstone-grpc && cargo build --release -p yellowstone-grpc-geyser
  systemctl restart agave-validator
  ```
- Port 10000 conflict: `ss -tlnp | grep 10000`

### Plugin version mismatch (validator panics at startup)

**Symptom:** Validator exits immediately on startup after an Agave upgrade.
Log shows `SIGSEGV` or `geyser plugin panic`.

**Fix:** The yellowstone-grpc `.so` must match the Agave version. After any Agave upgrade:
1. Find the new matching yellowstone-grpc tag on https://github.com/rpcpool/yellowstone-grpc/releases
2. Checkout the new tag: `git -C /home/sol/yellowstone-grpc fetch --tags && git -C /home/sol/yellowstone-grpc checkout <new-tag>`
3. Rebuild: `cargo build --release -p yellowstone-grpc-geyser`
4. Restart: `systemctl restart agave-validator`

### gRPC backpressure / slow consumer

**Symptom:** `chain-adapter` logs `channel full` or `send timeout`; plugin metrics show
high `channel_capacity_overflow` counter.

**Fix:** Increase `channel_capacity` in `yellowstone-grpc-config.json`:
```json
"channel_capacity": "500_000"
```
And ensure the `chain-adapter` consumer is processing fast enough (check CPU on the
consumer host, check Postgres write latency).

---

## 19. References

All external claims in this runbook are grounded against these sources.

| # | URL | What was extracted |
|---|---|---|
| 1 | https://github.com/anza-xyz/agave/releases | Pinned Agave stable version: v3.1.13 (latest stable as of 2026-04-10, per release listing) |
| 2 | https://github.com/rpcpool/yellowstone-grpc/releases | Pinned yellowstone-grpc tag: v12.2.0+solana.3.1.13 (latest stable matching Agave 3.1.13, as of 2026-04-10) |
| 3 | https://raw.githubusercontent.com/rpcpool/yellowstone-grpc/v12.2.0%2Bsolana.3.1.13/rust-toolchain.toml | Rust toolchain version: 1.86.0 (pinned in repo at the tagged release) |
| 4 | https://docs.anza.xyz/operations/requirements | Hardware requirements: CPU (16c/32t, 2.8 GHz+, AVX2, SHA-NI), RAM (512 GB recommended, ECC), Disk (separate NVMe mounts for accounts/ledger/snapshots), Network (1 Gbps min, 10 Gbps preferred) |
| 5 | https://docs.anza.xyz/operations/setup-a-validator/ | sysctl params (rmem_max, wmem_max, max_map_count, fs.nr_open), ulimits, CPU governor configuration, ext4 mount with separate ledger/accounts, pre-flight checklist |
| 6 | https://docs.anza.xyz/operations/setup-an-rpc-node | Complete RPC startup flags: --no-voting, --full-rpc-api, --private-rpc, --account-index options |
| 7 | https://github.com/CryptoManufaktur-io/solana-rpc/blob/main/start-validator.sh | Known-validator pubkeys (4 foundation addresses), mainnet entrypoints (5), genesis hash (5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d) |
| 8 | https://solana.com/docs/references/clusters | Genesis hash confirmation, entrypoint list for mainnet-beta |
| 9 | https://github.com/1500256797/solanahcl | Solana Hardware Compatibility List — NVMe models (Samsung PM9A3, Kioxia CD8, Micron 7450) sustained IOPS confirmation |
| 10 | https://www.storagereview.com/review/samsung-pm9a3-ssd-review | Samsung PM9A3: random 4KB write ~180K IOPS sustained |
| 11 | https://europe.kioxia.com/en-europe/business/ssd/data-center-ssd/cd8-r.html | Kioxia CD8-R: random read 1.25M IOPS, write 200K IOPS; CD8P-V up to 400K write IOPS |
| 12 | https://us.ovhcloud.com/bare-metal/ | OVHcloud bare-metal: AMD EPYC configs with 256 GB ECC DDR5, NVMe; ~$400+/mo for larger Genoa configs |
| 13 | https://www.cherryservers.com/bare-metal-dedicated-servers | Cherry Servers: EPYC 7443P 24-core, 256 GB, NVMe configs; ~$550–700/mo; used by Everstake for Agave production |
| 14 | https://www.latitude.sh/pricing | Latitude.sh: m3.large.x86 AMD EPYC 7543P 32c, 1 TB RAM, dual 3.8 TB NVMe, $938/mo |
| 15 | https://grafana.com/grafana/dashboards/19236-solana-validator-dashboard/ | Grafana dashboard ID 19236 — community Solana validator dashboard; panels and metric names used as reference for grafana-dashboard.json |
| 16 | https://rustiqtech.github.io/solana-exporter/basics/prometheus.html | solana-exporter metrics port (9179), Prometheus scrape target format |
| 17 | https://everstake.one/blog/how-to-maximize-agave-node-performance-with-cherry-servers | Cherry Servers + Agave production optimization; CPU governor settings, amd_pstate active mode |
| 18 | https://github.com/rpcpool/yellowstone-grpc | geyser plugin config format: libpath, grpc.address, prometheus.address, channel_capacity, x_token |
| 19 | docs/adr/0003-self-sovereign-infrastructure.md | Binding ADR: hardware spec table, cost ranges, no 3rd-party providers in hot path |
| 20 | docs/adr/0001-phase0-synthesis.md §D2 | Yellowstone gRPC as ingestion protocol; provider-agnostic; self-hosted default under ADR 0003 |
| 21 | config/adapters.toml.example | Default endpoint grpc://localhost:10000; auth_token empty; rpc_endpoint http://localhost:8899; commitment confirmed |
