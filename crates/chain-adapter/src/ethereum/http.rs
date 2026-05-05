//! HTTP JSON-RPC on-demand fetchers for EVM tokens.
//!
//! Mirrors the pull-based pattern used by `solana::subscribe::get_token_holders`
//! / `get_mint_state`: a single HTTP RPC round-trip per detector input,
//! callable by the `onchain-check-token` CLI binary against a self-hosted
//! Reth/Geth/BSC node (or any standards-conformant Ethereum JSON-RPC
//! endpoint).
//!
//! These functions DO NOT use the WS-based [`crate::ethereum::EthereumRpc`]
//! trait; the CLI is a one-shot tool that does not need long-lived
//! subscriptions. We open a fresh `reqwest` HTTP client per call and rely on
//! standard `eth_call` + `eth_getCode` per the spec.

use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};

use crate::error::AdapterError;

/// Standard ERC-20 + Ownable function selectors (first 4 bytes of
/// `keccak256("functionName(args)")`).
mod selectors {
    /// `name()` returns string.
    pub const NAME: [u8; 4] = [0x06, 0xfd, 0xde, 0x03];
    /// `symbol()` returns string.
    pub const SYMBOL: [u8; 4] = [0x95, 0xd8, 0x9b, 0x41];
    /// `decimals()` returns uint8.
    pub const DECIMALS: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];
    /// `totalSupply()` returns uint256.
    pub const TOTAL_SUPPLY: [u8; 4] = [0x18, 0x16, 0x0d, 0xdd];
    /// `owner()` returns address (Ownable).
    pub const OWNER: [u8; 4] = [0x8d, 0xa5, 0xcb, 0x5b];
    /// `paused()` returns bool (Pausable).
    pub const PAUSED: [u8; 4] = [0x5c, 0x97, 0x5a, 0xbb];
}

/// Bytecode-search selectors that signal write-access patterns commonly
/// associated with rug/honeypot setup. Presence in deployed code is a
/// necessary-but-not-sufficient signal — the function may be guarded by
/// access control or unreachable from external callers.
pub mod bytecode_selectors {
    // --- Mint surface (D06 mint_burn_anomaly) ---
    /// `mint(uint256)` — common in OpenZeppelin Ownable presets.
    pub const MINT_UINT256: [u8; 4] = [0xa0, 0x71, 0x2d, 0x68];
    /// `mint(address,uint256)` — most common ERC-20 owner mint pattern.
    pub const MINT_ADDR_UINT256: [u8; 4] = [0x40, 0xc1, 0x0f, 0x19];
    /// `issue(uint256)` — Tether USDT's mint method. Validated 2026-04-28
    /// after USDT D06 returned NONE because the standard mint() selectors
    /// were absent.
    pub const ISSUE_UINT256: [u8; 4] = [0xcc, 0x87, 0x2b, 0x66];
    /// `redeem(uint256)` — Tether USDT's burn method (paired with `issue`).
    pub const REDEEM_UINT256: [u8; 4] = [0x74, 0x54, 0x00, 0xc9];

    // --- Honeypot / sell-gate surface (D01 static) ---
    /// `setMaxTxAmount(uint256)` — anti-whale knob, also a sell-block knob.
    pub const SET_MAX_TX_AMOUNT: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb]; // legacy ABI clash; see note.
    /// `blacklist(address,bool)` selector seen in many rug contracts.
    pub const BLACKLIST_BOOL: [u8; 4] = [0xf9, 0xf9, 0x2b, 0xe4];
    /// `setSwapEnabled(bool)` — common gating mechanism on rug honeypots.
    pub const SET_SWAP_ENABLED: [u8; 4] = [0x40, 0x71, 0x82, 0x53];
    /// `pause()` — Pausable extension; used to halt all transfers.
    pub const PAUSE: [u8; 4] = [0x84, 0x56, 0xcb, 0x59];
}

/// Outcome of a simulate-sell probe — D01 honeypot dynamic signal. Three
/// possible terminal states (mutually exclusive):
///
/// - `Success`: transfer simulation succeeded → token is transferable from
///   that sender at that amount under that block tip. Strong negative
///   honeypot evidence.
/// - `Reverted`: transfer simulation reverted with a non-balance reason
///   (e.g. "trading not enabled", "cooldown", "bot detected"). Strong
///   positive honeypot evidence.
/// - `Skipped`: could not run a meaningful test (no candidate sender with
///   non-zero balance, or revert reason is balance-related and therefore
///   uninformative). Inconclusive — D01 falls back to static bytecode
///   signals only.
#[derive(Debug, Clone)]
pub enum SimulateSellOutcome {
    /// Transfer call simulated successfully under EVM rules.
    Success,
    /// Transfer reverted with a reason that does not look balance-related.
    /// `reason` is the ABI-decoded revert string when present, else the
    /// raw error message from the JSON-RPC response.
    Reverted { reason: String },
    /// Probe was skipped — caller should fall back to static signals.
    Skipped { reason: String },
}

/// Token metadata + risk-relevant on-chain state captured in a single
/// per-token CLI run. All fields are `Option<_>` because real ERC-20
/// deployments are inconsistent: not every token implements `name()` /
/// `symbol()`, not every token is Ownable, etc.
#[derive(Debug, Clone)]
pub struct EvmTokenMeta {
    /// Hex-encoded address with `0x` prefix (lowercased).
    pub address: String,
    /// Decoded `name()` if the call returned valid UTF-8 string-encoded data.
    pub name: Option<String>,
    /// Decoded `symbol()`.
    pub symbol: Option<String>,
    /// `decimals()` (u8). `None` if the call reverted.
    pub decimals: Option<u8>,
    /// `totalSupply()` raw u256 as decimal string (preserves precision).
    pub total_supply_raw: Option<String>,
    /// Owner address (Ownable). `None` if `owner()` reverts (not Ownable).
    pub owner: Option<String>,
    /// True iff `owner()` returns `0x0000…0000` — renounced ownership.
    pub owner_renounced: bool,
    /// `paused()` value if Pausable is implemented.
    pub paused: Option<bool>,
    /// Length of `eth_getCode` output in bytes (0 = EOA / not a contract).
    pub bytecode_len: usize,
    /// True iff bytecode contains a 4-byte selector matching `mint(uint256)`
    /// or `mint(address,uint256)` (D06 mint-burn surface).
    pub has_mint_selector: bool,
    /// True iff bytecode contains a `pause()` selector.
    pub has_pause_selector: bool,
    /// True iff bytecode contains a `blacklist(address,bool)` selector.
    pub has_blacklist_selector: bool,
    /// True iff bytecode contains `setSwapEnabled(bool)` (sell-toggle rug).
    pub has_swap_toggle_selector: bool,
    /// `Some(impl_address)` when the contract is an EIP-1967 proxy and we
    /// successfully resolved the implementation slot to a non-zero address.
    /// Selector flags above include selectors found in EITHER the proxy
    /// bytecode OR the implementation bytecode — that's how we see mint()
    /// through delegatecall on USDC, EIP-3156 wrappers, etc.
    pub proxy_implementation: Option<String>,
}

/// Fetch all the on-chain state the CLI needs for a single EVM token,
/// using only standard JSON-RPC: 6 × `eth_call` + 1 × `eth_getCode`.
///
/// `http_url` should point at a self-hosted Reth/Geth/BSC node (or, for
/// throwaway CLI use, any public endpoint that supports those methods —
/// note that public endpoints may rate-limit `eth_getCode` on large
/// contracts).
///
/// `token_addr` accepts either `0x…` or bare-hex form and is normalised
/// to lowercase `0x…`.
pub async fn evm_token_metadata(
    http_url: &str,
    token_addr: &str,
) -> Result<EvmTokenMeta, AdapterError> {
    let addr = normalise_addr(token_addr)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    // Six eth_calls run sequentially over the same TCP connection;
    // public endpoints rate-limit per-IP but a self-hosted node serves
    // all six in <50ms total.
    let name_raw = eth_call(&client, http_url, &addr, &selectors::NAME).await?;
    let symbol_raw = eth_call(&client, http_url, &addr, &selectors::SYMBOL).await?;
    let decimals_raw = eth_call(&client, http_url, &addr, &selectors::DECIMALS).await?;
    let total_supply_raw_bytes = eth_call(&client, http_url, &addr, &selectors::TOTAL_SUPPLY).await?;
    let owner_raw = eth_call(&client, http_url, &addr, &selectors::OWNER).await?;
    let paused_raw = eth_call(&client, http_url, &addr, &selectors::PAUSED).await?;

    // Decode each. Reverts → `None` (the helper returns `Vec::new()` on
    // CallReverted).
    let name = decode_string(&name_raw);
    let symbol = decode_string(&symbol_raw);
    let decimals = decode_u8(&decimals_raw);
    let total_supply_raw = decode_u256_decimal_string(&total_supply_raw_bytes);
    let owner = decode_address(&owner_raw);
    let owner_renounced = matches!(owner.as_deref(), Some("0x0000000000000000000000000000000000000000"));
    let paused = decode_bool(&paused_raw);

    let proxy_bytecode = eth_get_code(&client, http_url, &addr).await?;

    // EIP-1967 implementation slot: keccak256("eip1967.proxy.implementation") - 1.
    // Non-zero value at this slot means `addr` is a transparent / UUPS proxy
    // and the actual logic lives at the returned address. Modern stablecoins
    // (USDC, BUSD, FRAX), most DAOs, and many memecoins use this pattern.
    let proxy_implementation = read_proxy_implementation(&client, http_url, &addr).await?;

    let mut combined_bytecode = proxy_bytecode.clone();
    if let Some(ref impl_addr) = proxy_implementation {
        // Concatenate both byte streams so a single grep over `combined`
        // catches selectors present in either layer.
        let impl_bytecode = eth_get_code(&client, http_url, impl_addr).await?;
        combined_bytecode.extend_from_slice(&impl_bytecode);
    }

    let has_mint_selector = bytecode_contains(&combined_bytecode, &bytecode_selectors::MINT_UINT256)
        || bytecode_contains(&combined_bytecode, &bytecode_selectors::MINT_ADDR_UINT256)
        || bytecode_contains(&combined_bytecode, &bytecode_selectors::ISSUE_UINT256);
    let has_pause_selector = bytecode_contains(&combined_bytecode, &bytecode_selectors::PAUSE);
    let has_blacklist_selector = bytecode_contains(&combined_bytecode, &bytecode_selectors::BLACKLIST_BOOL);
    let has_swap_toggle_selector = bytecode_contains(&combined_bytecode, &bytecode_selectors::SET_SWAP_ENABLED);
    // Use proxy bytecode size for the user-facing display (the proxy is
    // what they pasted in); the impl is invisible in EvmTokenMeta beyond
    // the boolean signal flags.
    let bytecode = proxy_bytecode;

    Ok(EvmTokenMeta {
        address: addr,
        name,
        symbol,
        decimals,
        total_supply_raw,
        owner,
        owner_renounced,
        paused,
        bytecode_len: bytecode.len(),
        has_mint_selector,
        has_pause_selector,
        has_blacklist_selector,
        has_swap_toggle_selector,
        proxy_implementation,
    })
}

