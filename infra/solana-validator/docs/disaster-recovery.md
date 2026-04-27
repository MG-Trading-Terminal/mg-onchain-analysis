# Disaster Recovery — Solana Validator

**Scope:** Non-voting Agave RPC node with Yellowstone gRPC plugin.
**Covers:** State backup strategy, cold-start from backup, RTO/RPO estimates, data loss window.

---

## What State Exists

| State | Location | Size | Reproducible? |
|---|---|---|---|
| accounts-db | `/mnt/accounts` | 200–600 GB | Yes — re-sync from network |
| ledger (recent slots) | `/mnt/ledger` | 200–800 GB | Yes — re-sync from network |
| snapshots (staged) | `/mnt/snapshots` | 100–400 GB compressed | Yes — re-download |
| identity keypair | `/home/sol/keypairs/identity-keypair.json` | bytes | NO — back up immediately |
| plugin config | `/home/sol/config/yellowstone-grpc-config.json` | bytes | Yes — in this repo |
| chain-adapter checkpoint | `/home/sol/checkpoints/solana.json` | bytes | Yes — re-derives from ledger |

**The only irreplaceable state is the identity keypair.** Everything else re-syncs from the
Solana network, at the cost of 24–48 hours of downtime. If the keypair is lost, gossip peers
see a new unknown node; no functional impact for a non-voting RPC node beyond a temporary
increase in gossip introduction delay.

---

## Backup Strategy

### Tier 1 — Identity keypair (critical, tiny)

Back up immediately after generation. Store encrypted copies in at least two locations:

```bash
# Encrypt with a strong passphrase
gpg --symmetric --cipher-algo AES256 \
  /home/sol/keypairs/identity-keypair.json
# Produces: identity-keypair.json.gpg

# Store encrypted copy offsite — e.g. S3, Backblaze B2, or a USB in a safe
# Example: Backblaze B2 (S3-compatible, ~$6/TB/month)
aws s3 cp identity-keypair.json.gpg \
  s3://your-bucket/solana/identity-keypair.json.gpg \
  --sse aws:kms
```

Verify decryption works before relying on the backup:
```bash
gpg --decrypt identity-keypair.json.gpg > /tmp/test-restore.json
diff /home/sol/keypairs/identity-keypair.json /tmp/test-restore.json && echo "backup verified"
rm /tmp/test-restore.json
```

**Backup frequency:** Once (keypair does not change). Re-backup if you rotate the identity.

### Tier 2 — Operational config (low priority)

All config lives in this repository under `infra/solana-validator/`. The only
instance-specific file is the resolved `yellowstone-grpc-config.json` (no secrets in it).
Commit any local changes to this repo. Recovery is `git clone` + copy.

### Tier 3 — accounts-db incremental snapshots (optional, expensive)

Solana's accounts-db is several hundred GB. Incremental snapshot uploads reduce RTO from
48h (full network sync) to 2–4h (resume from snapshot). This is worthwhile if you are
operationally required to have RTO < 12h.

**Cost estimate:**

| Storage | Size | Price (Backblaze B2, ~$6/TB/mo) | Price (AWS S3, ~$23/TB/mo) |
|---|---|---|---|
| Full snapshot (compressed) | ~200 GB | ~$1.20/mo | ~$4.60/mo |
| Delta snapshots (7 days, ~20 GB each) | ~140 GB | ~$0.84/mo | ~$3.22/mo |
| **Total** | ~340 GB | ~$2.00/mo | ~$7.82/mo |

Validator produces snapshots at `--snapshot-interval-slots 300` (every 300 slots, ~2 min).
The startup flag `--maximum-snapshots-to-retain 2` keeps the last 2 local; the rest can be
uploaded and pruned.

**Upload script (run from cron or a systemd timer):**

```bash
#!/usr/bin/env bash
# upload-snapshots.sh — upload new snapshots to object storage
# Run every 5 minutes via cron or systemd timer.
set -eo pipefail

SNAPSHOT_DIR="/mnt/snapshots"
BUCKET="s3://your-bucket/solana/snapshots"

# Upload any snapshot archives not yet uploaded
find "${SNAPSHOT_DIR}" -name "snapshot-*.tar.zst" -newer /tmp/last-snapshot-upload \
  -exec aws s3 cp {} "${BUCKET}/" \;

touch /tmp/last-snapshot-upload

# Prune remote snapshots older than 7 days to control storage cost
aws s3 ls "${BUCKET}/" \
  | awk '{print $4}' \
  | while read -r key; do
      # Parse slot number from filename snapshot-<slot>-<hash>.tar.zst
      # Keep only the 50 most recent by listing and trimming
      true
    done
# Note: full prune logic depends on your retention policy.
# A simpler approach: use S3 lifecycle rules to delete objects older than 7 days.
```