/// Read the proxy-implementation slot for `addr`. Tries multiple
/// well-known proxy patterns in order until one returns a non-zero
/// implementation address:
///
/// 1. **EIP-1967** (`keccak256("eip1967.proxy.implementation") - 1`) —
///    OpenZeppelin TransparentUpgradeable, UUPS, modern contracts.
/// 2. **ZeppelinOS legacy** (`keccak256("org.zeppelinos.proxy.implementation")`) —
///    USDC FiatTokenProxy, older OZ, pre-2019 deployments.
/// 3. **EIP-1822 UUPS** (`keccak256("PROXIABLE")`) — early UUPS variants.
///
/// Returns `Ok(Some(addr))` on first hit, `Ok(None)` when every slot is
/// zero (regular non-proxy contract).
async fn read_proxy_implementation(
    client: &Client,
    http_url: &str,
    addr: &str,
) -> Result<Option<String>, AdapterError> {
    // Storage slots ordered by prevalence in production deployments.
    const PROXY_SLOTS: &[(&str, &str)] = &[
        (
            "EIP-1967",
            "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc",
        ),
        (
            "ZeppelinOS",
            "0x7050c9e0f4ca769c69bd3a8ef740bc37934f8e2c036e5a723fd8ee048ed3f8c3",
        ),
        (
            "EIP-1822",
            "0xc5f16f0fcc639fa48a6947836d9850f504798523bf8c9a3a87d5876cf622bcf7",
        ),
    ];

    for (_pattern, slot) in PROXY_SLOTS {
        if let Some(impl_addr) = read_storage_address(client, http_url, addr, slot).await? {
            return Ok(Some(impl_addr));
        }
    }
    Ok(None)
}

/// `eth_getStorageAt(addr, slot)` — return the lower-20 bytes as a
/// `0x…`-prefixed address string when non-zero, else `None`.
async fn read_storage_address(
    client: &Client,
    http_url: &str,
    addr: &str,
    slot: &str,
) -> Result<Option<String>, AdapterError> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getStorageAt",
        "params": [addr, slot, "latest"]
    });

    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getStorageAt HTTP error: {e}")))?;
    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getStorageAt parse error: {e}")))?;

    if let Some(err) = json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("eth_getStorageAt error: {err}"),
        });
    }

    let result = json
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AdapterError::Transport("eth_getStorageAt: missing result".to_owned()))?;
    let raw = decode_hex_prefixed(result)?;
    if raw.len() < 32 {
        return Ok(None);
    }
    let impl_bytes = &raw[12..32];
    if impl_bytes.iter().all(|b| *b == 0) {
        return Ok(None);
    }
    Ok(Some(format!("0x{}", hex::encode(impl_bytes))))
}

// ---------------------------------------------------------------------------
// Simulate-sell — D01 honeypot dynamic probe
// ---------------------------------------------------------------------------

/// `transfer(address,uint256)` selector — the canonical ERC-20 sell call.
const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

/// `balanceOf(address)` selector.
const BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];

/// Burn-address used as the recipient in simulate-sell — not coincidentally
/// the standard "send to burn" pattern. The probe transfers a tiny amount
/// (1 base unit) so any reasonable token cap-/ratio-check still passes.
const SIM_RECIPIENT: &str = "0x000000000000000000000000000000000000dEaD";

/// Revert-reason substrings (lowercased) that strongly suggest the token
/// blocks transfers under conditions other than balance. Hits here move
/// confidence sharply toward "honeypot".
///
/// These are drawn from a survey of real rug/honeypot contracts published
/// by HoneyBadger, Token Sniffer, and ScamBuster's labelled corpora — see
/// REFERENCES.md.
const HONEYPOT_REVERT_PHRASES: &[&str] = &[
    "trading not enabled",
    "trading is disabled",
    "trading not started",
    "trading paused",
    "transfer disabled",
    "cooldown",
    "bot detected",
    "anti-bot",
    "blacklist",
    "blocked",
    "max tx",
    "max_tx",
    "max sell",
    "max_sell",
    "not allowed",
    "not enabled",
    "not authorised",
    "not authorized",
    "frozen",
    "paused",
    "honeypot",
];

/// Revert-reason substrings that indicate the test failed for trivial
/// "sender has zero balance" reasons — the probe was uninformative, NOT a
/// honeypot signal.
const BALANCE_REVERT_PHRASES: &[&str] = &[
    "insufficient balance",
    "exceeds balance",
    "transfer amount exceeds balance",
    "balance too low",
    "subtraction overflow", // OpenZeppelin-style underflow on balance subtract
    "underflow",
    "ds-math-sub-underflow",
    "arithmetic operation",
];

/// Run a transfer simulation for `token_addr` from each candidate sender in
/// turn until one returns a verdict (success or a non-balance revert). The
/// function tries the owner first, then the token contract address itself
/// (some tokens hold balance for tax/fee mechanisms). Returns the first
/// informative outcome or `Skipped` if every candidate had zero balance.
///
/// `owner_addr` is the value we already fetched from `evm_token_metadata`
/// — passing it in saves a redundant `eth_call`.
///
/// `extra_senders` is an optional list of additional candidate sender
/// addresses to try when the owner / contract-self path doesn't have
/// non-zero balance. Typical caller passes the top-N net-flow receivers
/// from `fetch_recent_holder_flows` — they're known to hold tokens right
/// now, and probing through them catches honeypots whose `transfer()`
/// reverts for non-owner senders specifically (a common rug-prep pattern).
pub async fn simulate_sell_evm(
    http_url: &str,
    token_addr: &str,
    owner_addr: Option<&str>,
    extra_senders: &[String],
) -> Result<SimulateSellOutcome, AdapterError> {
    let token = normalise_addr(token_addr)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    // Build the candidate-sender list. Owner first (often holds treasury);
    // contract self second (some tokens accumulate balance internally);
    // extra_senders last (recent active holders — useful when owner is
    // renounced and contract holds nothing).
    let mut candidates: Vec<String> = Vec::new();
    if let Some(o) = owner_addr {
        let normalised = normalise_addr(o)?;
        if normalised != "0x0000000000000000000000000000000000000000" {
            candidates.push(normalised);
        }
    }
    candidates.push(token.clone());
    for s in extra_senders {
        let normalised = normalise_addr(s)?;
        if !candidates.contains(&normalised)
            && normalised != "0x0000000000000000000000000000000000000000"
        {
            candidates.push(normalised);
        }
    }

    let mut last_skip_reason = "no candidate sender produced a verdict".to_owned();

    for sender in candidates {
        // Step 1: confirm this sender has non-zero balance — otherwise the
        // transfer probe will only ever revert with "insufficient balance"
        // and never reach the honeypot logic.
        let balance = balance_of(&client, http_url, &token, &sender).await?;
        if balance.is_empty() || balance.iter().all(|b| *b == 0) {
            last_skip_reason = format!("candidate {sender} has zero balance");
            continue;
        }

        // Step 2: build calldata `transfer(SIM_RECIPIENT, 1)` — minimum
        // amount avoids triggering anti-whale caps that would NOT indicate
        // a honeypot.
        let calldata = encode_transfer(SIM_RECIPIENT, 1)?;

        // Step 3: simulate via eth_call with explicit `from` field.
        match eth_call_with_from(&client, http_url, &sender, &token, &calldata).await {
            Ok(_returned) => return Ok(SimulateSellOutcome::Success),
            Err(reason) => {
                let lower = reason.to_lowercase();
                if HONEYPOT_REVERT_PHRASES.iter().any(|p| lower.contains(p)) {
                    return Ok(SimulateSellOutcome::Reverted { reason });
                }
                if BALANCE_REVERT_PHRASES.iter().any(|p| lower.contains(p)) {
                    last_skip_reason =
                        format!("candidate {sender} reverted with balance-related reason: {reason}");
                    continue;
                }
                // Unknown revert reason — could be a non-trivial honeypot
                // pattern we haven't catalogued yet. Treat as positive
                // signal but with the raw revert reason captured so the
                // operator can review.
                return Ok(SimulateSellOutcome::Reverted { reason });
            }
        }
    }

    Ok(SimulateSellOutcome::Skipped {
        reason: last_skip_reason,
    })
}

/// Read `balanceOf(holder)` — returns the raw 32-byte ABI return.
async fn balance_of(
    client: &Client,
    http_url: &str,
    token_addr: &str,
    holder: &str,
) -> Result<Vec<u8>, AdapterError> {
    let mut calldata = Vec::with_capacity(4 + 32);
    calldata.extend_from_slice(&BALANCE_OF_SELECTOR);
    let holder_norm = normalise_addr(holder)?;
    let holder_bytes = hex::decode(holder_norm.trim_start_matches("0x"))
        .map_err(|e| AdapterError::Transport(format!("balanceOf addr decode error: {e}")))?;
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(&holder_bytes);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": [
            { "to": token_addr, "data": format!("0x{}", hex::encode(&calldata)) },
            "latest"
        ]
    });

    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("balanceOf HTTP error: {e}")))?;
    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("balanceOf parse error: {e}")))?;

    if json.get("error").is_some() {
        // Balance query reverted — treat as zero (caller will skip this candidate).
        return Ok(Vec::new());
    }
    let result = json
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AdapterError::Transport("balanceOf: missing result".to_owned()))?;
    decode_hex_prefixed(result)
}

/// Build calldata for `transfer(recipient, amount)`. `amount` is `u128` —
/// for the simulate-sell probe we only ever pass `1`, well within range.
fn encode_transfer(recipient: &str, amount: u128) -> Result<String, AdapterError> {
    let recipient_norm = normalise_addr(recipient)?;
    let recipient_bytes = hex::decode(recipient_norm.trim_start_matches("0x"))
        .map_err(|e| AdapterError::Transport(format!("transfer recipient decode: {e}")))?;
    let mut calldata = Vec::with_capacity(4 + 64);
    calldata.extend_from_slice(&TRANSFER_SELECTOR);
    // Recipient pads left with 12 zero bytes to fill the 32-byte ABI slot.
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(&recipient_bytes);
    // Amount as 32-byte big-endian.
    calldata.extend_from_slice(&[0u8; 16]);
    calldata.extend_from_slice(&amount.to_be_bytes());
    Ok(format!("0x{}", hex::encode(&calldata)))
}

/// `eth_call` with an explicit `from` field — the simulator runs the call
/// AS IF `from_addr` sent it. On success returns the ABI-encoded result.
/// On revert returns `Err(reason_string)` — caller decides whether the
/// revert is a honeypot signal.
async fn eth_call_with_from(
    client: &Client,
    http_url: &str,
    from_addr: &str,
    to_addr: &str,
    data_hex: &str,
) -> Result<Vec<u8>, String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": [
            { "from": from_addr, "to": to_addr, "data": data_hex },
            "latest"
        ]
    });

    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("transport: {e}"))?;
    let json: Value = response
        .json()
        .await
        .map_err(|e| format!("parse: {e}"))?;

    if let Some(err) = json.get("error") {
        let raw = err.to_string();
        // Try to extract the revert reason from `error.data` (Geth/Reth
        // format) or from the message itself.
        let reason = err
            .get("data")
            .and_then(|d| d.as_str())
            .and_then(decode_revert_reason)
            .or_else(|| {
                err.get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_owned())
            })
            .unwrap_or(raw);
        return Err(reason);
    }
    let result = json
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing result".to_owned())?;
    decode_hex_prefixed(result).map_err(|e| format!("decode: {e}"))
}

/// Decode a Solidity revert-reason from the standard `Error(string)`
/// envelope returned in `error.data`. Format:
/// `0x08c379a0` (Error(string) selector) + `0x...20` (offset 32) +
/// `0x...len` + UTF-8 bytes padded to 32-byte boundary. Returns the
/// extracted string when the envelope parses cleanly, else `None`.
fn decode_revert_reason(hex_data: &str) -> Option<String> {
    let raw = hex::decode(hex_data.trim_start_matches("0x")).ok()?;
    if raw.len() < 4 + 64 {
        return None;
    }
    if raw[..4] != [0x08, 0xc3, 0x79, 0xa0] {
        return None;
    }
    let len = u256_low_usize(&raw[36..68])?;
    if len == 0 || raw.len() < 68 + len {
        return None;
    }
    let bytes = &raw[68..68 + len];
    std::str::from_utf8(bytes).ok().map(|s| s.to_owned())
}

// ---------------------------------------------------------------------------
// D10 — token age via contract-creation block lookup
// ---------------------------------------------------------------------------

/// Result of a D10 token-age probe. `None` for `creation_block` indicates
/// the binary search failed (no `eth_getCode` history available, or the
/// contract was never deployed in the searched range).
#[derive(Debug, Clone)]
pub struct ContractAge {
    /// Lowest block number at which `eth_getCode(addr, block)` returned
    /// non-empty bytecode.
    pub creation_block: u64,
    /// UNIX timestamp of `creation_block`.
    pub creation_ts: i64,
    /// Age in seconds from `creation_ts` to wall-clock now.
    pub age_secs: i64,
    /// `true` when the RPC's archive state does not go back to block 1 —
    /// the binary search converged on the earliest block the RPC has state
    /// for, which is an UPPER BOUND on `creation_block` (real deployment
    /// may be earlier). Callers should treat the resulting age as a lower
    /// bound and avoid firing fresh-launch signals when this is set.
    pub archive_limited: bool,
}

/// Find the contract age for `addr` by binary-searching `eth_getCode`
/// against historical blocks until we narrow down the lowest block where
/// the contract has bytecode. Then read that block's timestamp.
///
/// Cost: O(log₂ N) RPC calls where N is current chain height. Mainnet at
/// ~22 M blocks → ~24 calls per probe. Public RPCs (publicnode.com,
/// 1rpc.io) serve archive-state `eth_getCode` against any historical
/// block at ~50ms latency, so total wall-clock is around 1.2 s per token
/// — acceptable for a one-shot CLI.
///
/// Returns `Ok(None)` when the binary search fails to converge (returned
/// empty code at every block — should never happen for a deployed
/// contract; usually a non-archive RPC).
pub async fn find_contract_age(
    http_url: &str,
    token_addr: &str,
) -> Result<Option<ContractAge>, AdapterError> {
    let addr = normalise_addr(token_addr)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    let latest = eth_block_number(&client, http_url).await?;
    if latest == 0 {
        return Ok(None);
    }

    // Detect whether the RPC keeps archive state back to block 1. We use
    // the strict (non-swallowing) probe because a real "no-code-here"
    // response and an archive-cutoff response are otherwise both reported
    // as empty by the lenient helper used in the binary search.
    let archive_limited = !is_archive_state_at_block_available(&client, http_url, &addr, 1).await;

    // Binary search: lo = first block where contract definitely doesn't
    // exist (per the RPC's view); hi = first block where it definitely
    // exists. Invariant: code(lo) empty, code(hi) non-empty. Narrow until
    // lo+1 == hi, then hi is the creation block (or, when `archive_limited`,
    // an upper bound on it).
    let mut lo: u64 = 0;
    let mut hi: u64 = latest;

    let latest_code = eth_get_code_at_block(&client, http_url, &addr, latest).await?;
    if latest_code.is_empty() {
        return Ok(None);
    }

    let early_code = eth_get_code_at_block(&client, http_url, &addr, 1).await?;
    if !early_code.is_empty() {
        let ts = eth_get_block_timestamp(&client, http_url, 1).await?;
        let now = chrono::Utc::now().timestamp();
        return Ok(Some(ContractAge {
            creation_block: 1,
            creation_ts: ts,
            age_secs: now - ts,
            archive_limited,
        }));
    }

    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        let code = eth_get_code_at_block(&client, http_url, &addr, mid).await?;
        if code.is_empty() {
            lo = mid;
        } else {
            hi = mid;
        }
    }

    let ts = eth_get_block_timestamp(&client, http_url, hi).await?;
    let now = chrono::Utc::now().timestamp();
    Ok(Some(ContractAge {
        creation_block: hi,
        creation_ts: ts,
        age_secs: now - ts,
        archive_limited,
    }))
}

/// Strict probe: returns true when the RPC has archive state at
/// `block_number` (i.e. our binary search would get a real answer there),
/// false when the RPC reports the historical state is pruned. Distinct
/// from the lenient `eth_get_code_at_block` which returns `Vec::new()` in
/// both "no code at this block" and "archive cutoff" cases.
async fn is_archive_state_at_block_available(
    client: &Client,
    http_url: &str,
    addr: &str,
    block_number: u64,
) -> bool {
    let block_hex = format!("0x{block_number:x}");
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getCode",
        "params": [addr, block_hex]
    });
    let response = match client.post(http_url).json(&body).send().await {
        Ok(r) => r,
        Err(_) => return false,
    };
    let json: Value = match response.json().await {
        Ok(j) => j,
        Err(_) => return false,
    };
    if let Some(err) = json.get("error") {
        let s = err.to_string().to_lowercase();
        if s.contains("historical state")
            || s.contains("missing trie node")
            || s.contains("does not exist")
        {
            return false;
        }
    }
    true
}

/// `eth_getTransactionCount(addr, "latest")` — returns the address nonce.
/// For EOAs this is the number of outgoing transactions ever sent from
/// the account; for contracts it's the number of contracts created via
/// CREATE/CREATE2 from that contract. High EOA nonce = "very active
/// account", typical of serial bot deployers (Banana Gun / Maestro
/// memecoin launcher EOAs accumulate thousands of outgoing tx).
pub async fn eth_get_transaction_count(
    http_url: &str,
    addr: &str,
) -> Result<u64, AdapterError> {
    let normalised = normalise_addr(addr)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getTransactionCount",
        "params": [normalised, "latest"]
    });
    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getTransactionCount HTTP: {e}")))?;
    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getTransactionCount parse: {e}")))?;
    if let Some(err) = json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("eth_getTransactionCount error: {err}"),
        });
    }
    let result = json
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AdapterError::Transport("eth_getTransactionCount: missing result".to_owned()))?;
    let stripped = result.strip_prefix("0x").unwrap_or(result);
    u64::from_str_radix(stripped, 16)
        .map_err(|e| AdapterError::Transport(format!("nonce hex parse: {e}")))
}

/// `eth_blockNumber` — return the latest block height as a u64.
async fn eth_block_number(client: &Client, http_url: &str) -> Result<u64, AdapterError> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_blockNumber",
        "params": []
    });
    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_blockNumber HTTP error: {e}")))?;
    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_blockNumber parse error: {e}")))?;
    if let Some(err) = json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("eth_blockNumber error: {err}"),
        });
    }
    let result = json
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AdapterError::Transport("eth_blockNumber: missing result".to_owned()))?;
    let stripped = result.strip_prefix("0x").unwrap_or(result);
    u64::from_str_radix(stripped, 16)
        .map_err(|e| AdapterError::Transport(format!("eth_blockNumber hex parse: {e}")))
}

/// `eth_getCode(addr, block_number)` — historical state read.
///
/// When the RPC reports "historical state ... is not available" (typical
/// of non-archive nodes or BSC publicnode at very early blocks), this is
/// treated as **equivalent to empty bytecode**. The binary search above
/// then converges on the earliest block where the RPC actually has state
/// — which is a safe upper bound on the deployment block (real
/// deployment may be earlier, but we just can't see it).
async fn eth_get_code_at_block(
    client: &Client,
    http_url: &str,
    addr: &str,
    block_number: u64,
) -> Result<Vec<u8>, AdapterError> {
    let block_hex = format!("0x{block_number:x}");
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getCode",
        "params": [addr, block_hex]
    });
    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getCode HTTP error: {e}")))?;
    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getCode parse error: {e}")))?;
    if let Some(err) = json.get("error") {
        let err_str = err.to_string().to_lowercase();
        if err_str.contains("historical state")
            || err_str.contains("missing trie node")
            || err_str.contains("does not exist")
        {
            // Non-archive RPC / pruning floor — treat as "no code at this
            // block" so the binary search converges on the earliest block
            // the RPC actually has state for.
            return Ok(Vec::new());
        }
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("eth_getCode @ {block_number} error: {err}"),
        });
    }
    let result = json
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AdapterError::Transport("eth_getCode: missing result".to_owned()))?;
    decode_hex_prefixed(result)
}