**S3 lifecycle rule (AWS console or Terraform):**
```json
{
  "Rules": [{
    "ID": "expire-old-snapshots",
    "Filter": { "Prefix": "solana/snapshots/" },
    "Status": "Enabled",
    "Expiration": { "Days": 7 }
  }]
}
```

### Tier 4 — No ledger backup

The ledger (`/mnt/ledger`) is not backed up. It is regenerated during snapshot sync.
The `--limit-ledger-size` flag keeps it bounded. No backup value for a non-archive node.

---

## Cold-Start from Backup

### Scenario A — Disk failure on accounts NVMe, identity keypair intact

**RTO estimate:** 24–48 hours (full network sync) OR 2–4 hours if Tier 3 snapshot backup exists.

```bash
# 1. Replace/reformat the failed NVMe
mkfs.ext4 -F /dev/nvme0n1
mount /dev/nvme0n1 /mnt/accounts
chown sol:sol /mnt/accounts

# 2a. If Tier 3 snapshots are available: download latest snapshot
aws s3 cp s3://your-bucket/solana/snapshots/snapshot-<SLOT>-<HASH>.tar.zst \
  /mnt/snapshots/

# 2b. Otherwise: delete any partial state and restart validator
# The validator will download a fresh snapshot from known validators automatically

# 3. Start validator
systemctl start agave-validator
journalctl -fu agave-validator | grep -E 'snapshot|Downloading|slot'
```

### Scenario B — Full machine failure, restore to new hardware

**RTO estimate:** Hardware provisioning (hours to days) + 24–48 hours sync.

```bash
# On new machine:
# 1. Provision OS, install dependencies per README.md §5
# 2. Restore identity keypair from encrypted backup
gpg --decrypt identity-keypair.json.gpg > /home/sol/keypairs/identity-keypair.json
chmod 600 /home/sol/keypairs/identity-keypair.json

# 3. Install Agave + build Yellowstone plugin per README.md §9-11
# 4. Copy configs from this repo
# 5. If Tier 3 snapshot available, download it to /mnt/snapshots/
# 6. Start validator
systemctl start agave-validator
```

### Scenario C — Identity keypair lost

**Impact:** None functionally (non-voting node). Gossip peers treat the node as a new
unknown peer and re-learn its address. No SOL at risk.

```bash
# Generate a new keypair
solana-keygen new --no-bip39-passphrase \
  --outfile /home/sol/keypairs/identity-keypair.json
# Back it up immediately (see Tier 1 above)

# Restart validator with new identity
systemctl restart agave-validator
```

---

## RPO / RTO Summary

| Scenario | RPO (data loss window) | RTO (restore time) | Notes |
|---|---|---|---|
| Clean restart (planned) | 0 (checkpoint survives) | < 5 minutes | `systemctl restart` |
| OOM crash, disk intact | < 5 minutes (last checkpoint) | 5–15 min | Validator auto-restarts |
| Accounts NVMe failure, no backup | 0 (on-chain data) | 24–48 hours | Re-sync from network |
| Accounts NVMe failure, snapshot backup | 0 (on-chain data) | 2–4 hours | Restore latest snapshot |
| Full machine failure | 0 (on-chain data) | HW + 24–48 hours | Hardware is the bottleneck |
| Identity keypair lost | 0 (non-voting, no SOL at stake) | < 1 hour | Generate new keypair |

**Key insight:** For a non-voting RPC node, "data loss" is not a meaningful concept — all
on-chain data is public and re-derivable by replaying from genesis or a recent snapshot.
The RTO is entirely driven by snapshot sync time. Tier 3 snapshot backups are the only
lever to pull RTO below 24 hours.

---

## Chain-Adapter Failover During Downtime

When the validator is offline, the `chain-adapter` loses its gRPC stream and begins emitting
`AdapterError::StreamEnded`. The indexer enters a "dark" state per ADR 0003:

- **Short outage (< 5 minutes):** The adapter's reconnect loop (`reconnect.rs`) retries with
  exponential backoff. No manual intervention needed. The `from_slot` resume mechanism
  ensures no events are missed when the stream reconnects.

- **Extended outage (> 5 minutes):** Detectors stop receiving new events. Alert fires
  (`SolanaValidatorDown`). No automatic failover to a 3rd-party provider (ADR 0003 policy).
  The team must either restore the validator or accept dark detectors until it comes back.

- **Manual bootstrap fallback (per ADR 0003):** During validator recovery, one-off fixture
  capture from `api.mainnet-beta.solana.com` (public Anza RPC, rate-limited) is tolerated
  for discovery calls. This is NOT a live detection path — it is read-only, low-rate,
  temporary scaffolding only.

To temporarily re-enable the public RPC endpoint (dev-bootstrap mode), edit
`config/adapters.toml` and uncomment the Anza public RPC block. Remove this before
returning to production. See `config/adapters.toml.example` warning blocks.