/// `eth_getBlockByNumber(block, false)` — return the block timestamp.
async fn eth_get_block_timestamp(
    client: &Client,
    http_url: &str,
    block_number: u64,
) -> Result<i64, AdapterError> {
    let block_hex = format!("0x{block_number:x}");
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getBlockByNumber",
        "params": [block_hex, false]
    });
    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getBlockByNumber HTTP error: {e}")))?;
    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getBlockByNumber parse error: {e}")))?;
    if let Some(err) = json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("eth_getBlockByNumber error: {err}"),
        });
    }
    let ts_hex = json
        .get("result")
        .and_then(|r| r.get("timestamp"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| {
            AdapterError::Transport("eth_getBlockByNumber: missing block.timestamp".to_owned())
        })?;
    let stripped = ts_hex.strip_prefix("0x").unwrap_or(ts_hex);
    i64::from_str_radix(stripped, 16)
        .map_err(|e| AdapterError::Transport(format!("block timestamp hex parse: {e}")))
}

// ---------------------------------------------------------------------------
// D04 — swap-volume probe via Uniswap V2/V3 pool log scan
// ---------------------------------------------------------------------------

/// Result of a D04 swap-volume probe.
#[derive(Debug, Clone)]
pub struct SwapVolumeProbe {
    /// Resolved DEX pool/pair address actually scanned.
    pub pool: String,
    /// Which DEX surface was scanned (`UniswapV2`, `UniswapV3-3000`, etc.).
    pub source: String,
    /// First block in the scan window.
    pub from_block: u64,
    /// Last block in the scan window.
    pub to_block: u64,
    /// Total swap events observed in the window.
    pub total_swaps: usize,
    /// Swap events in the most recent `recent_window` blocks (≈1 hour at
    /// 12s/block). The "last-hour" pump signal compares this to the
    /// trailing average over the rest of the window.
    pub recent_swaps: usize,
    /// Ratio (recent_per_block / trailing_per_block). Values ≫ 1 indicate
    /// a pump signal; very small values indicate a cooled-off token.
    /// `None` when the trailing window has zero events (cannot compute).
    pub spike_ratio: Option<f64>,
}

/// Mainnet WETH9 address. Hard-coded for now — when we add Base / Arbitrum
/// / Optimism / Polygon multi-EVM in a future sprint, make this a per-chain
/// constant.
const MAINNET_WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
/// BSC WBNB.
const BSC_WBNB: &str = "0xbb4cdb9cbd36b01bd1cbaebf2de08d9173bc095c";

/// Uniswap V2 Factory (Ethereum mainnet).
const UNISWAP_V2_FACTORY: &str = "0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f";
/// Uniswap V3 Factory (Ethereum mainnet).
const UNISWAP_V3_FACTORY: &str = "0x1F98431c8aD98523631AE4a59f267346ea31F984";
/// PancakeSwap V2 Factory (BSC).
const PANCAKE_V2_FACTORY: &str = "0xcA143Ce32Fe78f1f7019d7d551a6402fC5350c73";

/// `getPair(address,address)` selector — Uniswap V2 / PancakeSwap V2.
const GET_PAIR_SELECTOR: [u8; 4] = [0xe6, 0xa4, 0x39, 0x05];
/// `getPool(address,address,uint24)` selector — Uniswap V3.
const GET_POOL_SELECTOR: [u8; 4] = [0x16, 0x98, 0xee, 0x82];

/// Uniswap V2 `Swap` event topic0.
const V2_SWAP_TOPIC: &str = "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822";
/// Uniswap V3 `Swap` event topic0.
const V3_SWAP_TOPIC: &str = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67";

/// Scan window for D04: blocks behind `latest`. 7200 blocks ≈ 24 h on
/// 12 s-block chains (Ethereum), shorter on faster chains. We prefer
/// "events per block" rather than wall-clock time so the math doesn't
/// need block-timestamp lookups for every window block.
const D04_SCAN_WINDOW_BLOCKS: u64 = 7_200;
/// "Recent" sub-window — 300 blocks ≈ 1 h on Ethereum. Comparing
/// `recent / window-recent` gives the spike ratio.
const D04_RECENT_WINDOW_BLOCKS: u64 = 300;

/// Base mainnet WETH (canonical L2 WETH at the standard predeploy slot).
const BASE_WETH: &str = "0x4200000000000000000000000000000000000006";
/// Uniswap V3 Factory on Base mainnet (different from L1 deployment).
const UNISWAP_V3_FACTORY_BASE: &str = "0x33128a8fc17869897dce68ed026d694621f6fdfd";
/// Aerodrome V2 Factory on Base — Base's main DEX, Velodrome fork.
const AERODROME_V2_FACTORY: &str = "0x420dd381b31aef6683db6b902084cb0ffece40da";
/// Arbitrum One WETH.
const ARBITRUM_WETH: &str = "0x82af49447d8a07e3bd95bd0d56f35241523fbab1";
/// Camelot V2 Factory on Arbitrum — Arbitrum's primary DEX.
const CAMELOT_V2_FACTORY: &str = "0x6eccab422d763ac031210895c81787e87b43a652";

/// Run the D04 swap-volume probe for `token_addr` on the given chain.
/// `chain_id` follows EIP-155: 1 = Ethereum mainnet, 56 = BSC,
/// 8453 = Base. Picks the right factory + WETH-equivalent per chain.
/// Returns `Ok(None)` when no DEX pool can be resolved.
pub async fn probe_swap_volume(
    http_url: &str,
    token_addr: &str,
    chain_id: u64,
) -> Result<Option<SwapVolumeProbe>, AdapterError> {
    let token = normalise_addr(token_addr)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    // Pick the chain-correct WETH-equivalent + factory tuple.
    let (factory_v2, factory_v3, base_token, v2_label, v3_label) = match chain_id {
        56 => (
            PANCAKE_V2_FACTORY,
            UNISWAP_V3_FACTORY,
            BSC_WBNB,
            "PancakeSwap V2",
            "UniswapV3 (BSC)",
        ),
        8453 => (
            AERODROME_V2_FACTORY,
            UNISWAP_V3_FACTORY_BASE,
            BASE_WETH,
            "Aerodrome V2",
            "UniswapV3 (Base)",
        ),
        42161 => (
            CAMELOT_V2_FACTORY,
            UNISWAP_V3_FACTORY,
            ARBITRUM_WETH,
            "Camelot V2",
            "UniswapV3 (Arbitrum)",
        ),
        10 => (
            // Optimism: no canonical V2; we still attempt V2 (most calls
            // return zero pair) and use V3 as the primary. WETH at the
            // standard L2 predeploy slot.
            UNISWAP_V2_FACTORY, // returns 0x0 on Optimism — harmless, V3 fallback fires
            UNISWAP_V3_FACTORY,
            "0x4200000000000000000000000000000000000006",
            "(no V2 on OP)",
            "UniswapV3 (Optimism)",
        ),
        137 => (
            // Polygon: QuickSwap V2 fork at the same address pattern,
            // canonical V3 deployment.
            "0x5757371414417b8c6caad45baef941abc7d3ab32", // QuickSwap V2 factory
            UNISWAP_V3_FACTORY,
            "0x0d500b1d8e8ef31e21c99d1db9a6444d3adf1270", // WMATIC
            "QuickSwap V2",
            "UniswapV3 (Polygon)",
        ),
        43114 => (
            // Avalanche: Trader Joe V1 (V2-shape factory) + canonical V3.
            "0x9ad6c38be94206ca50bb0d90783181662f0cfa10", // Trader Joe V1 factory
            UNISWAP_V3_FACTORY,
            "0xb31f66aa3c1e785363f0875a1b74e27b85fd66c7", // WAVAX
            "Trader Joe V1",
            "UniswapV3 (Avalanche)",
        ),
        _ => (
            UNISWAP_V2_FACTORY,
            UNISWAP_V3_FACTORY,
            MAINNET_WETH,
            "UniswapV2",
            "UniswapV3-3000",
        ),
    };

    // Step 1: try V2 factory.getPair(token, base).
    if let Some(pair) =
        get_pair_v2(&client, http_url, factory_v2, &token, base_token).await?
    {
        return scan_pool_logs(&client, http_url, &pair, v2_label, V2_SWAP_TOPIC).await;
    }

    // Step 2: fall back to V3 with 0.30% fee tier (3000).
    if let Some(pool) = get_pool_v3(&client, http_url, factory_v3, &token, base_token, 3000).await?
    {
        return scan_pool_logs(&client, http_url, &pool, v3_label, V3_SWAP_TOPIC).await;
    }

    // Step 3: try V3 with 1% fee tier (10000) — common for low-cap memecoins.
    if let Some(pool) =
        get_pool_v3(&client, http_url, factory_v3, &token, base_token, 10000).await?
    {
        return scan_pool_logs(&client, http_url, &pool, "UniswapV3-10000", V3_SWAP_TOPIC).await;
    }

    Ok(None)
}

async fn get_pair_v2(
    client: &Client,
    http_url: &str,
    factory: &str,
    token_a: &str,
    token_b: &str,
) -> Result<Option<String>, AdapterError> {
    // Calldata: selector + 32B token_a + 32B token_b.
    let mut calldata = Vec::with_capacity(4 + 64);
    calldata.extend_from_slice(&GET_PAIR_SELECTOR);
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(&hex::decode(token_a.trim_start_matches("0x")).map_err(
        |e| AdapterError::Transport(format!("getPair token_a decode: {e}")),
    )?);
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(&hex::decode(token_b.trim_start_matches("0x")).map_err(
        |e| AdapterError::Transport(format!("getPair token_b decode: {e}")),
    )?);
    let response = raw_eth_call(client, http_url, factory, &calldata).await?;
    if response.len() < 32 {
        // Factory returned empty bytes — happens when the pair doesn't
        // exist OR when the factory address itself is not a valid V2
        // factory on this chain. Treat as "no pair" rather than failing
        // so D04 can fall through to the V3 path.
        return Ok(None);
    }
    let addr = decode_address_result(&response)?;
    if addr == "0x0000000000000000000000000000000000000000" {
        return Ok(None);
    }
    Ok(Some(addr))
}

async fn get_pool_v3(
    client: &Client,
    http_url: &str,
    factory: &str,
    token_a: &str,
    token_b: &str,
    fee: u32,
) -> Result<Option<String>, AdapterError> {
    let mut calldata = Vec::with_capacity(4 + 96);
    calldata.extend_from_slice(&GET_POOL_SELECTOR);
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(&hex::decode(token_a.trim_start_matches("0x")).map_err(
        |e| AdapterError::Transport(format!("getPool token_a decode: {e}")),
    )?);
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(&hex::decode(token_b.trim_start_matches("0x")).map_err(
        |e| AdapterError::Transport(format!("getPool token_b decode: {e}")),
    )?);
    let mut fee_word = [0u8; 32];
    fee_word[28..].copy_from_slice(&fee.to_be_bytes());
    calldata.extend_from_slice(&fee_word);
    let response = raw_eth_call(client, http_url, factory, &calldata).await?;
    if response.len() < 32 {
        return Ok(None);
    }
    let addr = decode_address_result(&response)?;
    if addr == "0x0000000000000000000000000000000000000000" {
        return Ok(None);
    }
    Ok(Some(addr))
}

async fn raw_eth_call(
    client: &Client,
    http_url: &str,
    to: &str,
    calldata: &[u8],
) -> Result<Vec<u8>, AdapterError> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": [
            { "to": to, "data": format!("0x{}", hex::encode(calldata)) },
            "latest"
        ]
    });
    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("raw_eth_call HTTP error: {e}")))?;
    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("raw_eth_call parse error: {e}")))?;
    if let Some(err) = json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("raw_eth_call error: {err}"),
        });
    }
    let result = json
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AdapterError::Transport("raw_eth_call: missing result".to_owned()))?;
    decode_hex_prefixed(result)
}

/// Like the lower `decode_address` (which returns `Option<String>` for
/// graceful no-data handling), but returns `Err` on short input — used by
/// factory-call responses where missing data IS an error condition.
fn decode_address_result(raw: &[u8]) -> Result<String, AdapterError> {
    if raw.len() < 32 {
        return Err(AdapterError::Transport(format!(
            "decode_address_result: short input ({} bytes)",
            raw.len()
        )));
    }
    Ok(format!("0x{}", hex::encode(&raw[12..32])))
}

async fn scan_pool_logs(
    client: &Client,
    http_url: &str,
    pool: &str,
    source_label: &str,
    swap_topic: &str,
) -> Result<Option<SwapVolumeProbe>, AdapterError> {
    let latest = eth_block_number(client, http_url).await?;
    let from_block = latest.saturating_sub(D04_SCAN_WINDOW_BLOCKS);
    let recent_threshold = latest.saturating_sub(D04_RECENT_WINDOW_BLOCKS);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getLogs",
        "params": [{
            "address": pool,
            "fromBlock": format!("0x{from_block:x}"),
            "toBlock": format!("0x{latest:x}"),
            "topics": [swap_topic],
        }]
    });

    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getLogs HTTP error: {e}")))?;
    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getLogs parse error: {e}")))?;
    if let Some(err) = json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("eth_getLogs error: {err}"),
        });
    }
    let logs = json
        .get("result")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AdapterError::Transport("eth_getLogs: missing result array".to_owned()))?;

    let total_swaps = logs.len();
    let mut recent_swaps = 0usize;
    for entry in logs {
        let block_num_hex = entry.get("blockNumber").and_then(|v| v.as_str()).unwrap_or("0x0");
        let block_n = u64::from_str_radix(block_num_hex.trim_start_matches("0x"), 16)
            .unwrap_or(0);
        if block_n >= recent_threshold {
            recent_swaps += 1;
        }
    }

    let trailing_swaps = total_swaps.saturating_sub(recent_swaps);
    let trailing_blocks =
        (latest - from_block).saturating_sub(D04_RECENT_WINDOW_BLOCKS).max(1);
    let recent_per_block = recent_swaps as f64 / D04_RECENT_WINDOW_BLOCKS as f64;
    let trailing_per_block = trailing_swaps as f64 / trailing_blocks as f64;
    let spike_ratio = if trailing_per_block > 0.0 {
        Some(recent_per_block / trailing_per_block)
    } else if recent_per_block > 0.0 {
        Some(f64::INFINITY)
    } else {
        None
    };

    Ok(Some(SwapVolumeProbe {
        pool: pool.to_owned(),
        source: source_label.to_owned(),
        from_block,
        to_block: latest,
        total_swaps,
        recent_swaps,
        spike_ratio,
    }))
}

// ---------------------------------------------------------------------------
// D02-aux — recent ownership-transfer event detection
// ---------------------------------------------------------------------------

/// `OwnershipTransferred(address indexed previousOwner, address indexed newOwner)`
/// — OpenZeppelin Ownable's standard event. Topic0 = keccak256(signature).
const OWNERSHIP_TRANSFERRED_TOPIC: &str =
    "0x8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e0";

/// Result of an ownership-event probe. `recently_renounced` is `true` when
/// the most recent `OwnershipTransferred` event in the scan window had
/// `newOwner == 0x0`. Combined with current `owner() == 0x0` from the
/// metadata fetch, this surfaces the **post-rug renounce pattern** —
/// scammers frequently call `renounceOwnership()` immediately after the
/// liquidity pull to make the contract look "trustless" when in fact the
/// damage is already done.
#[derive(Debug, Clone)]
pub struct OwnershipEventProbe {
    /// `(from_block, to_block)` actually scanned.
    pub window: (u64, u64),
    /// True when an `OwnershipTransferred` event was observed in the window.
    pub had_event: bool,
    /// True when the LAST event in the window had `newOwner = 0x0`.
    pub recently_renounced: bool,
    /// Block number of the last observed event (when `had_event`).
    pub last_event_block: Option<u64>,
}

/// Scan the last 50,000 blocks (~7 d on Ethereum) for
/// `OwnershipTransferred` events emitted by `token_addr`. The Ownable
/// pattern emits this event on every `transferOwnership` and
/// `renounceOwnership` call. Detecting a recent `→ 0x0` transfer
/// alongside `owner() == 0x0` is the calibrated signal for "ownership
/// was renounced *recently*", which strongly differentiates a
/// post-rug abandoned contract from a token that was launched
/// trustless from day one.
pub async fn probe_ownership_events(
    http_url: &str,
    token_addr: &str,
) -> Result<OwnershipEventProbe, AdapterError> {
    let token = normalise_addr(token_addr)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    let latest = eth_block_number(&client, http_url).await?;
    // 50,000 blocks ≈ 7 days on Ethereum, ~1.5 days on BSC.
    let from_block = latest.saturating_sub(50_000);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getLogs",
        "params": [{
            "address": token,
            "fromBlock": format!("0x{from_block:x}"),
            "toBlock": format!("0x{latest:x}"),
            "topics": [OWNERSHIP_TRANSFERRED_TOPIC],
        }]
    });

    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getLogs HTTP error: {e}")))?;
    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getLogs parse error: {e}")))?;
    if let Some(err) = json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("eth_getLogs OwnershipTransferred error: {err}"),
        });
    }
    let logs = json
        .get("result")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AdapterError::Transport("eth_getLogs: missing result array".to_owned()))?;

    let had_event = !logs.is_empty();
    let mut last_event_block: Option<u64> = None;
    let mut recently_renounced = false;

    if let Some(last) = logs.last() {
        // topics[2] is the new owner address (right-aligned in 32-byte slot).
        let topics = last.get("topics").and_then(|t| t.as_array());
        if let Some(t) = topics
            && t.len() >= 3
            && let Some(new_owner) = topic_to_address(&t[2])
        {
            if new_owner == "0x0000000000000000000000000000000000000000" {
                recently_renounced = true;
            }
        }
        let block_hex = last.get("blockNumber").and_then(|v| v.as_str()).unwrap_or("0x0");
        last_event_block = u64::from_str_radix(block_hex.trim_start_matches("0x"), 16).ok();
    }

    Ok(OwnershipEventProbe {
        window: (from_block, latest),
        had_event,
        recently_renounced,
        last_event_block,
    })
}

// ---------------------------------------------------------------------------
// Token discovery — recent PairCreated events on Uniswap V2 / PancakeSwap V2
// ---------------------------------------------------------------------------

/// One newly-listed token discovered via `PairCreated` event scanning.
#[derive(Debug, Clone)]
pub struct DiscoveredToken {
    /// The non-stablecoin token address (the "interesting" one).
    pub token: String,
    /// The DEX pair contract address.
    pub pair: String,
    /// Block number where the pair was created.
    pub block_number: u64,
    /// Approximate deployment timestamp (UNIX seconds) — derived from the
    /// block number when available, else `None`.
    pub block_ts: Option<i64>,
    /// `"WETH"` or `"WBNB"` depending on chain — the "base" side of the pair.
    pub paired_with: &'static str,
}

/// `PairCreated(address indexed token0, address indexed token1, address pair, uint256)`
/// — Uniswap V2 / SushiSwap / PancakeSwap V2 standard event.
const PAIR_CREATED_TOPIC: &str =
    "0x0d3648bd0f6ba80134a33ba9275ac585d9d315f0ad8355cddefde31afa28d0e9";

/// `PoolCreated(address indexed token0, address indexed token1,
///              uint24 indexed fee, int24 tickSpacing, address pool)`
/// — Uniswap V3 standard event. Used for Base discovery (Base ecosystem
/// is V3-first; Aerodrome V2 PoolCreated has a different topic).
const V3_POOL_CREATED_TOPIC: &str =
    "0x783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118";

/// Scan the last `lookback_blocks` blocks of the V2 factory for newly-
/// created pairs whose other side is the chain's canonical wrapped native
/// (WETH on Ethereum, WBNB on BSC). Returns the list sorted by deployment
/// block descending — newest first. The caller can take(N) for "last N
/// memecoins listed".
///
/// `is_bsc = true` switches to PancakeSwap V2 + WBNB.
pub async fn discover_recent_pairs(
    http_url: &str,
    is_bsc: bool,
    lookback_blocks: u64,
) -> Result<Vec<DiscoveredToken>, AdapterError> {
    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    let (factory, base_token, base_label) = if is_bsc {
        (PANCAKE_V2_FACTORY, BSC_WBNB, "WBNB")
    } else {
        (UNISWAP_V2_FACTORY, MAINNET_WETH, "WETH")
    };

    let latest = eth_block_number(&client, http_url).await?;
    let from_block = latest.saturating_sub(lookback_blocks);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getLogs",
        "params": [{
            "address": factory,
            "fromBlock": format!("0x{from_block:x}"),
            "toBlock": format!("0x{latest:x}"),
            "topics": [PAIR_CREATED_TOPIC],
        }]
    });

    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getLogs HTTP error: {e}")))?;
    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getLogs parse error: {e}")))?;
    if let Some(err) = json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("eth_getLogs PairCreated error: {err}"),
        });
    }
    let logs = json
        .get("result")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AdapterError::Transport("eth_getLogs: missing result array".to_owned()))?;

    let base_lower = base_token.to_ascii_lowercase();
    let mut out: Vec<DiscoveredToken> = Vec::new();

    for log in logs {
        let topics = match log.get("topics").and_then(|t| t.as_array()) {
            Some(t) if t.len() >= 3 => t,
            _ => continue,
        };
        let token0 = match topic_to_address_str(&topics[1]) {
            Some(a) => a,
            None => continue,
        };
        let token1 = match topic_to_address_str(&topics[2]) {
            Some(a) => a,
            None => continue,
        };
        // Keep only pairs that include the chain's canonical wrapped-native
        // — the typical "new memecoin listed against WETH/WBNB" pattern.
        let token = if token0 == base_lower {
            token1
        } else if token1 == base_lower {
            token0
        } else {
            continue;
        };
        // `pair` address is in the log data — first 32 bytes, right-aligned.
        let data = log.get("data").and_then(|d| d.as_str()).unwrap_or("0x");
        let raw = match decode_hex_prefixed(data) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if raw.len() < 32 {
            continue;
        }
        let pair = format!("0x{}", hex::encode(&raw[12..32]));
        let block_hex = log.get("blockNumber").and_then(|v| v.as_str()).unwrap_or("0x0");
        let block_number = u64::from_str_radix(block_hex.trim_start_matches("0x"), 16)
            .unwrap_or(0);
        out.push(DiscoveredToken {
            token,
            pair,
            block_number,
            block_ts: None,
            paired_with: base_label,
        });
    }

    out.sort_by(|a, b| b.block_number.cmp(&a.block_number));
    Ok(out)
}

/// Discover newly-created Uniswap V3 pools whose other side is the
/// chain's WETH-equivalent. Used for Base (Aerodrome V2 has its own
/// event topic; the V3 path covers the bulk of new memecoin pools on
/// Base). Returns descending by deployment block.
pub async fn discover_recent_v3_pools(
    http_url: &str,
    factory: &str,
    base_token_addr: &str,
    base_label: &'static str,
    lookback_blocks: u64,
) -> Result<Vec<DiscoveredToken>, AdapterError> {
    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    let latest = eth_block_number(&client, http_url).await?;
    let from_block = latest.saturating_sub(lookback_blocks);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getLogs",
        "params": [{
            "address": factory,
            "fromBlock": format!("0x{from_block:x}"),
            "toBlock": format!("0x{latest:x}"),
            "topics": [V3_POOL_CREATED_TOPIC],
        }]
    });

    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getLogs HTTP error: {e}")))?;
    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getLogs parse error: {e}")))?;
    if let Some(err) = json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("eth_getLogs PoolCreated error: {err}"),
        });
    }
    let logs = json
        .get("result")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AdapterError::Transport("eth_getLogs: missing result array".to_owned()))?;

    let base_lower = base_token_addr.to_ascii_lowercase();
    let mut out: Vec<DiscoveredToken> = Vec::new();

    for log in logs {
        let topics = match log.get("topics").and_then(|t| t.as_array()) {
            Some(t) if t.len() >= 4 => t,
            _ => continue,
        };
        let token0 = match topic_to_address_str(&topics[1]) {
            Some(a) => a,
            None => continue,
        };
        let token1 = match topic_to_address_str(&topics[2]) {
            Some(a) => a,
            None => continue,
        };
        let token = if token0 == base_lower {
            token1
        } else if token1 == base_lower {
            token0
        } else {
            continue;
        };
        // Pool address is in data, second 32-byte word, last 20 bytes.
        let data = log.get("data").and_then(|d| d.as_str()).unwrap_or("0x");
        let raw = match decode_hex_prefixed(data) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if raw.len() < 64 {
            continue;
        }
        let pair = format!("0x{}", hex::encode(&raw[44..64]));
        let block_hex = log.get("blockNumber").and_then(|v| v.as_str()).unwrap_or("0x0");
        let block_number = u64::from_str_radix(block_hex.trim_start_matches("0x"), 16)
            .unwrap_or(0);
        out.push(DiscoveredToken {
            token,
            pair,
            block_number,
            block_ts: None,
            paired_with: base_label,
        });
    }

    out.sort_by(|a, b| b.block_number.cmp(&a.block_number));
    Ok(out)
}

/// `topics[i]` is a `0x…` 32-byte hex string with an EVM address right-
/// aligned (the lower 20 bytes). Returns the lowercased `0x…40hex` form
/// or `None` on malformed input.
fn topic_to_address_str(topic: &Value) -> Option<String> {
    let s = topic.as_str()?;
    let raw = s.strip_prefix("0x").unwrap_or(s);
    if raw.len() != 64 {
        return None;
    }
    Some(format!("0x{}", &raw[24..].to_ascii_lowercase()))
}

// ---------------------------------------------------------------------------
// D03 helper — entity classification (DEX-pool detection)
// ---------------------------------------------------------------------------

/// Classification of an address relative to the rug/whale-concentration math.
/// Used by D03 to suppress addresses that are structurally guaranteed to
/// concentrate flow (DEX pool contracts, routers, aggregators, MEV bots,
/// CEX hot wallets) so the gini / top-N math reflects real holder
/// behaviour, not market-structure noise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressClass {
    /// EOA wallet — counts toward concentration math.
    Unknown,
    /// Uniswap V2 / PancakeSwap V2 / SushiSwap V2 pair contract.
    UniswapV2Pair,
    /// Uniswap V3 pool.
    UniswapV3Pool,
    /// Generic deployed contract not matching the specific DEX-pair patterns
    /// — typically a router (Uniswap UniversalRouter, 1inch, Cowswap),
    /// aggregator settlement, MEV bot, or bridge. For D03 purposes these
    /// are all "infrastructure": they dominate net flow because every
    /// swap routes through them, but they aren't real holders.
    ContractInfrastructure,
    /// Hardcoded known address (CEX hot wallet, bridge router, treasury).
    Known(&'static str),
}

/// Selector for `factory()` returns address — Uniswap V2 / V3 pool standard.
const FACTORY_SELECTOR: [u8; 4] = [0xc4, 0x5a, 0x01, 0x55];
/// Selector for `slot0()` returns (uint160, int24, ...) — Uniswap V3 pool only.
const SLOT0_SELECTOR: [u8; 4] = [0x38, 0x50, 0xc7, 0xbd];
/// Selector for `token0()` — present on every UniV2 pair.
const TOKEN0_SELECTOR: [u8; 4] = [0x0d, 0xfe, 0x16, 0x81];

/// Hardcoded labels for well-known infrastructure addresses (CEX hot
/// wallets, Wormhole / bridge routers, common treasury controllers).
/// Lowercased keys for normalised lookup.
fn known_address_label(addr_lower: &str) -> Option<&'static str> {
    match addr_lower {
        // Binance hot wallets (mainnet)
        "0x28c6c06298d514db089934071355e5743bf21d60" => Some("binance_14"),
        "0x21a31ee1afc51d94c2efccaa2092ad1028285549" => Some("binance_15"),
        "0xdfd5293d8e347dfe59e90efd55b2956a1343963d" => Some("binance_16"),
        "0x56eddb7aa87536c09ccc2793473599fd21a8b17f" => Some("binance_17"),
        "0x9696f59e4d72e237be84ffd425dcad154bf96976" => Some("binance_18"),
        "0x4d9ff50ef4da947364bb9650892b2554e7be5e2b" => Some("binance_peg"),
        "0x4976a4a02f38326660d17bf34b431dc6e2eb2327" => Some("binance_8"),
        // Coinbase
        "0x71660c4005ba85c37ccec55d0c4493e66fe775d3" => Some("coinbase_1"),
        "0x503828976d22510aad0201ac7ec88293211d23da" => Some("coinbase_2"),
        "0xddfabcdc4d8ffc6d5beaf154f18b778f892a0740" => Some("coinbase_3"),
        "0x3cd751e6b0078be393132286c442345e5dc49699" => Some("coinbase_4"),
        "0xb5d85cbf7cb3ee0d56b3bb207d5fc4b82f43f511" => Some("coinbase_5"),
        // Kraken
        "0x267be1c1d684f78cb4f6a176c4911b741e4ffdc0" => Some("kraken_1"),
        "0xe853c56864a2ebe4576a807d26fdc4a0ada51919" => Some("kraken_2"),
        "0xa83b11093c858c86321fbc4c20fe82cdbd58e09e" => Some("kraken_3"),
        "0xae2d4617c862309a3d75a0ffb358c7a5009c673f" => Some("kraken_4"),
        // Bitfinex
        "0x1151314c646ce4e0efd76d1af4760ae66a9fe30f" => Some("bitfinex_5"),
        "0x876eabf441b2ee5b5b0554fd502a8e0600950cfa" => Some("bitfinex_6"),
        // OKX
        "0x6cc5f688a315f3dc28a7781717a9a798a59fda7b" => Some("okx_1"),
        "0x868dab0b8e21ec0a48b76a7dc8538270d5dca4a4" => Some("okx_2"),
        "0x236f9f97e0e62388479bf9e5ba4889e46b0273c3" => Some("okx_3"),
        // Crypto.com
        "0x6262998ced04146fa42253a5c0af90ca02dfd2a3" => Some("crypto_com_1"),
        "0x46340b20830761efd32832a74d7169b29feb9758" => Some("crypto_com_2"),
        // Bitget
        "0x0639556f03714a74a5feeaf5736a4a64ff70d206" => Some("bitget_1"),
        "0x5051c30d7f1d684a87bdde5fe7eaa4ce5ea91c34" => Some("bitget_2"),
        // KuCoin
        "0x2b5634c42055806a59e9107ed44d43c426e58258" => Some("kucoin_1"),
        "0x689c56aef474df92d44a1b70850f808488f9769c" => Some("kucoin_2"),
        // Gate.io
        "0x0d0707963952f2fba59dd06f2b425ace40b492fe" => Some("gate_io_1"),
        "0x1c4b70a3968436b9a0a9cf5205c787eb81bb558c" => Some("gate_io_2"),
        // HTX (formerly Huobi)
        "0xeee28d484628d41a82d01e21d12e2e78d69920da" => Some("htx_1"),
        "0xab5c66752a9e8167967685f1450532fb96d5d24f" => Some("htx_2"),
        // Bybit
        "0xa7a93fd0a276fc1c0197a5b5623ed117786eed06" => Some("bybit_1"),
        "0xf89d7b9c864f589bbf53a82105107622b35eaa40" => Some("bybit_2"),
        // MEXC
        "0x9642b23ed1e01df1092b92641051881a322f5d4e" => Some("mexc_1"),
        // Wormhole bridge
        "0x3ee18b2214aff97000d974cf647e7c347e8fa585" => Some("wormhole_bridge"),
        // LayerZero / Stargate
        "0x8731d54e9d02c286767d56ac03e8037c07e01e98" => Some("stargate_router"),
        // null / dead
        "0x000000000000000000000000000000000000dead" => Some("burn_dead"),
        _ => None,
    }
}

/// Classify `addr` for D03 suppression purposes. Performs at most two
/// `eth_call`s — `factory()` (covers V2 pairs and V3 pools) and one
/// fallback `token0()` (covers V2 pairs whose factory() reverts because
/// they were deployed before the standard interface; rare).
pub async fn classify_address(http_url: &str, addr: &str) -> AddressClass {
    let lower = addr.to_ascii_lowercase();
    if let Some(label) = known_address_label(&lower) {
        return AddressClass::Known(label);
    }

    let client = match Client::builder().timeout(Duration::from_secs(15)).build() {
        Ok(c) => c,
        Err(_) => return AddressClass::Unknown,
    };

    // Try `factory()` — universal across Uniswap V2 / V3 / Pancake / Sushi.
    if let Ok(returned) = eth_call(&client, http_url, &lower, &FACTORY_SELECTOR).await {
        if returned.len() == 32 {
            let factory_lower = format!("0x{}", hex::encode(&returned[12..32]));
            if is_known_v3_factory(&factory_lower) {
                return AddressClass::UniswapV3Pool;
            }
            if is_known_v2_factory(&factory_lower) {
                return AddressClass::UniswapV2Pair;
            }
        }
    }

    // Try `slot0()` — distinct V3-only signature. If it succeeds, V3 pool.
    if let Ok(returned) = eth_call(&client, http_url, &lower, &SLOT0_SELECTOR).await {
        if !returned.is_empty() {
            return AddressClass::UniswapV3Pool;
        }
    }

    // Try `token0()` — V2 pair fallback when factory() didn't help.
    if let Ok(returned) = eth_call(&client, http_url, &lower, &TOKEN0_SELECTOR).await {
        if returned.len() == 32 {
            return AddressClass::UniswapV2Pair;
        }
    }

    // Last check: is the address a contract at all? Any deployed bytecode
    // means it's NOT an EOA wallet — for D03 purposes that's "infrastructure"
    // (router / aggregator / settlement / MEV bot / treasury). Real holder
    // concentration is over EOAs.
    if let Ok(code) = eth_get_code(&client, http_url, &lower).await {
        if !code.is_empty() {
            return AddressClass::ContractInfrastructure;
        }
    }

    AddressClass::Unknown
}

fn is_known_v2_factory(addr_lower: &str) -> bool {
    matches!(
        addr_lower,
        // Uniswap V2 (Ethereum)
        "0x5c69bee701ef814a2b6a3edd4b1652cb9cc5aa6f"
        // SushiSwap (Ethereum)
        | "0xc0aee478e3658e2610c5f7a4a2e1777ce9e4f2ac"
        // PancakeSwap V2 (BSC)
        | "0xca143ce32fe78f1f7019d7d551a6402fc5350c73"
        // Aerodrome (Base)
        | "0x420dd381b31aef6683db6b902084cb0ffece40da"
    )
}

fn is_known_v3_factory(addr_lower: &str) -> bool {
    matches!(
        addr_lower,
        // Uniswap V3 (deployed on mainnet, Polygon, Arbitrum, Optimism, BSC)
        "0x1f98431c8ad98523631ae4a59f267346ea31f984"
        // PancakeSwap V3
        | "0x0bfbcf9fa4f9c56b0f40a671ad40e0805a091865"
    )
}

// ---------------------------------------------------------------------------
// D03 — recent-window holder concentration via Transfer log replay
// ---------------------------------------------------------------------------

/// One Transfer event captured from the log scan, kept around for
/// downstream graph-based detectors (D05 wash trading, D11 burst). The
/// fields are denormalised so the consumer can iterate without re-parsing.
#[derive(Debug, Clone)]
pub struct TransferEdge {
    pub from: String,
    pub to: String,
    pub amount: u128,
    pub block_number: u64,
}

/// Result of a D03 recent-flow probe. Carries both the aggregated
/// `net_flows` (used by D03 concentration math) and the raw `transfers`
/// list (used by D05 wash-trade ping-pong detection and D11 burst
/// time-bucketing).
#[derive(Debug, Clone)]
pub struct RecentHolderFlows {
    /// `(from_block, to_block)` actually scanned.
    pub window: (u64, u64),
    /// Total Transfer events observed.
    pub transfer_count: usize,
    /// `true` when the result count hit the RPC's per-call cap (most public
    /// RPCs return at most 10,000 logs); the data is incomplete and the
    /// concentration math is biased toward only the most recent slice of
    /// the requested window.
    pub truncated: bool,
    /// Net-flow per address, descending. Only addresses with positive net
    /// (received > sent) are included — they're the "active accumulators"
    /// the recent-window concentration metric is computed from.
    pub net_flows: Vec<(String, u128)>,
    /// Raw Transfer edges in observation order — input for D05 / D11.
    pub transfers: Vec<TransferEdge>,
}

/// `Transfer(address indexed from, address indexed to, uint256 value)` topic0.
/// keccak256("Transfer(address,address,uint256)").
const TRANSFER_TOPIC: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// How many blocks to look back from `latest` for the D03 sample. 2,000
/// blocks ≈ 7 h on Ethereum, ~2 h on BSC — kept narrow enough that
/// high-volume tokens (USDT, USDC, WETH) don't blow the public-RPC
/// response-size cap, while still wide enough to give a meaningful
/// recent-flow snapshot for memecoins.
const D03_LOOKBACK_BLOCKS: u64 = 2_000;

/// Probe recent holder flows for `token_addr` by scanning `Transfer` events
/// from the last `D03_LOOKBACK_BLOCKS` blocks. The returned net-flow map
/// is what the concentration math (gini, top-N share) is run against.
///
/// Note: this is **recent-window net concentration**, not total
/// `balanceOf`-weighted distribution. For new tokens (< 7 days old) the
/// two metrics converge; for established tokens this captures whale
/// buying/selling concentration in the window — a different but
/// arguably more actionable signal for active-trader use cases.
pub async fn fetch_recent_holder_flows(
    http_url: &str,
    token_addr: &str,
) -> Result<RecentHolderFlows, AdapterError> {
    let token = normalise_addr(token_addr)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| AdapterError::Config(format!("reqwest client build error: {e}")))?;

    let latest = eth_block_number(&client, http_url).await?;
    let from_block = latest.saturating_sub(D03_LOOKBACK_BLOCKS);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getLogs",
        "params": [{
            "address": token,
            "fromBlock": format!("0x{from_block:x}"),
            "toBlock": format!("0x{latest:x}"),
            "topics": [TRANSFER_TOPIC],
        }]
    });

    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getLogs HTTP error: {e}")))?;
    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getLogs parse error: {e}")))?;
    if let Some(err) = json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("eth_getLogs Transfer error: {err}"),
        });
    }
    let logs = json
        .get("result")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AdapterError::Transport("eth_getLogs: missing result array".to_owned()))?;

    let transfer_count = logs.len();
    // Many public RPCs cap `eth_getLogs` responses at exactly 10,000
    // entries — a count of EXACTLY 10,000 is a near-certain truncation.
    // publicnode.com / 1rpc.io / mainnet.io can return well over 10,000
    // entries on a single call, so a high raw count alone is not enough
    // to flag truncation.
    let truncated = transfer_count == 10_000;

    // Net-flow map: i128 because addresses can be net-out (negative) when
    // they sent more than they received in the window.
    let mut flows: std::collections::HashMap<String, i128> =
        std::collections::HashMap::with_capacity(transfer_count.min(50_000));
    let mut transfers: Vec<TransferEdge> = Vec::with_capacity(transfer_count);
    for log in logs {
        let topics = log.get("topics").and_then(|t| t.as_array());
        let topics = match topics {
            Some(t) if t.len() >= 3 => t,
            _ => continue, // Malformed log — skip.
        };
        let from_addr = match topic_to_address(&topics[1]) {
            Some(a) => a,
            None => continue,
        };
        let to_addr = match topic_to_address(&topics[2]) {
            Some(a) => a,
            None => continue,
        };
        // Skip self-transfers (they net to zero anyway).
        if from_addr == to_addr {
            continue;
        }
        let data_hex = log
            .get("data")
            .and_then(|d| d.as_str())
            .unwrap_or("0x");
        let amount = match parse_u256_data(data_hex) {
            Some(v) => v,
            None => continue,
        };
        let block_hex = log
            .get("blockNumber")
            .and_then(|v| v.as_str())
            .unwrap_or("0x0");
        let block_number = u64::from_str_radix(block_hex.trim_start_matches("0x"), 16)
            .unwrap_or(0);
        // Cap the per-event amount at i128::MAX / 2 so a single absurd
        // event can't overflow the accumulator. Values that big aren't
        // realistic ERC-20 transfers anyway — they would imply 10^21+
        // tokens at standard 18 decimals.
        let signed = amount.min(i128::MAX as u128 / 2) as i128;

        // From-side: subtract. Skip the zero address (mints).
        if from_addr != "0x0000000000000000000000000000000000000000" {
            *flows.entry(from_addr.clone()).or_insert(0) -= signed;
        }
        // To-side: add. Skip the zero address (burns).
        if to_addr != "0x0000000000000000000000000000000000000000" {
            *flows.entry(to_addr.clone()).or_insert(0) += signed;
        }
        transfers.push(TransferEdge {
            from: from_addr,
            to: to_addr,
            amount,
            block_number,
        });
    }

    // Filter to addresses with strictly positive net flow, descending.
    let mut net: Vec<(String, u128)> = flows
        .into_iter()
        .filter_map(|(a, n)| if n > 0 { Some((a, n as u128)) } else { None })
        .collect();
    net.sort_by(|a, b| b.1.cmp(&a.1));

    Ok(RecentHolderFlows {
        window: (from_block, latest),
        transfer_count,
        truncated,
        net_flows: net,
        transfers,
    })
}

/// Topic-encoded address: `topics[i]` is a `0x…` 32-byte hex string with
/// the address right-aligned (lower 20 bytes). Returns the lowercased
/// `0x…40hex` address string or `None` on a malformed topic.
fn topic_to_address(topic: &Value) -> Option<String> {
    let s = topic.as_str()?;
    let raw = s.strip_prefix("0x").unwrap_or(s);
    if raw.len() != 64 {
        return None;
    }
    Some(format!("0x{}", &raw[24..].to_ascii_lowercase()))
}

/// Parse the first 32 bytes of an ABI-encoded `data` field as a u256
/// little- or big-endian (the EVM is big-endian) into a u128. Saturates
/// at u128::MAX when the value exceeds 16 bytes — fine for the D03
/// concentration math which only cares about relative magnitudes.
fn parse_u256_data(data_hex: &str) -> Option<u128> {
    let raw = data_hex.strip_prefix("0x").unwrap_or(data_hex);
    if raw.len() < 64 {
        return None;
    }
    let bytes = hex::decode(&raw[..64]).ok()?;
    // Take low 16 bytes (last 16 of 32) — most ERC-20 amounts fit easily.
    let low: [u8; 16] = bytes[16..32].try_into().ok()?;
    // If high 16 bytes are non-zero, saturate.
    if bytes[..16].iter().any(|b| *b != 0) {
        return Some(u128::MAX);
    }
    Some(u128::from_be_bytes(low))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn normalise_addr(s: &str) -> Result<String, AdapterError> {
    let raw = s.strip_prefix("0x").unwrap_or(s);
    if raw.len() != 40 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AdapterError::Config(format!(
            "invalid EVM address (expected 40 hex chars, optionally 0x-prefixed): {s}"
        )));
    }
    Ok(format!("0x{}", raw.to_ascii_lowercase()))
}

fn calldata_hex(selector: &[u8; 4]) -> String {
    let mut out = String::with_capacity(2 + 8);
    out.push_str("0x");
    for b in selector {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Generic `eth_call` request. Returns empty `Vec<u8>` on revert (so callers
/// treat absence-of-method as "field not present").
async fn eth_call(
    client: &Client,
    http_url: &str,
    to: &str,
    selector: &[u8; 4],
) -> Result<Vec<u8>, AdapterError> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": [
            { "to": to, "data": calldata_hex(selector) },
            "latest"
        ]
    });

    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_call HTTP error: {e}")))?;

    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_call parse error: {e}")))?;

    if let Some(err) = json.get("error") {
        // Treat reverts and "execution reverted" as soft errors (empty result).
        let err_str = err.to_string();
        if err_str.contains("revert") || err_str.contains("invalid opcode") {
            return Ok(Vec::new());
        }
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("eth_call error: {err}"),
        });
    }

    let result = json
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AdapterError::Transport("eth_call: missing result".to_owned()))?;
    decode_hex_prefixed(result)
}

async fn eth_get_code(client: &Client, http_url: &str, addr: &str) -> Result<Vec<u8>, AdapterError> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getCode",
        "params": [addr, "latest"]
    });

    let response = client
        .post(http_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getCode HTTP error: {e}")))?;

    let json: Value = response
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("eth_getCode parse error: {e}")))?;

    if let Some(err) = json.get("error") {
        return Err(AdapterError::RpcError {
            slot: 0,
            reason: format!("eth_getCode error: {err}"),
        });
    }

    let result = json
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AdapterError::Transport("eth_getCode: missing result".to_owned()))?;
    decode_hex_prefixed(result)
}

fn decode_hex_prefixed(s: &str) -> Result<Vec<u8>, AdapterError> {
    let raw = s.strip_prefix("0x").unwrap_or(s);
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    hex::decode(raw)
        .map_err(|e| AdapterError::Transport(format!("hex decode error on {s:?}: {e}")))
}

fn bytecode_contains(bytecode: &[u8], selector: &[u8; 4]) -> bool {
    bytecode.windows(4).any(|w| w == selector)
}

/// Decode an ABI-encoded `string` return value: `[offset:32][length:32][data:padded]`.
fn decode_string(raw: &[u8]) -> Option<String> {
    if raw.len() < 64 {
        // Some ERC-20 tokens (older contracts, e.g. MKR) return string as
        // a fixed bytes32 — try that fallback first.
        if raw.len() == 32 {
            // Trim trailing zero bytes.
            let trimmed: Vec<u8> = raw.iter().copied().take_while(|b| *b != 0).collect();
            return std::str::from_utf8(&trimmed).ok().map(|s| s.to_owned());
        }
        return None;
    }
    let len = u256_low_usize(&raw[32..64])?;
    if len == 0 || raw.len() < 64 + len {
        return None;
    }
    let data = &raw[64..64 + len];
    std::str::from_utf8(data).ok().map(|s| s.to_owned())
}

fn decode_u8(raw: &[u8]) -> Option<u8> {
    if raw.len() < 32 {
        return None;
    }
    Some(raw[31])
}

fn decode_bool(raw: &[u8]) -> Option<bool> {
    if raw.len() < 32 {
        return None;
    }
    Some(raw[31] != 0)
}

fn decode_address(raw: &[u8]) -> Option<String> {
    if raw.len() < 32 {
        return None;
    }
    let addr_bytes = &raw[12..32];
    Some(format!("0x{}", hex::encode(addr_bytes)))
}

fn decode_u256_decimal_string(raw: &[u8]) -> Option<String> {
    if raw.len() < 32 {
        return None;
    }
    // Convert big-endian u256 → decimal string via a manual base-10
    // accumulator. We avoid pulling in num-bigint by treating the 32-byte
    // value as four u64 limbs and dividing repeatedly. For CLI display this
    // is well within budget.
    let mut limbs = [0u64; 4];
    for (i, limb) in limbs.iter_mut().enumerate() {
        let start = i * 8;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&raw[start..start + 8]);
        *limb = u64::from_be_bytes(bytes);
    }
    if limbs.iter().all(|&l| l == 0) {
        return Some("0".to_owned());
    }
    let mut digits: Vec<u8> = Vec::with_capacity(80);
    while limbs.iter().any(|&l| l != 0) {
        // Divide the 256-bit value by 10, collect remainder.
        let mut remainder: u128 = 0;
        for limb in limbs.iter_mut() {
            let value = (remainder << 64) | (*limb as u128);
            *limb = (value / 10) as u64;
            remainder = value % 10;
        }
        digits.push(remainder as u8);
    }
    Some(
        digits
            .iter()
            .rev()
            .map(|d| char::from(b'0' + d))
            .collect(),
    )
}

fn u256_low_usize(raw: &[u8]) -> Option<usize> {
    if raw.len() < 32 {
        return None;
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&raw[24..32]);
    let v = u64::from_be_bytes(bytes);
    usize::try_from(v).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_addresses() {
        assert_eq!(
            normalise_addr("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap(),
            "0xdac17f958d2ee523a2206206994597c13d831ec7"
        );
        assert_eq!(
            normalise_addr("dAC17F958D2ee523a2206206994597C13D831ec7").unwrap(),
            "0xdac17f958d2ee523a2206206994597c13d831ec7"
        );
        assert!(normalise_addr("0x123").is_err());
        assert!(normalise_addr("0xZZ17F958D2ee523a2206206994597C13D831ec7").is_err());
    }

    #[test]
    fn decodes_u8_decimals() {
        let mut raw = vec![0u8; 32];
        raw[31] = 18;
        assert_eq!(decode_u8(&raw), Some(18));
        assert_eq!(decode_u8(&raw[..31]), None);
    }

    #[test]
    fn decodes_bool() {
        let mut zero = vec![0u8; 32];
        assert_eq!(decode_bool(&zero), Some(false));
        zero[31] = 1;
        assert_eq!(decode_bool(&zero), Some(true));
    }

    #[test]
    fn decodes_dynamic_string() {
        // ABI-encoded "USDT": offset=32, length=4, data=USDT padded.
        let mut raw = vec![0u8; 96];
        raw[31] = 0x20; // offset = 32
        raw[63] = 4; // length = 4
        raw[64] = b'U';
        raw[65] = b'S';
        raw[66] = b'D';
        raw[67] = b'T';
        assert_eq!(decode_string(&raw).as_deref(), Some("USDT"));
    }

    #[test]
    fn decodes_address() {
        let mut raw = vec![0u8; 32];
        // Last 20 bytes carry the address. dac17f95...831ec7 (USDT).
        let addr_bytes = hex::decode("dac17f958d2ee523a2206206994597c13d831ec7").unwrap();
        raw[12..32].copy_from_slice(&addr_bytes);
        assert_eq!(
            decode_address(&raw).as_deref(),
            Some("0xdac17f958d2ee523a2206206994597c13d831ec7")
        );
    }

    #[test]
    fn decodes_u256_total_supply() {
        // 10**18 (1 ether worth) = 1_000_000_000_000_000_000.
        let mut raw = vec![0u8; 32];
        let v: u128 = 1_000_000_000_000_000_000;
        raw[16..32].copy_from_slice(&v.to_be_bytes());
        assert_eq!(decode_u256_decimal_string(&raw).as_deref(), Some("1000000000000000000"));

        let zeroes = vec![0u8; 32];
        assert_eq!(decode_u256_decimal_string(&zeroes).as_deref(), Some("0"));
    }

    #[test]
    fn bytecode_contains_selector() {
        let bytecode = vec![
            0x60, 0x80, 0x60, 0x40, 0x52, // some EVM prelude
            0xa0, 0x71, 0x2d, 0x68, // mint(uint256) selector embedded
            0x60, 0x00,
        ];
        assert!(bytecode_contains(&bytecode, &bytecode_selectors::MINT_UINT256));
        assert!(!bytecode_contains(&bytecode, &bytecode_selectors::MINT_ADDR_UINT256));
    }
}
