//! `onchain-check-token` — pull-based query-engine CLI that runs OUR D03
//! detector math against REAL on-chain holder data fetched via OUR
//! `mg_onchain_chain_adapter::solana` HTTP RPC functions.
//!
//! Sprint 27 (2026-04-28): CLI is the product surface (per
//! feedback_cli_first_product memory). First end-to-end validation that
//! uses our chain-adapter to fetch + our detector signals to compute.
//! No reqwest in this binary, no jq, no curl. The whole stack is OUR code.
//!
//! Usage:
//!   cargo run --bin onchain-check-token -- \
//!     --token orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE
//!
//! Optional `--rpc <http_url>` overrides the default
//! `https://api.mainnet-beta.solana.com`. Self-hosted RPC nodes are
//! recommended for production; mainnet-beta serves `getProgramAccounts`
//! but with high latency.

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use rust_decimal::Decimal;
use url::Url;

use chrono::{DateTime, TimeZone, Utc};

use mg_onchain_chain_adapter::ethereum::{
    AddressClass, EvmTokenMeta, SimulateSellOutcome, classify_address, discover_recent_pairs,
    discover_recent_v3_pools, eth_get_transaction_count, evm_token_metadata,
    fetch_recent_holder_flows, find_contract_age, probe_ownership_events, probe_swap_volume,
    simulate_sell_evm,
};
use mg_onchain_chain_adapter::solana::{
    config::{CommitmentConfig, ReconnectPolicy, SolanaAdapterConfig, SubscribeFiltersConfig},
    subscribe::{
        SolanaAddressClass, TokenHolder, classify_solana_owner, discover_pumpfun_recent,
        get_mint_state, get_oldest_signature_block_time, get_recent_signatures, get_token_holders,
    },
};
use mg_onchain_detectors::signals::{gini_descending, severity_from_confidence, top_n_pct};

#[derive(Parser, Debug)]
#[command(
    name = "onchain-check-token",
    about = "Run mg-onchain-analysis D03 holder-concentration math on a real token via OUR chain-adapter."
)]
struct Args {
    /// Token to investigate. Accepts:
    /// - Solana base58 mint (32-44 chars, e.g. `orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE`)
    /// - EVM `0x…` 20-byte contract address (e.g. `0xdAC17F958D2ee523a2206206994597C13D831ec7`)
    /// - Symbol (`ORCA`, `USDT`, `BONK`) — looked up in known token lists
    /// Chain is auto-detected from the address format. Override via `--chain`
    /// for ambiguous inputs. Optional when `--discover` is set.
    token: Option<String>,

    /// Discover newly-listed tokens via on-chain DEX-factory `PairCreated`
    /// events, instead of analysing one. Requires `--chain ethereum` or
    /// `--chain bsc`. Combine with `--blocks N` to set the lookback window
    /// (default 5000 blocks ≈ 17 h on Ethereum, ~4 h on BSC).
    #[arg(long)]
    discover: bool,

    /// Lookback window for `--discover`, in blocks. Default 5000.
    #[arg(long, default_value_t = 5_000)]
    blocks: u64,

    /// When set with `--discover`, additionally run analytics on the top-N
    /// discovered tokens and output a comparison table. Each row shows the
    /// composite verdict + the leading driving signal so a memecoin trader
    /// can spot the active-owner / pump-spike standouts in one pass.
    #[arg(long)]
    analyze: bool,

    /// How many tokens to analyse when `--discover --analyze` is set.
    /// Default 10. Each token costs ~10 RPC calls; raise carefully on
    /// public endpoints.
    #[arg(long, default_value_t = 10)]
    top: usize,

    /// Optional explicit chain (`solana` / `ethereum` / `bsc`). Useful when
    /// the same address format works on multiple chains — e.g. a `0x…`
    /// address could be on Ethereum or BSC; default is Ethereum.
    #[arg(long)]
    chain: Option<String>,

    /// Optional override for the JSON-RPC endpoint. Each chain has a
    /// working public default that's used when this flag is absent — pass
    /// `--rpc http://your-self-hosted-node` to point at your own RPC.
    #[arg(long)]
    rpc: Option<String>,

    /// Optional path to a captured holders JSON (mainnet
    /// `getTokenLargestAccounts`-shaped, aggregated by owner). When set, the
    /// CLI loads holders from disk instead of calling
    /// `get_token_holders` — turns the regression artefact at
    /// `tests/fixtures/solana/<token>/largest_accounts_full.json` into a
    /// deterministic replay. Mint state, age, signature-rate paths still hit
    /// the live RPC unless those fixtures are wired separately.
    #[arg(long)]
    holders_file: Option<PathBuf>,
}

/// One holders-JSON entry; matches mainnet `getTokenLargestAccounts` shape
/// when aggregated by owner. We only consume `address` (the owner) and
/// `amount` (raw u64 as string).
#[derive(serde::Deserialize)]
struct HoldersFileEntry {
    address: String,
    amount: String,
}

#[derive(serde::Deserialize)]
struct HoldersFileResult {
    value: Vec<HoldersFileEntry>,
}

#[derive(serde::Deserialize)]
struct HoldersFileEnvelope {
    result: HoldersFileResult,
}

fn load_holders_from_file(path: &std::path::Path) -> anyhow::Result<Vec<TokenHolder>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read holders file {}", path.display()))?;
    let env: HoldersFileEnvelope = serde_json::from_str(&raw)
        .with_context(|| format!("parse holders JSON {}", path.display()))?;
    let mut out = Vec::with_capacity(env.result.value.len());
    for e in env.result.value {
        let amount: u64 = e
            .amount
            .parse()
            .with_context(|| format!("amount field is not u64: {}", e.amount))?;
        out.push(TokenHolder {
            owner: e.address,
            amount,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChainKind {
    Solana,
    Ethereum,
    Bsc,
    Base,
    Arbitrum,
    Optimism,
    Polygon,
    Avalanche,
}

impl ChainKind {
    fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw.to_lowercase().as_str() {
            "solana" | "sol" => Ok(Self::Solana),
            "ethereum" | "eth" | "mainnet" => Ok(Self::Ethereum),
            "bsc" | "bnb" | "binance" => Ok(Self::Bsc),
            "base" => Ok(Self::Base),
            "arbitrum" | "arb" => Ok(Self::Arbitrum),
            "optimism" | "op" => Ok(Self::Optimism),
            "polygon" | "matic" => Ok(Self::Polygon),
            "avalanche" | "avax" => Ok(Self::Avalanche),
            other => anyhow::bail!(
                "unsupported --chain {other:?} (supported: solana, ethereum, bsc, base, arbitrum, optimism, polygon, avalanche)"
            ),
        }
    }

    fn default_rpc(self) -> &'static str {
        match self {
            Self::Solana => "https://api.mainnet-beta.solana.com",
            Self::Ethereum => "https://ethereum-rpc.publicnode.com",
            Self::Bsc => "https://bsc-rpc.publicnode.com",
            Self::Base => "https://base-rpc.publicnode.com",
            Self::Arbitrum => "https://arbitrum-one-rpc.publicnode.com",
            Self::Optimism => "https://optimism-rpc.publicnode.com",
            Self::Polygon => "https://polygon-bor-rpc.publicnode.com",
            Self::Avalanche => "https://avalanche-c-chain-rpc.publicnode.com",
        }
    }
}

/// Detect chain from token-address shape. EVM `0x…` defaults to Ethereum;
/// pass `--chain bsc` to override for BSC tokens that share the address
/// space.
///
/// Returns `Ok(None)` when the input is neither a Solana base58 mint nor an
/// EVM 0x-address — caller should attempt symbol resolution as the next
/// step.
fn detect_chain_from_token(token: &str) -> Option<ChainKind> {
    let trimmed = token.trim();
    if let Some(hex) = trimmed.strip_prefix("0x").or(trimmed.strip_prefix("0X")) {
        if hex.len() == 40 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(ChainKind::Ethereum);
        }
    }
    // Solana base58 mints are 32-44 chars over the base58 alphabet (no 0/O/I/l).
    let len = trimmed.len();
    let is_base58 = trimmed
        .chars()
        .all(|c| "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz".contains(c));
    if (32..=44).contains(&len) && is_base58 {
        return Some(ChainKind::Solana);
    }
    None
}

/// One entry from the Solana Foundation token list. Schema follows the
/// token-list spec used across the Solana ecosystem.
#[derive(serde::Deserialize, Debug, Clone)]
struct SolanaTokenListEntry {
    address: String,
    #[serde(default)]
    name: String,
    symbol: String,
    #[serde(default)]
    decimals: u8,
}

#[derive(serde::Deserialize)]
struct SolanaTokenListEnvelope {
    tokens: Vec<SolanaTokenListEntry>,
}

/// URL of the Solana Foundation token list. Open data, GitHub-hosted, no
/// API key. Same trust posture as `api.mainnet-beta.solana.com` — public
/// ecosystem data source the entire Solana DEX ecosystem references.
const SOLANA_TOKEN_LIST_URL: &str =
    "https://raw.githubusercontent.com/solana-labs/token-list/main/src/tokens/solana.tokenlist.json";

/// Cache TTL for the downloaded token list. The list updates rarely; a
/// 24-hour cache is plenty for CLI use and keeps the per-invocation cost at
/// a few milliseconds (disk read) instead of ~30 MB HTTP fetch.
const TOKEN_LIST_CACHE_TTL_SECS: u64 = 24 * 60 * 60;

/// Where the cached token list lives on disk. `~/.cache/onchain-check-token/`.
fn token_list_cache_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut path = PathBuf::from(home);
    path.push(".cache");
    path.push("onchain-check-token");
    Some(path)
}

/// Load the Solana token list, preferring a fresh disk cache. Falls back to
/// HTTP fetch + write-through cache when the local copy is missing or older
/// than `TOKEN_LIST_CACHE_TTL_SECS`.
async fn load_solana_token_list() -> anyhow::Result<Vec<SolanaTokenListEntry>> {
    let cache_path = token_list_cache_path().map(|mut p| {
        p.push("solana-tokens.json");
        p
    });

    if let Some(ref path) = cache_path
        && let Ok(metadata) = std::fs::metadata(path)
        && let Ok(modified) = metadata.modified()
        && let Ok(age) = modified.elapsed()
        && age.as_secs() < TOKEN_LIST_CACHE_TTL_SECS
        && let Ok(raw) = std::fs::read_to_string(path)
        && let Ok(envelope) = serde_json::from_str::<SolanaTokenListEnvelope>(&raw)
    {
        return Ok(envelope.tokens);
    }

    eprintln!("[resolver] downloading Solana token list (~30MB; cached locally for 24h)…");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("build reqwest client for Solana token list")?;
    let raw = client
        .get(SOLANA_TOKEN_LIST_URL)
        .send()
        .await
        .context("fetch Solana token list")?
        .text()
        .await
        .context("read Solana token list body")?;
    let envelope: SolanaTokenListEnvelope =
        serde_json::from_str(&raw).context("parse Solana token list JSON")?;

    if let Some(path) = cache_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(err) = std::fs::write(&path, &raw) {
            eprintln!("[resolver] warning: failed to write token-list cache to {}: {err}", path.display());
        }
    }

    Ok(envelope.tokens)
}

/// Hard-coded canonical Solana mint addresses for the most-traded tokens.
/// The Foundation token-list (solana-labs/token-list) was frozen in 2021
/// and misses everything launched after — JUP, WIF, POPCAT, the modern
/// BONK, etc. Worse, it contains stale entries that share names with
/// modern tokens (an old "BONK" pre-dates the canonical Solana memecoin).
/// This curated map ships with the binary as the **first source of
/// truth** for a small list of well-known mints; the Foundation list
/// is consulted only when the symbol isn't hard-coded here.
///
/// Entries: `(SYMBOL_UPPERCASE, mint_address, name, decimals)`.
const SOLANA_CURATED_MINTS: &[(&str, &str, &str, u8)] = &[
    // Stablecoins
    ("USDC", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USD Coin", 6),
    ("USDT", "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", "Tether USD (Wormhole)", 6),
    ("PYUSD", "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo", "PayPal USD", 6),
    // Major DeFi / DEX governance
    ("ORCA", "orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE", "Orca", 6),
    ("RAY", "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R", "Raydium", 6),
    ("JUP", "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN", "Jupiter", 6),
    ("KMNO", "KMNo3nJsBXfcpJTVhZcXLW7RmTwTt4GVFE7suUBo9sS", "Kamino", 6),
    // Wrapped / bridged
    ("SOL", "So11111111111111111111111111111111111111112", "Wrapped SOL", 9),
    ("WSOL", "So11111111111111111111111111111111111111112", "Wrapped SOL", 9),
    // Memecoins (canonical mints — many scam tokens share these symbols)
    ("BONK", "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", "Bonk", 5),
    ("WIF", "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm", "dogwifhat", 6),
    ("POPCAT", "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr", "Popcat", 9),
    ("MEW", "MEW1gQWJ3nEXg2qgERiKu7FAFj79PHvQVREQUzScPP5", "cat in a dogs world", 5),
    ("BOME", "ukHH6c7mMyiWCf1b9pnWe25TSpkDDt3H5pQZgZ74J82", "BOOK OF MEME", 6),
    ("WEN", "WENWENvqqNya429ubCdR81ZmD69brwQaaBYY6p3LCpk", "Wen", 5),
    ("PENGU", "2zMMhcVQEXDtdE6vsFS7S7D5oUodfJHE8vd1gnBouauv", "Pudgy Penguins", 6),
    ("PNUT", "2qEHjDLDLbuBgRYvsxhc5D6uDWAivNFZGan56P1tpump", "Peanut the Squirrel", 6),
    // Infra / oracle / staking
    ("PYTH", "HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3", "Pyth Network", 6),
    ("JTO", "jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL", "Jito", 9),
    ("RENDER", "rndrizKT3MK1iimdxRdWabcF7Zg7AR5T4nud4EkHBof", "Render", 8),
    ("HNT", "hntyVP6YFm1Hg25TN9WGLqM12b8TQmcknKrdu1oxWux", "Helium", 8),
    ("INF", "5oVNBeEEQvYi1cX3ir8Dx5n1P7pdxydbGF2X4TxVusJm", "Infinity", 9),
    ("MNGO", "MangoCzJ36AjZyKwVj3VnYU4GTonjfVEnJmvvWaxLac", "Mango", 6),
    // Bridge tokens
    ("W", "85VBFQZC9TZkfaptBWjvUw7YbZjy52A6mjtPGjstQAmQ", "Wormhole", 6),
];

/// Resolve a short symbol like `ORCA` against:
/// 1. The hard-coded curated mint map (canonical addresses for top
///    Solana tokens, regardless of foundation-list staleness).
/// 2. As a fallback, the Solana Foundation token list (covers older
///    long-tail tokens not on the curated list).
async fn resolve_solana_symbol(
    symbol: &str,
) -> anyhow::Result<Option<(String, String, u8)>> {
    let needle = symbol.trim().to_uppercase();

    // Step 1: curated map — canonical addresses for the symbols every
    // Solana trader recognises. Avoids the stale-foundation-list trap.
    for (sym, addr, name, decimals) in SOLANA_CURATED_MINTS {
        if *sym == needle.as_str() {
            return Ok(Some(((*addr).to_owned(), (*name).to_owned(), *decimals)));
        }
    }

    // Step 2: foundation list fallback for less-common symbols.
    let tokens = load_solana_token_list().await?;
    Ok(tokens
        .into_iter()
        .find(|t| t.symbol.to_uppercase() == needle)
        .map(|t| (t.address, t.name, t.decimals)))
}

/// One entry from the Uniswap Labs Default token list. Schema is the
/// EIP-stylee TokenLists.org format used across the EVM ecosystem.
#[derive(serde::Deserialize, Debug, Clone)]
struct UniswapTokenListEntry {
    #[serde(rename = "chainId")]
    chain_id: u64,
    address: String,
    #[serde(default)]
    name: String,
    symbol: String,
    #[serde(default)]
    decimals: u8,
}

#[derive(serde::Deserialize)]
struct UniswapTokenListEnvelope {
    tokens: Vec<UniswapTokenListEntry>,
}

/// EIP-3155-ish chain-IDs used across the EVM ecosystem. Stay in sync with
/// Uniswap's published token list.
const ETHEREUM_CHAIN_ID: u64 = 1;
const BSC_CHAIN_ID: u64 = 56;
const BASE_CHAIN_ID: u64 = 8453;
const ARBITRUM_CHAIN_ID: u64 = 42_161;
const OPTIMISM_CHAIN_ID: u64 = 10;
const POLYGON_CHAIN_ID: u64 = 137;
const AVALANCHE_CHAIN_ID: u64 = 43_114;

/// Public Uniswap default token list — open data published for the entire
/// EVM ecosystem (every wallet/DEX uses this list as a baseline).
const UNISWAP_TOKEN_LIST_URL: &str = "https://tokens.uniswap.org/";

async fn load_uniswap_token_list() -> anyhow::Result<Vec<UniswapTokenListEntry>> {
    let cache_path = token_list_cache_path().map(|mut p| {
        p.push("uniswap-tokens.json");
        p
    });

    if let Some(ref path) = cache_path
        && let Ok(metadata) = std::fs::metadata(path)
        && let Ok(modified) = metadata.modified()
        && let Ok(age) = modified.elapsed()
        && age.as_secs() < TOKEN_LIST_CACHE_TTL_SECS
        && let Ok(raw) = std::fs::read_to_string(path)
        && let Ok(envelope) = serde_json::from_str::<UniswapTokenListEnvelope>(&raw)
    {
        return Ok(envelope.tokens);
    }

    eprintln!("[resolver] downloading Uniswap default token list (~600KB; cached locally for 24h)…");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build reqwest client for Uniswap token list")?;
    let raw = client
        .get(UNISWAP_TOKEN_LIST_URL)
        .send()
        .await
        .context("fetch Uniswap default token list")?
        .text()
        .await
        .context("read Uniswap token list body")?;
    let envelope: UniswapTokenListEnvelope =
        serde_json::from_str(&raw).context("parse Uniswap token list JSON")?;

    if let Some(path) = cache_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(err) = std::fs::write(&path, &raw) {
            eprintln!("[resolver] warning: failed to write token-list cache to {}: {err}", path.display());
        }
    }

    Ok(envelope.tokens)
}

/// Resolve `symbol` against the Uniswap list filtered by `target_chain_id`
/// (`1` for Ethereum, `56` for BSC).
async fn resolve_evm_symbol(
    symbol: &str,
    target_chain_id: u64,
) -> anyhow::Result<Option<(String, String, u8)>> {
    let tokens = load_uniswap_token_list().await?;
    let needle = symbol.trim().to_uppercase();
    Ok(tokens
        .into_iter()
        .find(|t| t.chain_id == target_chain_id && t.symbol.to_uppercase() == needle)
        .map(|t| (t.address, t.name, t.decimals)))
}

/// Resolve `symbol` to `(chain, address, name, decimals)` across all known
/// token lists. When `chain_hint` is `Some`, only that chain's list is
/// queried; when `None`, we cascade Ethereum → Solana — common ambiguous
/// symbols (`USDT`/`USDC`/`WETH`/`WBTC`) overwhelmingly mean the ETH
/// version, while Solana-only symbols (`ORCA`/`BONK`/`WIF`/`JUP`) don't
/// collide. To force the Solana version of a cross-chain symbol, pass
/// `--chain solana USDT`.
async fn resolve_symbol_cascade(
    symbol: &str,
    chain_hint: Option<ChainKind>,
) -> anyhow::Result<Option<(ChainKind, String, String, u8)>> {
    match chain_hint {
        Some(ChainKind::Solana) => Ok(resolve_solana_symbol(symbol)
            .await?
            .map(|(a, n, d)| (ChainKind::Solana, a, n, d))),
        Some(ChainKind::Ethereum) => Ok(resolve_evm_symbol(symbol, ETHEREUM_CHAIN_ID)
            .await?
            .map(|(a, n, d)| (ChainKind::Ethereum, a, n, d))),
        Some(ChainKind::Bsc) => Ok(resolve_evm_symbol(symbol, BSC_CHAIN_ID)
            .await?
            .map(|(a, n, d)| (ChainKind::Bsc, a, n, d))),
        Some(ChainKind::Base) => Ok(resolve_evm_symbol(symbol, BASE_CHAIN_ID)
            .await?
            .map(|(a, n, d)| (ChainKind::Base, a, n, d))),
        Some(ChainKind::Arbitrum) => Ok(resolve_evm_symbol(symbol, ARBITRUM_CHAIN_ID)
            .await?
            .map(|(a, n, d)| (ChainKind::Arbitrum, a, n, d))),
        Some(ChainKind::Optimism) => Ok(resolve_evm_symbol(symbol, OPTIMISM_CHAIN_ID)
            .await?
            .map(|(a, n, d)| (ChainKind::Optimism, a, n, d))),
        Some(ChainKind::Polygon) => Ok(resolve_evm_symbol(symbol, POLYGON_CHAIN_ID)
            .await?
            .map(|(a, n, d)| (ChainKind::Polygon, a, n, d))),
        Some(ChainKind::Avalanche) => Ok(resolve_evm_symbol(symbol, AVALANCHE_CHAIN_ID)
            .await?
            .map(|(a, n, d)| (ChainKind::Avalanche, a, n, d))),
        None => {
            // Look up both lists and surface the collision when found, so
            // the user knows whether their `ORCA` was the Solana DEX token
            // or the unrelated ETH "ORCA Alliance" coin sharing the symbol.
            let evm = resolve_evm_symbol(symbol, ETHEREUM_CHAIN_ID).await?;
            let sol = resolve_solana_symbol(symbol).await?;
            match (evm, sol) {
                (Some((ea, en, ed)), Some((sa, sn, sd))) => {
                    eprintln!("[resolver] {:?} found on multiple chains:", symbol);
                    eprintln!("[resolver]   - Ethereum: {} ({}, {} decimals)", ea, en, ed);
                    eprintln!("[resolver]   - Solana:   {} ({}, {} decimals)", sa, sn, sd);
                    eprintln!("[resolver] using Ethereum by default cascade priority; pass `--chain solana {}` to investigate the Solana version instead.", symbol);
                    Ok(Some((ChainKind::Ethereum, ea, en, ed)))
                }
                (Some((a, n, d)), None) => Ok(Some((ChainKind::Ethereum, a, n, d))),
                (None, Some((a, n, d))) => Ok(Some((ChainKind::Solana, a, n, d))),
                (None, None) => Ok(None),
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args = Args::parse();

    // Discovery mode short-circuits the per-token analysis path.
    if args.discover {
        let chain = match args.chain.as_deref() {
            Some(c) => ChainKind::parse(c)?,
            None => anyhow::bail!(
                "--discover requires --chain ethereum / --chain bsc / --chain solana"
            ),
        };
        let rpc = args
            .rpc
            .clone()
            .unwrap_or_else(|| chain.default_rpc().to_owned());
        return match chain {
            ChainKind::Ethereum | ChainKind::Bsc => {
                let is_bsc = matches!(chain, ChainKind::Bsc);
                run_discover(&rpc, is_bsc, args.blocks, chain, args.analyze, args.top).await
            }
            ChainKind::Base => run_discover_base(&rpc, args.blocks, args.analyze, args.top).await,
            ChainKind::Arbitrum => run_discover_arbitrum(&rpc, args.blocks, args.analyze, args.top).await,
            ChainKind::Optimism => run_discover_v3(&rpc, args.blocks, args.analyze, args.top, "Optimism", "optimism", "0x4200000000000000000000000000000000000006").await,
            ChainKind::Polygon => run_discover_v3(&rpc, args.blocks, args.analyze, args.top, "Polygon", "polygon", "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270").await,
            ChainKind::Avalanche => run_discover_v3(&rpc, args.blocks, args.analyze, args.top, "Avalanche", "avalanche", "0xB31f66AA3C1e785363F0875A1B74E27b85FD66c7").await,
            ChainKind::Solana => run_discover_solana(&rpc, args.analyze, args.top).await,
        };
    }

    let token = args.token.clone().ok_or_else(|| {
        anyhow::anyhow!("missing positional <token> argument (or use --discover to list fresh tokens)")
    })?;
    args.token = Some(token.clone());

    // Resolve the token argument into (chain, address):
    //   - If it parses as a recognised address (base58 32-44 → Solana, or
    //     0x + 40 hex → EVM), use that. `--chain` overrides the auto-detected
    //     chain in case the same address shape exists across multiple EVMs.
    //   - Otherwise treat it as a symbol and look it up in token lists:
    //     `--chain solana` → Solana token list; `--chain ethereum` / `--chain bsc`
    //     → Uniswap default list filtered by chainId; no `--chain` → cascade
    //     Solana first, then Ethereum.
    let chain_hint = args.chain.as_deref().map(ChainKind::parse).transpose()?;
    let chain = if let Some(detected) = detect_chain_from_token(&token) {
        chain_hint.unwrap_or(detected)
    } else {
        eprintln!("[resolver] {:?} is not an address; querying token lists…", token);
        match resolve_symbol_cascade(&token, chain_hint).await? {
            Some((resolved_chain, address, name, decimals)) => {
                eprintln!(
                    "[resolver] {:?} → {} ({:?}, {} decimals, name={:?})",
                    token, address, resolved_chain, decimals, name
                );
                args.token = Some(address);
                resolved_chain
            }
            None => anyhow::bail!(
                "could not resolve {:?}: not a Solana base58 mint, not an EVM 0x-address, \
                 and not found in any known token list (Solana Foundation list, Uniswap default). \
                 Pass the full address or use --chain <chain> with an explicit address.",
                token
            ),
        }
    };
    let rpc = args
        .rpc
        .clone()
        .unwrap_or_else(|| chain.default_rpc().to_owned());

    let resolved_token: String = args
        .token
        .clone()
        .expect("token must be resolved by this point in main");

    println!("== onchain-check-token ==");
    println!("token: {}", resolved_token);
    println!("chain: {}", match chain {
        ChainKind::Solana => "solana",
        ChainKind::Ethereum => "ethereum",
        ChainKind::Bsc => "bsc",
        ChainKind::Base => "base",
        ChainKind::Arbitrum => "arbitrum",
        ChainKind::Optimism => "optimism",
        ChainKind::Polygon => "polygon",
        ChainKind::Avalanche => "avalanche",
    });
    // RPC endpoint is a deployment detail; only print it when overridden so
    // the user knows their --rpc actually took effect.
    if args.rpc.is_some() {
        println!("rpc override: {}", rpc);
    }
    println!();

    match chain {
        ChainKind::Solana => run_solana(&args, &resolved_token, &rpc).await,
        ChainKind::Ethereum
        | ChainKind::Bsc
        | ChainKind::Base
        | ChainKind::Arbitrum
        | ChainKind::Optimism
        | ChainKind::Polygon
        | ChainKind::Avalanche => run_evm(&args, &resolved_token, &rpc, chain).await,
    }
}

async fn run_solana(args: &Args, token: &str, rpc: &str) -> anyhow::Result<()> {
    let http_url = Url::parse(rpc)
        .with_context(|| format!("invalid --rpc URL: {}", rpc))?;
    let ws_url = Url::parse("ws://127.0.0.1:8900")
        .expect("hard-coded ws placeholder must parse");
    let config = SolanaAdapterConfig {
        http_url,
        ws_url,
        auth_token: None,
        commitment: CommitmentConfig::Confirmed,
        reconnect: ReconnectPolicy::default(),
        filters: SubscribeFiltersConfig::default(),
        checkpoint_path: "/tmp/onchain-check-token-noop.checkpoint".to_owned(),
    };

    let mut verdicts: Vec<DetectorVerdict> = Vec::new();

    // ------------------------------------------------------------------------
    // Step 1: fetch the FULL holder list — either from a captured fixture
    // (deterministic replay) or via OUR chain-adapter against live RPC.
    // ------------------------------------------------------------------------
    let holders = if let Some(ref path) = args.holders_file {
        eprintln!("[fixture-replay] loading holders from {} (RPC fetch skipped)", path.display());
        load_holders_from_file(path)?
    } else {
        eprintln!("[chain-adapter] calling get_token_holders(...) — SPL getProgramAccounts under the hood");
        get_token_holders(&config, token)
            .await
            .context("chain-adapter::solana::get_token_holders failed")?
    };

    let total_accounts = holders.len();
    let active: Vec<&_> = holders.iter().filter(|h| h.amount > 0).collect();
    let zero_count = total_accounts - active.len();
    let total_supply_raw: u128 = active.iter().map(|h| h.amount as u128).sum();

    println!("== fetch via mg_onchain_chain_adapter::solana ==");
    println!("  token accounts returned: {}", total_accounts);
    println!("  non-zero holders:        {}", active.len());
    println!("  zero-balance accounts:   {}", zero_count);
    println!("  summed supply (active):  {} raw", total_supply_raw);
    println!();

    if active.is_empty() {
        anyhow::bail!("no non-zero holders for token {}", token);
    }

    // ------------------------------------------------------------------------
    // Step 2: sort holders desc + classify top-20 owners (DEX vault vs wallet).
    // ------------------------------------------------------------------------
    let mut sorted: Vec<&_> = active.clone();
    sorted.sort_by_key(|h| std::cmp::Reverse(h.amount));

    // Classify top-20 owners — replay/fixture mode skips RPC since we don't
    // have a way to verify off-chain whether an owner is a DEX vault.
    println!("== D03 holder_concentration via mg_onchain_detectors::signals ==");
    let mut classified: Vec<(&TokenHolder, SolanaAddressClass)> = Vec::new();
    let mut suppressed: Vec<(String, SolanaAddressClass)> = Vec::new();
    if args.holders_file.is_some() {
        eprintln!("[d03] holders-file replay mode — skipping owner classification (would need live RPC)");
        for h in sorted.iter().take(20) {
            classified.push((*h, SolanaAddressClass::Unknown));
        }
    } else {
        eprintln!("[d03] classifying top-20 owners via getAccountInfo.owner (DEX vault / wallet)");
        for h in sorted.iter().take(20) {
            let class = classify_solana_owner(&config, &h.owner).await;
            if matches!(class, SolanaAddressClass::DexVault(_) | SolanaAddressClass::KnownProgram(_))
            {
                suppressed.push((h.owner.clone(), class.clone()));
            }
            classified.push((*h, class));
        }
    }
    for h in sorted.iter().skip(20) {
        classified.push((*h, SolanaAddressClass::Unknown));
    }

    // Filter: real wallets + Unknown stay; DEX vaults / known programs are
    // suppressed. Note: replay-mode treats everything as Unknown so the
    // pre-suppression numbers still reproduce.
    let real_holders: Vec<&TokenHolder> = classified
        .iter()
        .filter(|(_, c)| !matches!(c, SolanaAddressClass::DexVault(_) | SolanaAddressClass::KnownProgram(_)))
        .map(|(h, _)| *h)
        .collect();

    if !suppressed.is_empty() {
        println!(
            "  entity-label suppression: {} of top-20 are DEX vaults / known programs, excluded from gini math",
            suppressed.len()
        );
        for (owner, class) in &suppressed {
            println!("    - {owner}  →  {class:?}");
        }
    }

    let balances_desc: Vec<Decimal> =
        real_holders.iter().map(|h| Decimal::from(h.amount)).collect();
    let gini = gini_descending(&balances_desc);
    let top10_pct = top_n_pct(&balances_desc, 10);

    println!(
        "  gini_descending ({} real wallets, post-suppression) = {}",
        real_holders.len(),
        gini
    );
    println!(
        "  top_n_pct({}, 10) = {} ({}%)",
        real_holders.len(),
        top10_pct,
        top10_pct * Decimal::new(100, 0)
    );
    println!();

    // Recalibrated bands matching the EVM side (T27-26): HIGH only at
    // ≥95 % top-10 share post-suppression — that's a near-monopoly. 75-95
    // = MEDIUM whale-dominated. 50-75 = LOW. <50 = INFO/distributed.
    let high_threshold = Decimal::new(95, 2);
    let medium_threshold = Decimal::new(75, 2);
    let low_threshold = Decimal::new(50, 2);

    let (severity, signal_3_fires, rationale) = if top10_pct >= high_threshold {
        (
            "HIGH",
            true,
            format!(
                "top-10 wallet share {top10_pct} ≥ 0.95 — near-monopoly whale concentration (post-suppression of {} infrastructure addresses)",
                suppressed.len()
            ),
        )
    } else if top10_pct >= medium_threshold {
        (
            "MEDIUM",
            false,
            format!("top-10 wallet share {top10_pct} between 0.75-0.95 — whale-dominated"),
        )
    } else if top10_pct >= low_threshold {
        (
            "LOW",
            false,
            format!("top-10 wallet share {top10_pct} between 0.50-0.75 — moderate concentration"),
        )
    } else {
        (
            "INFO",
            false,
            format!("top-10 wallet share {top10_pct} < 0.50 — distributed real holders"),
        )
    };

    println!("== verdict ==");
    println!("  severity:  {}", severity);
    println!("  rationale: {}", rationale);
    println!();

    let d03_confidence = if signal_3_fires {
        0.85
    } else if top10_pct >= medium_threshold {
        0.55
    } else if top10_pct >= low_threshold {
        0.30
    } else {
        0.10
    };
    let (d03_label, d03_fired) = match severity {
        s if s.starts_with("HIGH") => ("HIGH", true),
        "MEDIUM" => ("MEDIUM", true),
        "LOW" => ("LOW", true),
        _ => ("INFO", true),
    };
    verdicts.push(DetectorVerdict {
        id: "d03_holder_concentration",
        fired: d03_fired,
        confidence: d03_confidence,
        severity_label: d03_label,
        rationale: rationale.clone(),
    });

    println!("top-10 holder owners (from getProgramAccounts):");
    for (i, h) in sorted.iter().take(10).enumerate() {
        let pct = if total_supply_raw > 0 {
            (h.amount as f64) / (total_supply_raw as f64) * 100.0
        } else {
            0.0
        };
        println!("  {:2}. {} | {} raw | {:.4}%", i + 1, h.owner, h.amount, pct);
    }
    println!();

    // ------------------------------------------------------------------------
    // D01 — honeypot static signals (Solana branch from d01_honeypot::compute_static)
    //
    // For Solana tokens D01 is a STATE-ONLY detector (no simulate-sell yet on
    // this branch — see crates/detectors/src/d01_honeypot.rs comment §S6).
    // The raw score formula:
    //   S1 freeze_authority_active     → +0.25  (or +0.10 if non_transferable, attenuation)
    //   S2 transfer_fee_bps > threshold → +0.45 * sigmoid((fee_frac - 0.50) / 0.20)
    //      transfer_fee_authority_active → +0.10 extra (config default)
    //   S3 permanent_delegate_active   → +0.20
    //   S4 transfer_hook_active        → +0.20  (Token-2022 only)
    //   S6 non_transferable            → handled in S1 attenuation
    //   static_conf = sigmoid(raw / 0.55 - 1.0)
    //
    // For regular SPL tokens (no Token-2022 extensions): S2/S3/S4 all 0; only
    // S1 (freeze authority) contributes. ORCA has freeze=None → raw=0 → static_conf≈0.12.
    // ------------------------------------------------------------------------
    eprintln!("[chain-adapter] calling get_mint_state(...) — getAccountInfo + SPL Mint layout decode");
    let mint = get_mint_state(&config, token)
        .await
        .context("chain-adapter::solana::get_mint_state failed")?;

    println!("== D02/D06 mint state via mg_onchain_chain_adapter::solana::get_mint_state ==");
    println!("  supply (raw):     {} ({} ui at {} decimals)",
             mint.supply,
             mint.supply as f64 / 10f64.powi(mint.decimals as i32),
             mint.decimals);
    println!("  is_initialized:   {}", mint.is_initialized);
    match &mint.mint_authority {
        Some(pk) => println!("  mint_authority:   Some({})  ← can mint more — RUG-PREP RISK if unverified", pk),
        None     => println!("  mint_authority:   None  ← supply is fixed (renounced)"),
    }
    match &mint.freeze_authority {
        Some(pk) => println!("  freeze_authority: Some({})  ← holders can be frozen — RISK", pk),
        None     => println!("  freeze_authority: None  ← cannot freeze any holder"),
    }
    println!();

    // D02 / D06 verdict from state
    let mut state_signal_count = 0u8;
    let mut state_rationales: Vec<String> = Vec::new();
    if mint.mint_authority.is_some() {
        state_signal_count += 1;
        state_rationales.push("mint authority active (RUG-PREP risk; legit if DAO PDA)".to_owned());
    }
    if mint.freeze_authority.is_some() {
        state_signal_count += 1;
        state_rationales.push("freeze authority active (token can freeze holders)".to_owned());
    }
    let state_confidence = match state_signal_count {
        0 => 0.0,
        1 => 0.40,
        2 => 0.60,
        _ => 0.0,
    };
    let state_severity = severity_from_confidence(state_confidence);
    println!("  D02/D06 verdict (mint-state heuristic): severity={:?} confidence={:.2}", state_severity, state_confidence);
    let state_combined_rationale = if state_rationales.is_empty() {
        "mint+freeze authorities both renounced — clean fixed-supply token".to_owned()
    } else {
        state_rationales.join("; ")
    };
    if state_rationales.is_empty() {
        println!("    rationale: mint+freeze authorities both renounced — clean fixed-supply token");
    } else {
        for r in &state_rationales {
            println!("    rationale: {}", r);
        }
    }
    println!();
    let d02_label = match state_signal_count {
        0 => "INFO",
        1 => "MEDIUM",
        _ => "HIGH",
    };
    verdicts.push(DetectorVerdict {
        id: "d02_d06_mint_authority",
        fired: true,
        confidence: state_confidence,
        severity_label: d02_label,
        rationale: state_combined_rationale,
    });

    // D01 honeypot static — for SPL tokens this collapses to S1 (freeze).
    // Token-2022 extensions (S2/S3/S4) require TLV parsing of the mint account
    // — out of scope for v1 CLI; if the token is regular SPL (token_program =
    // TokenkegQfeZ...) those signals are correctly 0 by definition.
    let s1_freeze = if mint.freeze_authority.is_some() { 0.25_f64 } else { 0.0_f64 };
    let s2_fee = 0.0_f64; // Token-2022 only — out of scope here
    let s3_delegate = 0.0_f64; // Token-2022 only
    let s4_hook = 0.0_f64; // Token-2022 only
    let raw_d01 = s1_freeze + s2_fee + s3_delegate + s4_hook;
    let static_conf_d01 = sigmoid(raw_d01 / 0.55 - 1.0);
    let d01_severity = severity_from_confidence(static_conf_d01);

    println!("== D01 honeypot static via mg_onchain_detectors (formula from d01_honeypot::compute_static) ==");
    println!("  S1 freeze_authority_active: {} (weight {:+.2})", mint.freeze_authority.is_some(), s1_freeze);
    println!("  S2 transfer_fee_bps:        0 (regular SPL — Token-2022 ext not parsed)");
    println!("  S3 permanent_delegate:      none (regular SPL)");
    println!("  S4 transfer_hook:           none (regular SPL)");
    println!("  raw signal sum:             {:.3}", raw_d01);
    println!("  static_conf (sigmoid):      {:.3}", static_conf_d01);
    println!("  D01 verdict: severity={:?} confidence={:.3}", d01_severity, static_conf_d01);
    // Map D01 confidence to a severity label that is honest about the
    // sigmoid floor: raw=0 produces 0.269 ("Low" by severity_from_confidence)
    // even when no honeypot signal fires; we surface that as INFO in the
    // composite so it doesn't drag a clean SPL into a Low verdict.
    let d01_label_for_composite = if raw_d01 == 0.0 {
        "INFO"
    } else if static_conf_d01 >= 0.60 {
        "HIGH"
    } else if static_conf_d01 >= 0.40 {
        "MEDIUM"
    } else {
        "LOW"
    };
    let d01_confidence_for_composite = if raw_d01 == 0.0 {
        0.0
    } else {
        static_conf_d01
    };
    let d01_rationale = if raw_d01 == 0.0 {
        "no honeypot static signals; regular SPL with renounced freeze authority".to_owned()
    } else {
        format!("static-only D01 raw={raw_d01:.2} → conf {static_conf_d01:.2}")
    };
    verdicts.push(DetectorVerdict {
        id: "d01_honeypot_static",
        fired: true,
        confidence: d01_confidence_for_composite,
        severity_label: d01_label_for_composite,
        rationale: d01_rationale,
    });
    if raw_d01 == 0.0 {
        println!("    rationale: no honeypot static signals; regular SPL with renounced freeze authority");
    } else {
        println!("    rationale: state-only D01 score {raw_d01:.3} → static_conf {static_conf_d01:.3}");
    }
    println!();

    // ------------------------------------------------------------------------
    // D10 — token age (oldest signature on the mint account)
    // ------------------------------------------------------------------------
    eprintln!("[chain-adapter] calling get_oldest_signature_block_time(...) — paginated getSignaturesForAddress");
    let age_result = get_oldest_signature_block_time(&config, token, 50)
        .await
        .context("chain-adapter::solana::get_oldest_signature_block_time failed")?;

    println!("== D10 launch_audit via mg_onchain_chain_adapter::solana ==");
    println!("  pages fetched:    {} (cap 50, ≤ 50,000 signatures)", age_result.pages_fetched);
    println!("  scan complete:    {}", age_result.complete);
    if let Some(ref reason) = age_result.stop_reason {
        println!("  stop reason:      {}", reason);
    }
    match age_result.oldest_block_time {
        Some(secs) => {
            let oldest: DateTime<Utc> = Utc.timestamp_opt(secs, 0).single().unwrap_or_else(Utc::now);
            let now = Utc::now();
            let age_days = (now - oldest).num_days();
            // Important: when scan is INCOMPLETE, oldest_block_time is the
            // oldest-signature-we-managed-to-fetch — NOT a lower bound on
            // token age. A multi-year token with millions of historical
            // signatures returns its 2000 most recent across 2 pages, which
            // can span just a few hours. The "lower bound" interpretation
            // only holds when the observed age is ≥ 7d (then we're past the
            // fresh-launch window regardless of the true genesis).
            println!("  oldest signature observed: {} (UNIX={})", oldest.to_rfc3339(), secs);
            println!("  observed-window age: {} days{}",
                     age_days,
                     if age_result.complete { "" } else { " (incomplete scan; observed window may be much shorter than true token age)" });

            let (sev, conf, rationale) = match (age_result.complete, age_days) {
                (true, d) if d < 7 => {
                    let confidence = (1.0 - (d as f64 / 7.0)).clamp(0.0, 1.0);
                    ("YOUNG (<7d)", confidence,
                     format!("complete scan: age {} days < 7d threshold → D10 fresh-launch signal fires", d))
                }
                (true, d) if d < 30 => ("RECENT (7-30d)", 0.30,
                    format!("complete scan: age {} days — recent but past fresh-launch window", d)),
                (true, d) => ("MATURE (>30d)", 0.0,
                    format!("complete scan: age {} days — established, no fresh-launch signal", d)),
                (false, d) if d >= 7 => ("MATURE (≥7d, incomplete scan sufficient)", 0.0,
                    format!("incomplete scan but observed signatures ≥{}d back → token is at least 7 days old → D10 cannot fire", d)),
                (false, _) => ("UNKNOWN (incomplete scan, observed window <7d)", 0.0,
                    "incomplete scan and observed window <7d — cannot distinguish fresh launch from recent activity on a mature token. Use --rpc <self-hosted> to complete the scan.".to_owned()),
            };
            let severity = severity_from_confidence(conf);
            println!("  D10 verdict: {} (severity={:?}, confidence={:.2})", sev, severity, conf);
            println!("    rationale: {}", rationale);
            let d10_fired = !sev.starts_with("UNKNOWN");
            let d10_label = if !d10_fired {
                "INFO"
            } else if conf >= 0.60 {
                "HIGH"
            } else if conf >= 0.40 {
                "MEDIUM"
            } else if conf >= 0.20 {
                "LOW"
            } else {
                "INFO"
            };
            verdicts.push(DetectorVerdict {
                id: "d10_launch_audit",
                fired: d10_fired,
                confidence: conf,
                severity_label: d10_label,
                rationale: rationale.clone(),
            });
        }
        None => {
            println!("  no oldest_block_time observed");
            println!("  D10 verdict: UNKNOWN — RPC blocked before any signature page returned");
            println!("    rationale: use a self-hosted Solana RPC (--rpc <url>) to bypass public-endpoint throttling");
            verdicts.push(DetectorVerdict {
                id: "d10_launch_audit",
                fired: false,
                confidence: 0.0,
                severity_label: "INFO",
                rationale: "RPC blocked before any signature page returned — use --rpc <self-hosted>".to_owned(),
            });
        }
    }
    println!();

    // ------------------------------------------------------------------------
    // D04 pump-dump (proxy: signature-rate vs trailing baseline)
    //
    // Real D04 needs decoded swap events from DEX programs (Raydium / Orca /
    // Jupiter) — out of scope for v1 CLI. As a PROXY, we count signatures on
    // the mint address per hour-bucket and compare the most-recent hour to the
    // trailing-N-hour average. A 5× spike is suggestive but not conclusive.
    // ------------------------------------------------------------------------
    eprintln!("[chain-adapter] calling get_recent_signatures(...) — page-1 sig list for activity histogram");
    let recent = get_recent_signatures(&config, token, 1)
        .await
        .context("chain-adapter::solana::get_recent_signatures failed")?;

    println!("== D04 pump-dump (proxy via signature-rate, NOT real swap-event detection) ==");
    println!("  signatures fetched: {} (1 page; complete={})", recent.rows.len(), recent.complete);
    if let Some(ref reason) = recent.stop_reason {
        println!("  stop reason:        {}", reason);
    }

    let block_times: Vec<i64> = recent.rows.iter().filter_map(|r| r.block_time).collect();
    if block_times.is_empty() {
        println!("  D04 verdict: UNKNOWN — no block_times returned (RPC blocked before page 1 yielded data)");
        println!();
        println!("== D11 synchronized-activity (proxy via 10s-bucket burst rate) ==");
        println!("  D11-proxy verdict: UNKNOWN — shares signature data path with D04; both blocked by public-RPC throttling");
        println!("    rationale: use --rpc <self-hosted> Solana node to bypass mainnet-beta rate limits");
        println!();
        verdicts.push(DetectorVerdict {
            id: "d04_pump_dump_proxy",
            fired: false,
            confidence: 0.0,
            severity_label: "INFO",
            rationale: "RPC blocked before page 1 yielded data — use --rpc <self-hosted>".to_owned(),
        });
        verdicts.push(DetectorVerdict {
            id: "d11_synchronized_proxy",
            fired: false,
            confidence: 0.0,
            severity_label: "INFO",
            rationale: "shares signature data path with D04; both blocked by public-RPC".to_owned(),
        });
    } else {
        let now_ts = Utc::now().timestamp();
        let oldest_in_window = *block_times.iter().min().unwrap();
        let span_secs = now_ts - oldest_in_window;
        let span_hours = span_secs as f64 / 3600.0;

        // Histogram by hour bucket relative to now.
        let mut buckets: std::collections::BTreeMap<i64, u64> = std::collections::BTreeMap::new();
        for bt in &block_times {
            let hours_ago = ((now_ts - bt) / 3600).max(0);
            *buckets.entry(hours_ago).or_insert(0) += 1;
        }
        let last_hour_count = *buckets.get(&0).unwrap_or(&0);
        let trailing: Vec<u64> = buckets.iter().filter(|(h, _)| **h >= 1).map(|(_, c)| *c).collect();
        let trailing_count: u64 = trailing.iter().sum();
        let trailing_hours = trailing.len().max(1) as f64;
        let trailing_avg = trailing_count as f64 / trailing_hours;

        let ratio = if trailing_avg > 0.0 {
            last_hour_count as f64 / trailing_avg
        } else if last_hour_count > 0 {
            f64::INFINITY
        } else {
            0.0
        };

        let (sev, conf, rationale) = if !ratio.is_finite() {
            ("UNKNOWN", 0.0,
             "trailing baseline = 0 (window covers <1h or no prior activity); cannot compute ratio".to_owned())
        } else if ratio >= 5.0 {
            ("HIGH (potential pump)", 0.70,
             format!("last-hour count {} ≥ 5× trailing avg {:.1}/h → potential pump signal", last_hour_count, trailing_avg))
        } else if ratio >= 2.0 {
            ("MEDIUM (elevated)", 0.50,
             format!("last-hour count {} ≈ {:.1}× trailing avg → elevated but within normal variance", last_hour_count, ratio))
        } else if ratio >= 0.3 {
            ("NORMAL", 0.10,
             format!("last-hour count {} ≈ trailing avg {:.1}/h", last_hour_count, trailing_avg))
        } else {
            ("LOW (cooled off)", 0.0,
             format!("last-hour count {} below {:.1} typical", last_hour_count, trailing_avg))
        };

        println!("  observed window:     {:.1}h (oldest sig {}s ago)", span_hours, span_secs);
        println!("  signatures last 1h:  {}", last_hour_count);
        println!("  trailing avg/h:      {:.2}", trailing_avg);
        println!("  ratio (last/avg):    {}", if ratio.is_finite() { format!("{:.2}×", ratio) } else { "N/A".to_owned() });
        let severity = severity_from_confidence(conf);
        println!("  D04-proxy verdict:   {} (severity={:?}, confidence={:.2})", sev, severity, conf);
        println!("    rationale: {}", rationale);
        println!("    NOTE: this is a SIGNATURE-RATE proxy, not real swap-event D04. Real D04 needs DEX-program filter + price baseline (Sprint 28+).");
        println!();
        let d04_fired = sev != "UNKNOWN";
        let d04_label = if !d04_fired {
            "INFO"
        } else if conf >= 0.60 {
            "HIGH"
        } else if conf >= 0.40 {
            "MEDIUM"
        } else if conf >= 0.20 {
            "LOW"
        } else {
            "INFO"
        };
        verdicts.push(DetectorVerdict {
            id: "d04_pump_dump_proxy",
            fired: d04_fired,
            confidence: conf,
            severity_label: d04_label,
            rationale: rationale.clone(),
        });

        // ----------------------------------------------------------------
        // D11 synchronized-activity (proxy: 10-second burst rate)
        //
        // Real D11 detects multiple distinct wallets buying within the same
        // short window — needs per-tx sender decode (getTransaction per
        // signature, 1000+ RPC calls per page; out of scope for v1 CLI).
        // Proxy: count signatures per 10-second bucket. A 10s window with
        // ≥5 signatures suggests bot-coordinated bursts; ≥10 indicates a
        // strong burst pattern. Cannot distinguish "5 distinct wallets" from
        // "1 wallet sending 5 tx" without sender decode — flag as proxy.
        // ----------------------------------------------------------------
        let mut sec10: std::collections::BTreeMap<i64, u64> = std::collections::BTreeMap::new();
        for bt in &block_times {
            let bucket10 = bt / 10;
            *sec10.entry(bucket10).or_insert(0) += 1;
        }
        let max_burst = *sec10.values().max().unwrap_or(&0);
        let total_buckets = sec10.len();

        println!("== D11 synchronized-activity (proxy via 10s-bucket burst rate, NOT real multi-wallet decode) ==");
        println!("  total 10s buckets observed: {}", total_buckets);
        println!("  peak burst (max sigs/10s):  {}", max_burst);
        let (sev_d11, conf_d11, rationale_d11) = if max_burst >= 10 {
            ("HIGH (burst pattern)", 0.70,
             format!("peak {} signatures in single 10s window — strong burst signal (could be bot-coordinated buys or a single high-frequency trader)", max_burst))
        } else if max_burst >= 5 {
            ("MEDIUM (elevated burst)", 0.50,
             format!("peak {} signatures in 10s window — elevated coordination/burst pattern", max_burst))
        } else if max_burst >= 2 {
            ("LOW (normal bursts)", 0.20,
             format!("peak {} signatures in 10s window — minor bursts within normal patterns", max_burst))
        } else {
            ("NONE", 0.0, "no multi-tx bursts in any 10-second window".to_owned())
        };
        let severity_d11 = severity_from_confidence(conf_d11);
        println!("  D11-proxy verdict: {} (severity={:?}, confidence={:.2})", sev_d11, severity_d11, conf_d11);
        println!("    rationale: {}", rationale_d11);
        println!("    NOTE: cannot distinguish 5 distinct wallets from 1 wallet sending 5 tx without per-tx decode (Sprint 28+).");
        println!();
        let d11_label = if conf_d11 >= 0.60 {
            "HIGH"
        } else if conf_d11 >= 0.40 {
            "MEDIUM"
        } else if conf_d11 >= 0.20 {
            "LOW"
        } else {
            "INFO"
        };
        verdicts.push(DetectorVerdict {
            id: "d11_synchronized_proxy",
            fired: true,
            confidence: conf_d11,
            severity_label: d11_label,
            rationale: rationale_d11.clone(),
        });
    }

    // Detectors not even reached in v1 CLI — surfaced in coverage gap.
    for (id, why) in [
        ("d05_wash_trading", "needs full transfer graph + cycle detection"),
        ("d07_withdraw_withheld", "Token-2022 only; SPL token has no withheld extension"),
        ("d08_sybil", "needs cluster store + funding-graph traversal"),
        ("d09_deployer_changepoint", "needs deployer launch-history"),
    ] {
        verdicts.push(DetectorVerdict {
            id,
            fired: false,
            confidence: 0.0,
            severity_label: "INFO",
            rationale: why.to_owned(),
        });
    }

    print_composite(token, "Solana", &verdicts);
    Ok(())
}

/// Standard logistic sigmoid — matches `mg_onchain_detectors::signals::sigmoid`
/// shape (the inline copy avoids re-exporting the private helper across crates).
fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

// ===========================================================================
// Composite verdict — turn the per-detector ladder into a single TokenScore
// ===========================================================================

/// Per-detector weight applied to confidences before they enter the
/// composite max/mean math. Memecoin trader's perspective: how directly
/// does the detector say "this token can be rugged RIGHT NOW" vs "this is
/// background information"?
///
/// - **1.00 OPERATIONAL** — the detector caught a capability or active
///   action that lets the token be rugged or has already been ruined.
///   Active owner with mint surface, honeypot revert, dormant abandoned.
/// - **0.60-0.90 SUPPORTING** — concentration, volume spike, mint surface
///   without active owner. Risk indicator but not by itself decisive.
/// - **0.50 INFORMATIONAL** — fresh-launch timing. Every new token gets
///   this; needs to combine with operational signals to escalate.
fn detector_weight(detector_id: &str) -> f64 {
    match detector_id {
        "d01_honeypot_static" => 1.0,
        "d02_ownable_owner" => 1.0,
        "d02_recent_renounce" => 1.0,
        "d02_d06_mint_authority" => 1.0,
        "d06_mint_burn" => 0.9,
        "d03_holder_concentration" => 0.7,
        "d03_dormant_token" => 0.9,
        "d04_pump_dump" => 0.6,
        "d05_wash_trading" => 0.85,
        "d11_synchronized" => 0.65,
        "d09_deployer_pattern" => 0.7,
        "d08_sybil_throwaway" => 0.7,
        "d04_pump_dump_proxy" => 0.6,
        "d11_synchronized_proxy" => 0.6,
        "d10_launch_audit" => 0.5,
        _ => 0.7,
    }
}

/// One detector's contribution to the composite verdict. `fired = false`
/// means the detector branch ran but produced no evaluable signal (RPC
/// rate-limit, missing input, not yet wired); we surface those in the
/// "coverage gap" list rather than averaging zero-confidence into the
/// composite (which would dilute toward "looks healthy" on a token where
/// half the analysis was actually skipped).
struct DetectorVerdict {
    /// Stable detector ID, e.g. `"d03_holder_concentration"`.
    id: &'static str,
    /// Whether the detector produced a real verdict on real data.
    fired: bool,
    /// Calibrated probability (0.0 … 1.0) — only meaningful when `fired`.
    confidence: f64,
    /// Human-readable severity label.
    severity_label: &'static str,
    /// One-line reasoning shown in the driving-signals breakdown.
    rationale: String,
}

/// Print the final composite block: overall severity, driving signals,
/// coverage gap. Replaces the old 13-line raw dump.
fn print_composite(token: &str, chain_label: &str, verdicts: &[DetectorVerdict]) {
    let fired: Vec<&DetectorVerdict> = verdicts.iter().filter(|v| v.fired).collect();
    let unfired: Vec<&DetectorVerdict> = verdicts.iter().filter(|v| !v.fired).collect();

    println!("====================================================================");
    println!("  TOKEN VERDICT — {} on {}", token, chain_label);
    println!("====================================================================");

    if fired.is_empty() {
        println!("  composite: UNKNOWN — no detector produced an evaluable signal.");
        println!("  next step: pass --rpc <self-hosted-node> to bypass public-RPC throttling.");
        if !unfired.is_empty() {
            println!();
            println!("  detectors that did not fire:");
            for v in &unfired {
                println!("    - {} — {}", v.id, v.rationale);
            }
        }
        return;
    }

    // Composite math: weighted noisy-OR probability combining.
    //
    //   per-detector contribution p_i = clamp(weight_i × confidence_i, 0, 0.95)
    //   composite                     = 1 − Π_i (1 − p_i)
    //
    // Treats each detector as a roughly independent rug-risk indicator
    // and combines them as you would independent probabilities. Two
    // medium signals (each 0.5) stack to 0.75 — corroborating evidence
    // raises composite. A single 0.5 stays at 0.5. Maxes out asymptotically
    // toward 1.0 as more signals fire.
    //
    // The 0.95 per-detector cap keeps a single detector from saturating
    // the composite — at least two firing signals are needed to reach
    // CRITICAL (≥0.80). Memecoin trader benefit: fresh-launch alone
    // (D10 weight 0.5) reaches MEDIUM; fresh + active-owner stacks to
    // HIGH; fresh + active-owner + honeypot-revert stacks to CRITICAL.
    let signals: Vec<&&DetectorVerdict> =
        fired.iter().filter(|v| v.confidence > 0.0).collect();
    let composite: f64 = if signals.is_empty() {
        0.0
    } else {
        let inv_product = signals
            .iter()
            .map(|v| {
                let p = (detector_weight(v.id) * v.confidence).clamp(0.0, 0.95);
                1.0 - p
            })
            .product::<f64>();
        (1.0 - inv_product).clamp(0.0, 1.0)
    };

    let composite_label = if composite >= 0.80 {
        "CRITICAL"
    } else if composite >= 0.60 {
        "HIGH"
    } else if composite >= 0.40 {
        "MEDIUM"
    } else if composite >= 0.20 {
        "LOW"
    } else {
        "INFO / clean"
    };

    println!("  composite verdict: {composite_label}  (confidence {composite:.2})");
    println!(
        "  detectors fired:   {} of {} ({} skipped — see gap below)",
        fired.len(),
        verdicts.len(),
        unfired.len()
    );
    println!();

    // Top-3 driving signals (highest confidence first).
    let mut sorted = fired.clone();
    sorted.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    println!("  driving signals (highest first):");
    for v in sorted.iter().take(3) {
        println!(
            "    {:>8} {}  conf {:.2}  — {}",
            v.severity_label, v.id, v.confidence, v.rationale
        );
    }

    if !unfired.is_empty() {
        println!();
        println!("  coverage gap ({} detector(s) skipped):", unfired.len());
        for v in &unfired {
            println!("    - {} — {}", v.id, v.rationale);
        }
    }

    println!();
    if composite >= 0.60 {
        println!("  RECOMMENDATION: do not interact without manual review. Multiple high-confidence risk signals.");
    } else if composite >= 0.40 {
        println!("  RECOMMENDATION: investigate the driving signals above before engaging.");
    } else if !unfired.is_empty() && fired.len() < 4 {
        println!("  RECOMMENDATION: low-signal verdict but coverage is incomplete. Re-run against a self-hosted RPC for full ladder.");
    } else {
        println!("  RECOMMENDATION: no risk signals fired. Safe-to-engage from this analysis.");
    }
}

// ===========================================================================
// EVM (Ethereum / BSC) branch
// ===========================================================================

async fn run_evm(_args: &Args, token_addr: &str, rpc: &str, chain: ChainKind) -> anyhow::Result<()> {
    let mut verdicts: Vec<DetectorVerdict> = Vec::new();

    eprintln!("[chain-adapter] calling evm_token_metadata(...) — 6× eth_call + 1× eth_getCode");
    let meta: EvmTokenMeta = evm_token_metadata(rpc, token_addr)
        .await
        .context("chain-adapter::ethereum::evm_token_metadata failed")?;

    println!("== EVM token metadata via mg_onchain_chain_adapter::ethereum ==");
    println!("  address:        {}", meta.address);
    println!("  name:           {}", meta.name.as_deref().unwrap_or("<unset / not implemented>"));
    println!("  symbol:         {}", meta.symbol.as_deref().unwrap_or("<unset / not implemented>"));
    match meta.decimals {
        Some(d) => println!("  decimals:       {}", d),
        None => println!("  decimals:       <revert / not implemented>"),
    }
    match meta.total_supply_raw.as_deref() {
        Some(s) => {
            let pretty = match meta.decimals {
                Some(d) if d > 0 => format_decimal_with_decimals(s, d),
                _ => s.to_owned(),
            };
            println!("  totalSupply:    {} raw ({} ui)", s, pretty);
        }
        None => println!("  totalSupply:    <revert / not implemented>"),
    }
    println!("  bytecode size:  {} bytes ({})", meta.bytecode_len,
             if meta.bytecode_len == 0 { "EOA / not a contract!" } else { "contract present" });
    println!();

    if meta.bytecode_len == 0 {
        anyhow::bail!(
            "address {} has zero bytecode → not a deployed contract; nothing to score",
            meta.address
        );
    }

    // ------------------------------------------------------------------------
    // Pre-fetch recent Transfer flows ONCE — used by both D01 simulate-sell
    // (top-N receivers as extra sender candidates) and D03 (concentration
    // math). Saves a redundant eth_getLogs call later.
    // ------------------------------------------------------------------------
    eprintln!("[chain-adapter] calling fetch_recent_holder_flows(...) — eth_getLogs Transfer over last 2000 blocks");
    let flows_result = fetch_recent_holder_flows(rpc, &meta.address).await;
    let extra_senders: Vec<String> = match &flows_result {
        Ok(flows) => flows.net_flows.iter().take(5).map(|(a, _)| a.clone()).collect(),
        Err(_) => Vec::new(),
    };
    // Snapshot raw transfers for D05 wash-trade detection downstream.
    let transfers_for_d05: Vec<mg_onchain_chain_adapter::ethereum::TransferEdge> =
        match &flows_result {
            Ok(flows) => flows.transfers.clone(),
            Err(_) => Vec::new(),
        };

    // ------------------------------------------------------------------------
    // D02 — Ownable owner (rug-prep risk if not renounced)
    // ------------------------------------------------------------------------
    println!("== D02 rug_pull_lp_drain / mint-authority via Ownable owner() ==");
    let (d02_severity, d02_confidence, d02_rationale) = match meta.owner.as_deref() {
        Some(addr) if meta.owner_renounced => (
            "NONE",
            0.0_f64,
            format!("owner() = {addr} → ownership renounced (zero address) — no privileged caller"),
        ),
        Some(addr) => (
            "MEDIUM",
            0.40,
            format!("owner() = {addr} → active EOA/contract can call privileged functions; verify against deployer + DAO label store before clearing"),
        ),
        None => (
            "INFO",
            0.10,
            "owner() reverted → not Ownable; cannot fire D02 from this signal alone".to_owned(),
        ),
    };
    let d02_sev_obj = severity_from_confidence(d02_confidence);
    println!("  D02 verdict: {} (severity={:?}, confidence={:.2})", d02_severity, d02_sev_obj, d02_confidence);
    println!("    rationale: {}", d02_rationale);
    println!();
    let d02_label_static: &'static str = match d02_severity {
        "HIGH" => "HIGH",
        "MEDIUM" => "MEDIUM",
        "LOW" => "LOW",
        "NONE" => "INFO",
        _ => "INFO",
    };
    verdicts.push(DetectorVerdict {
        id: "d02_ownable_owner",
        fired: meta.owner.is_some(),
        confidence: d02_confidence,
        severity_label: d02_label_static,
        rationale: d02_rationale.clone(),
    });

    // D02-aux: recent-renounce probe. Only meaningful when the contract
    // currently shows `owner == 0x0` — we want to know whether it's been
    // 0x0 since deployment (legitimate trustless launch) or whether the
    // renounce happened recently (classic post-rug cleanup pattern).
    if meta.owner_renounced {
        eprintln!("[chain-adapter] calling probe_ownership_events(...) — eth_getLogs OwnershipTransferred over last 50000 blocks");
        match probe_ownership_events(rpc, &meta.address).await {
            Ok(probe) => {
                if probe.had_event && probe.recently_renounced {
                    let last_block = probe
                        .last_event_block
                        .map(|b| format!(" (block {b})"))
                        .unwrap_or_default();
                    println!("== D02-aux post-rug renounce probe ==");
                    println!("  RECENT renounceOwnership() observed{last_block} — contract was actively owned, then nullified");
                    let rationale = format!(
                        "RECENT-RENOUNCE: OwnershipTransferred(prev → 0x0) emitted in last 50000 blocks{last_block}. \
                         The contract was actively owned, then ownership was nullified. \
                         Distinct from a token launched trustless from day one. Common post-rug cleanup pattern."
                    );
                    println!("    rationale: {rationale}");
                    println!("  D02-aux verdict: HIGH (severity=High, confidence=0.65)");
                    println!();
                    verdicts.push(DetectorVerdict {
                        id: "d02_recent_renounce",
                        fired: true,
                        confidence: 0.65,
                        severity_label: "HIGH",
                        rationale,
                    });
                } else if probe.had_event {
                    // OwnershipTransferred fired but the last one was non-zero
                    // (most likely transfer to a different controller, not a
                    // renounce). Quietly move on.
                } else {
                    // No event in window: either token was always trustless
                    // (deployed without Ownable) or the renounce happened
                    // earlier than 50k blocks ago. Either way, NOT a fresh-
                    // post-rug pattern.
                }
            }
            Err(e) => {
                eprintln!("[chain-adapter] probe_ownership_events failed: {e}");
            }
        }
    }

    // ------------------------------------------------------------------------
    // D06 — mint-burn surface (mint() in bytecode + active owner = high)
    // ------------------------------------------------------------------------
    println!("== D06 mint_burn_anomaly via bytecode selector grep ==");
    println!("  has mint(uint256) | mint(address,uint256) | issue(uint256): {}", meta.has_mint_selector);
    let (d06_severity, d06_confidence, d06_rationale) = match (meta.has_mint_selector, meta.owner.as_deref(), meta.owner_renounced) {
        (true, Some(addr), false) => (
            "HIGH",
            0.70,
            format!("mint() exists in bytecode AND owner ({addr}) is active → privileged caller can mint more supply (rug-prep capability)"),
        ),
        (true, _, true) => (
            "LOW",
            0.20,
            "mint() in bytecode but owner renounced → mint path likely unreachable (verify no other access control bypass)".to_owned(),
        ),
        (true, None, _) => (
            "MEDIUM",
            0.50,
            "mint() in bytecode and contract not Ownable → access control via something other than OpenZeppelin Ownable; needs manual review".to_owned(),
        ),
        (false, _, _) => (
            "NONE",
            0.0,
            "no mint() selector in bytecode → fixed supply (modulo non-standard mint paths in proxy/diamond patterns)".to_owned(),
        ),
    };
    let d06_sev_obj = severity_from_confidence(d06_confidence);
    println!("  D06 verdict: {} (severity={:?}, confidence={:.2})", d06_severity, d06_sev_obj, d06_confidence);
    println!("    rationale: {}", d06_rationale);
    println!();
    let d06_label_static: &'static str = match d06_severity {
        "HIGH" => "HIGH",
        "MEDIUM" => "MEDIUM",
        "LOW" => "LOW",
        "NONE" => "INFO",
        _ => "INFO",
    };
    verdicts.push(DetectorVerdict {
        id: "d06_mint_burn",
        fired: true,
        confidence: d06_confidence,
        severity_label: d06_label_static,
        rationale: d06_rationale.clone(),
    });

    // ------------------------------------------------------------------------
    // D01 honeypot — static bytecode patterns + live simulate-sell probe
    //
    // Static path: grep deployed bytecode for selectors that gate transfers /
    // sells (paused, blacklist, swap-toggle). Each selector contributes a
    // small weight to raw.
    //
    // Dynamic path (added T27-14): `eth_call(from=owner, to=token,
    // data=transfer(0xdEaD, 1))` — if the simulator reverts with a
    // non-balance reason we've catalogued as honeypot-y, add 0.45 to raw.
    // Same sigmoid normalisation as the Solana D01 path.
    // ------------------------------------------------------------------------
    println!("== D01 honeypot (static bytecode patterns + simulate-sell probe) ==");
    println!("  paused() returns:               {}", match meta.paused {
        Some(true)  => "true ◄ TRANSFERS HALTED RIGHT NOW".to_owned(),
        Some(false) => "false (Pausable but not currently paused)".to_owned(),
        None        => "<revert / not Pausable>".to_owned(),
    });
    println!("  pause() in bytecode:            {}", meta.has_pause_selector);
    println!("  blacklist(address,bool):        {}", meta.has_blacklist_selector);
    println!("  setSwapEnabled(bool):           {}", meta.has_swap_toggle_selector);

    let mut raw_d01 = 0.0_f64;
    let mut d01_signals: Vec<String> = Vec::new();
    if matches!(meta.paused, Some(true)) {
        raw_d01 += 0.45; // currently paused = strong honeypot indicator right now
        d01_signals.push("paused()=true".to_owned());
    } else if meta.has_pause_selector {
        raw_d01 += 0.10;
        d01_signals.push("pause() selector".to_owned());
    }
    if meta.has_blacklist_selector {
        raw_d01 += 0.20;
        d01_signals.push("blacklist(address,bool) selector".to_owned());
    }
    if meta.has_swap_toggle_selector {
        raw_d01 += 0.25;
        d01_signals.push("setSwapEnabled(bool) selector".to_owned());
    }

    // -- D01 dynamic signal: simulate-sell --------------------------------
    eprintln!("[chain-adapter] calling simulate_sell_evm(...) — eth_call from owner + top-5 recent receivers");
    match simulate_sell_evm(rpc, &meta.address, meta.owner.as_deref(), &extra_senders).await {
        Ok(SimulateSellOutcome::Success) => {
            println!("  simulate-sell:                  ✓ transferable (sender→0xdEaD probe succeeded)");
        }
        Ok(SimulateSellOutcome::Reverted { reason }) => {
            println!("  simulate-sell:                  ✗ REVERTED — reason: {reason}");
            raw_d01 += 0.45;
            d01_signals.push(format!("simulate-sell reverted: {reason}"));
        }
        Ok(SimulateSellOutcome::Skipped { reason }) => {
            println!("  simulate-sell:                  skipped — {reason}");
        }
        Err(e) => {
            println!("  simulate-sell:                  RPC error: {e}");
        }
    }

    let d01_conf = sigmoid(raw_d01 / 0.55 - 1.0);
    let d01_sev_obj = severity_from_confidence(d01_conf);
    println!("  raw signal sum:                 {:.3}", raw_d01);
    println!("  static_conf (sigmoid):          {:.3}", d01_conf);
    println!("  D01 verdict: severity={:?} confidence={:.3}", d01_sev_obj, d01_conf);
    if d01_signals.is_empty() {
        println!("    rationale: no honeypot static patterns in bytecode (no pause / blacklist / swap-toggle selectors found)");
    } else {
        println!("    rationale: bytecode contains: {}", d01_signals.join(", "));
    }
    println!("    NOTE: combines static bytecode-pattern grep with a live simulate-sell probe; rugs that revert on transfer with a non-balance reason are caught.");
    println!();
    let d01_label_static: &'static str = if raw_d01 == 0.0 {
        "INFO"
    } else if d01_conf >= 0.60 {
        "HIGH"
    } else if d01_conf >= 0.40 {
        "MEDIUM"
    } else {
        "LOW"
    };
    let d01_rationale_owned = if d01_signals.is_empty() {
        "no honeypot static patterns in bytecode".to_owned()
    } else {
        format!("bytecode contains: {}", d01_signals.join(", "))
    };
    verdicts.push(DetectorVerdict {
        id: "d01_honeypot_static",
        fired: true,
        confidence: if raw_d01 == 0.0 { 0.0 } else { d01_conf },
        severity_label: d01_label_static,
        rationale: d01_rationale_owned,
    });

    // ------------------------------------------------------------------------
    // D10 — token age via binary-search eth_getCode over historical blocks.
    // Same severity bands as the Solana branch:
    //   <7d   → YOUNG     (fresh-launch fires HIGH/CRITICAL)
    //   7-30d → RECENT    (LOW)
    //   >30d  → MATURE    (clean)
    // ------------------------------------------------------------------------
    eprintln!("[chain-adapter] calling find_contract_age(...) — binary-search eth_getCode (~24 RPC calls)");
    println!("== D10 launch_audit via binary-search eth_getCode ==");
    let mut contract_age_days: Option<i64> = None;
    match find_contract_age(rpc, &meta.address).await {
        Ok(Some(age)) if age.archive_limited => {
            // The RPC's archive doesn't reach block 1, so `creation_block`
            // is an upper bound — real deployment may be much earlier. We
            // can only say "token is at LEAST this old"; firing a YOUNG
            // signal here would false-positive on every well-aged token
            // queried against a non-archive RPC (BSC publicnode, most
            // L2s).
            let oldest: DateTime<Utc> =
                Utc.timestamp_opt(age.creation_ts, 0).single().unwrap_or_else(Utc::now);
            let age_days = age.age_secs / 86_400;
            println!("  RPC archive state is pruned — cannot read block 1.");
            println!("  earliest observed block w/ contract code: {}", age.creation_block);
            println!("  earliest observed time:                   {} (UNIX={})",
                     oldest.to_rfc3339(), age.creation_ts);
            println!("  age LOWER BOUND:                          {} days", age_days);
            println!("  D10 verdict: UNKNOWN (archive cutoff, age ≥ {} days only)", age_days);
            println!("    rationale: RPC pruned archive state below the cutoff; D10 cannot fire YOUNG \
                      because real deployment may be much earlier than what we can observe");
            verdicts.push(DetectorVerdict {
                id: "d10_launch_audit",
                fired: false,
                confidence: 0.0,
                severity_label: "INFO",
                rationale: format!(
                    "RPC archive pruned; observed age ≥ {age_days} days but real deployment may be earlier — pass --rpc <archive-node> for a definitive answer"
                ),
            });
            contract_age_days = None;
            println!();
        }
        Ok(Some(age)) => {
            let oldest: DateTime<Utc> =
                Utc.timestamp_opt(age.creation_ts, 0).single().unwrap_or_else(Utc::now);
            let age_days = age.age_secs / 86_400;
            contract_age_days = Some(age_days);
            println!("  creation block:   {}", age.creation_block);
            println!("  creation time:    {} (UNIX={})", oldest.to_rfc3339(), age.creation_ts);
            println!("  age:              {} days", age_days);
            let (sev, conf, rationale) = if age_days < 7 {
                let confidence = (1.0 - (age_days as f64 / 7.0)).clamp(0.0, 1.0);
                ("YOUNG (<7d)", confidence,
                 format!("contract age {age_days} days < 7d threshold → D10 fresh-launch signal fires"))
            } else if age_days < 30 {
                ("RECENT (7-30d)", 0.30,
                 format!("contract age {age_days} days — recent but past fresh-launch window"))
            } else {
                ("MATURE (>30d)", 0.0,
                 format!("contract age {age_days} days — established, no fresh-launch signal"))
            };
            let severity_obj = severity_from_confidence(conf);
            println!("  D10 verdict: {sev} (severity={severity_obj:?}, confidence={conf:.2})");
            println!("    rationale: {rationale}");
            let d10_label_static: &'static str = if conf >= 0.60 {
                "HIGH"
            } else if conf >= 0.40 {
                "MEDIUM"
            } else if conf >= 0.20 {
                "LOW"
            } else {
                "INFO"
            };
            verdicts.push(DetectorVerdict {
                id: "d10_launch_audit",
                fired: true,
                confidence: conf,
                severity_label: d10_label_static,
                rationale: rationale.clone(),
            });
        }
        Ok(None) => {
            println!("  D10 verdict: UNKNOWN — could not resolve creation block (RPC may not be archive-state-enabled)");
            verdicts.push(DetectorVerdict {
                id: "d10_launch_audit",
                fired: false,
                confidence: 0.0,
                severity_label: "INFO",
                rationale: "could not resolve creation block — pass --rpc <archive-node>".to_owned(),
            });
        }
        Err(e) => {
            println!("  D10 verdict: UNKNOWN — RPC error: {e}");
            verdicts.push(DetectorVerdict {
                id: "d10_launch_audit",
                fired: false,
                confidence: 0.0,
                severity_label: "INFO",
                rationale: format!("RPC error during binary-search: {e}"),
            });
        }
    }
    println!();

    // ------------------------------------------------------------------------
    // D04 — swap-volume probe via Uniswap V2/V3 (or PancakeSwap V2 on BSC)
    // pool log scan. Real D04 not proxy: counts actual `Swap` events from
    // the canonical DEX pool against WETH (or WBNB on BSC), buckets by
    // block-number into "last hour" vs "trailing 24h" sub-windows, computes
    // spike ratio.
    // ------------------------------------------------------------------------
    eprintln!("[chain-adapter] calling probe_swap_volume(...) — getPair/getPool + eth_getLogs over 7200 blocks");
    println!("== D04 pump_dump via DEX swap-event scan ==");
    let chain_id_for_d04: u64 = match chain {
        ChainKind::Bsc => BSC_CHAIN_ID,
        ChainKind::Base => BASE_CHAIN_ID,
        ChainKind::Arbitrum => ARBITRUM_CHAIN_ID,
        ChainKind::Optimism => OPTIMISM_CHAIN_ID,
        ChainKind::Polygon => POLYGON_CHAIN_ID,
        ChainKind::Avalanche => AVALANCHE_CHAIN_ID,
        _ => ETHEREUM_CHAIN_ID,
    };
    match probe_swap_volume(rpc, &meta.address, chain_id_for_d04).await {
        Ok(Some(probe)) => {
            println!("  pool resolved:    {} ({})", probe.pool, probe.source);
            println!("  scan window:      blocks {}..{} ({} blocks)",
                     probe.from_block, probe.to_block, probe.to_block - probe.from_block);
            println!("  total swaps:      {}", probe.total_swaps);
            println!("  recent (last 300 blocks ≈ 1h):  {}", probe.recent_swaps);
            // Min-trailing guard: with fewer than 20 swaps in the trailing
            // window, the per-block baseline is too noisy to derive a
            // meaningful spike ratio — fresh pools with 1-3 sniper trades
            // would otherwise false-fire HIGH on a single follow-on buy.
            const MIN_TRAILING_SWAPS: usize = 20;
            let trailing_swaps = probe.total_swaps.saturating_sub(probe.recent_swaps);
            let (sev, conf, rationale) = if trailing_swaps < MIN_TRAILING_SWAPS {
                (
                    "UNKNOWN",
                    0.0,
                    format!(
                        "only {trailing_swaps} swaps in trailing window — insufficient baseline (need ≥{MIN_TRAILING_SWAPS}); typical for fresh pools, retry once volume builds"
                    ),
                )
            } else { match probe.spike_ratio {
                Some(r) if !r.is_finite() => (
                    "UNKNOWN",
                    0.0,
                    "trailing baseline = 0 events; cannot compute ratio".to_owned(),
                ),
                Some(r) if r >= 5.0 => (
                    "HIGH (potential pump)",
                    0.70,
                    format!("spike ratio {r:.2}× — last-hour swap rate ≥ 5× trailing baseline"),
                ),
                Some(r) if r >= 2.0 => (
                    "MEDIUM (elevated)",
                    0.50,
                    format!("spike ratio {r:.2}× — elevated above trailing baseline"),
                ),
                Some(r) if r >= 0.3 => (
                    "NORMAL",
                    0.10,
                    format!("spike ratio {r:.2}× — within normal variance"),
                ),
                Some(r) => (
                    "LOW (cooled off)",
                    0.0,
                    format!("spike ratio {r:.2}× — last-hour swap rate below typical"),
                ),
                None => (
                    "UNKNOWN",
                    0.0,
                    "no swaps in the recent window AND no swaps in trailing window".to_owned(),
                ),
            } };
            let severity_obj = severity_from_confidence(conf);
            println!("  D04 verdict: {sev} (severity={severity_obj:?}, confidence={conf:.2})");
            println!("    rationale: {rationale}");
            let d04_label_static: &'static str = if conf >= 0.60 {
                "HIGH"
            } else if conf >= 0.40 {
                "MEDIUM"
            } else if conf >= 0.20 {
                "LOW"
            } else {
                "INFO"
            };
            verdicts.push(DetectorVerdict {
                id: "d04_pump_dump",
                fired: sev != "UNKNOWN",
                confidence: conf,
                severity_label: d04_label_static,
                rationale: rationale.clone(),
            });
        }
        Ok(None) => {
            println!("  D04 verdict: SKIPPED — no DEX pool resolved against WETH/WBNB on Uniswap V2/V3 / PancakeSwap V2");
            verdicts.push(DetectorVerdict {
                id: "d04_pump_dump",
                fired: false,
                confidence: 0.0,
                severity_label: "INFO",
                rationale: "no DEX pool resolved against WETH/WBNB on Uniswap V2/V3 / PancakeSwap V2".to_owned(),
            });
        }
        Err(e) => {
            println!("  D04 verdict: UNKNOWN — RPC error: {e}");
            verdicts.push(DetectorVerdict {
                id: "d04_pump_dump",
                fired: false,
                confidence: 0.0,
                severity_label: "INFO",
                rationale: format!("RPC error during swap-log scan: {e}"),
            });
        }
    }
    println!();

    // ------------------------------------------------------------------------
    // D03 — recent-window holder concentration via Transfer log replay
    // (reuses the already-fetched `flows_result` from earlier in run_evm)
    // ------------------------------------------------------------------------
    println!("== D03 holder_concentration via Transfer-log net-flow analysis ==");
    match flows_result {
        Ok(flows) => {
            println!("  scan window:       blocks {}..{} (10000 blocks)", flows.window.0, flows.window.1);
            println!("  transfer events:   {}{}", flows.transfer_count,
                     if flows.truncated { "  ◄ TRUNCATED at RPC cap" } else { "" });
            println!("  net-positive accumulators: {}", flows.net_flows.len());

            if flows.transfer_count < 10 {
                // Near-dormant path: fewer than 10 Transfer events in the
                // last 2000 blocks is structurally suspicious for any
                // deployed contract. Three typical causes:
                //   (a) abandoned post-rug (SQUID Game pattern, 2 events)
                //   (b) launched-but-no-buyers (rare for fresh launches —
                //       LP-add and first swap usually push count past 10)
                //   (c) paused / sell-blocked at the contract level
                //
                // Combine with age (from D10) when available to sharpen
                // severity; when archive-pruned RPCs hide age we still
                // fire MEDIUM so the composite reflects the structural
                // concern.
                println!("  near-dormant pattern: only {} Transfer events in last 2000 blocks", flows.transfer_count);
                let n_events = flows.transfer_count;
                let (label, conf, rationale) = match contract_age_days {
                    Some(d) if d > 30 => (
                        "HIGH",
                        0.75,
                        format!(
                            "DORMANT — token age {d} days but only {n_events} Transfer events in last 2000 blocks. \
                             A live deployment that went silent is the classic post-rug / abandoned-scam pattern."
                        ),
                    ),
                    Some(d) if d > 7 => (
                        "MEDIUM",
                        0.55,
                        format!(
                            "DORMANT — token age {d} days, only {n_events} Transfer events in window. \
                             Either abandoned, paused, or never gained traction. Investigate deployer history + liquidity."
                        ),
                    ),
                    Some(d) => (
                        "LOW",
                        0.25,
                        format!(
                            "{n_events} Transfer events in window; token only {d} days old (could be fresh launch with no buyers yet)"
                        ),
                    ),
                    None => (
                        "MEDIUM",
                        0.50,
                        format!(
                            "DORMANT — contract is deployed but only {n_events} Transfer events in last 2000 blocks. \
                             Age unknown (RPC archive pruned). A deployed contract with no recent activity \
                             is structurally suspicious regardless of age — typical of post-rug abandons."
                        ),
                    ),
                };
                let severity_obj = severity_from_confidence(conf);
                println!(
                    "  D03 verdict: {label} (severity={severity_obj:?}, confidence={conf:.2}) — DORMANT-TOKEN PATH"
                );
                println!("    rationale: {rationale}");
                // Use a dedicated detector_id so the composite weighting
                // can distinguish 'dormant abandoned contract' (a strong
                // structural rug indicator, weight 0.9) from 'distribution
                // concentration' (weighting math, weight 0.7).
                verdicts.push(DetectorVerdict {
                    id: "d03_dormant_token",
                    fired: !matches!(label, "INFO"),
                    confidence: conf,
                    severity_label: label,
                    rationale,
                });
            } else if flows.transfer_count < 30 || flows.net_flows.len() < 5 {
                // Small-sample path: not enough independent observations
                // for the gini math to be stable, but ALSO not "zero
                // events" so the dormant path doesn't apply. Report as
                // LOW-confidence informational.
                println!(
                    "  D03 verdict: SKIPPED-SMALL-SAMPLE — {} Transfer events / {} accumulators (need ≥30 events + ≥10 accumulators)",
                    flows.transfer_count, flows.net_flows.len()
                );
                verdicts.push(DetectorVerdict {
                    id: "d03_holder_concentration",
                    fired: false,
                    confidence: 0.0,
                    severity_label: "INFO",
                    rationale: format!(
                        "small sample ({} Transfer events / {} accumulators); concentration math suppressed",
                        flows.transfer_count, flows.net_flows.len()
                    ),
                });
            } else if flows.net_flows.is_empty() {
                println!("  D03 verdict: SKIPPED — no addresses had positive net flow in window (all churn)");
                verdicts.push(DetectorVerdict {
                    id: "d03_holder_concentration",
                    fired: false,
                    confidence: 0.0,
                    severity_label: "INFO",
                    rationale: "no positive-net accumulators in window".to_owned(),
                });
            } else {
                // ENTITY-LABEL SUPPRESSION: classify each top-20 recipient
                // (DEX pool / known CEX hot wallet / unknown wallet). DEX
                // pools and CEX hot wallets are STRUCTURALLY guaranteed to
                // dominate net flow — every swap routes through them — but
                // they don't represent real holders concentrating supply.
                // Removing them from the gini math prevents PEPE / LINK /
                // every actively-traded token from false-firing HIGH on
                // "concentration" that's actually just market structure.
                eprintln!("[d03] classifying top-20 net-flow recipients (DEX pool / CEX vault / unknown)");
                let mut classified: Vec<(String, u128, AddressClass)> = Vec::new();
                let mut suppressed_addresses: Vec<(String, AddressClass)> = Vec::new();
                for (addr, amt) in flows.net_flows.iter().take(20) {
                    let class = classify_address(rpc, addr).await;
                    if class != AddressClass::Unknown {
                        suppressed_addresses.push((addr.clone(), class.clone()));
                    }
                    classified.push((addr.clone(), *amt, class));
                }
                // Append remaining (deeper than top-20) entries as Unknown
                // — we don't pay for classification calls past the cut-off.
                for (addr, amt) in flows.net_flows.iter().skip(20) {
                    classified.push((addr.clone(), *amt, AddressClass::Unknown));
                }
                // Filter to only Unknown addresses for concentration math.
                let real_holders: Vec<(String, u128)> = classified
                    .iter()
                    .filter(|(_, _, c)| *c == AddressClass::Unknown)
                    .map(|(a, n, _)| (a.clone(), *n))
                    .collect();

                if !suppressed_addresses.is_empty() {
                    println!(
                        "  entity-label suppression: {} of top-20 are infrastructure (DEX pools / CEX vaults), excluded from gini math",
                        suppressed_addresses.len()
                    );
                    for (addr, class) in &suppressed_addresses {
                        println!("    - {addr}  →  {class:?}");
                    }
                }

                if real_holders.len() < 10 {
                    println!(
                        "  D03 verdict: SKIPPED-AFTER-SUPPRESSION — only {} real holders left after removing {} infrastructure addresses (need ≥10)",
                        real_holders.len(),
                        suppressed_addresses.len()
                    );
                    verdicts.push(DetectorVerdict {
                        id: "d03_holder_concentration",
                        fired: false,
                        confidence: 0.0,
                        severity_label: "INFO",
                        rationale: format!(
                            "after entity-label suppression of {} infrastructure addresses, only {} real holders left — insufficient sample",
                            suppressed_addresses.len(),
                            real_holders.len()
                        ),
                    });
                    println!();
                } else {
                let max_amt = real_holders.iter().map(|(_, a)| *a).max().unwrap_or(0);
                let target_cap: u128 = 10u128.pow(22); // safely below Decimal::MAX even after summing thousands of entries
                let scale_shift: u32 = if max_amt > target_cap {
                    let mut s = 0u32;
                    let mut probe = max_amt;
                    while probe > target_cap {
                        probe /= 10;
                        s += 1;
                    }
                    s
                } else {
                    0
                };
                let scale = 10u128.pow(scale_shift);
                if scale_shift > 0 {
                    eprintln!("[d03] scaling raw flows by 10^{scale_shift} to fit Decimal range (max raw {max_amt})");
                }
                let balances_desc: Vec<Decimal> = real_holders
                    .iter()
                    .map(|(_, amt)| Decimal::from(*amt / scale))
                    .collect();
                let gini = gini_descending(&balances_desc);
                let top10_pct = top_n_pct(&balances_desc, 10);
                println!("  gini_descending (over {} real holders, post-suppression): {}", real_holders.len(), gini);
                println!("  top_n_pct(10) (whale share of net flow): {} ({}%)",
                         top10_pct,
                         top10_pct * Decimal::new(100, 0));
                println!("  top-10 real holders (recent net buyers, infrastructure suppressed):");
                for (i, (addr, amt)) in real_holders.iter().take(10).enumerate() {
                    println!("    {:2}. {} | net +{} raw", i + 1, addr, amt);
                }

                // Recalibrated thresholds (T27-26 follow-up). After we
                // suppress DEX pools / contracts from the math, the
                // remaining EOA top-10 still tends to dominate net flow on
                // any actively traded token (PEPE, LINK have ~90 % top-10
                // share among real wallets). HIGH should be reserved for
                // near-monopoly concentration (~95 %+) consistent with
                // single-whale-controls patterns. 75-95 % = MEDIUM
                // "whale-dominated activity, trade with caution".
                let high_threshold = Decimal::new(95, 2);
                let medium_threshold = Decimal::new(75, 2);
                let low_threshold = Decimal::new(50, 2);
                let (label, conf, rationale) = if top10_pct >= high_threshold {
                    (
                        "HIGH",
                        0.85,
                        format!("top-10 EOA net-flow share {top10_pct} ≥ 0.95 — near-monopoly whale concentration in last 36h"),
                    )
                } else if top10_pct >= medium_threshold {
                    (
                        "MEDIUM",
                        0.55,
                        format!("top-10 EOA net-flow share {top10_pct} between 0.75-0.95 — whale-dominated recent activity"),
                    )
                } else if top10_pct >= low_threshold {
                    (
                        "LOW",
                        0.30,
                        format!("top-10 EOA net-flow share {top10_pct} between 0.50-0.75 — moderate concentration"),
                    )
                } else {
                    (
                        "INFO",
                        0.10,
                        format!("top-10 EOA net-flow share {top10_pct} < 0.50 — distributed recent activity"),
                    )
                };
                let severity_obj = severity_from_confidence(conf);
                println!("  D03 verdict: {label} (severity={severity_obj:?}, confidence={conf:.2})");
                println!("    rationale: {rationale}");
                println!("    NOTE: this is RECENT-WINDOW net concentration (last ~36h), not total balanceOf-weighted distribution. For new tokens these converge; for established tokens it captures whale accumulation/distribution.");
                if flows.truncated {
                    println!("    WARNING: log set was truncated at the RPC cap; concentration is biased toward the tail of the window.");
                }
                verdicts.push(DetectorVerdict {
                    id: "d03_holder_concentration",
                    fired: true,
                    confidence: conf,
                    severity_label: label,
                    rationale: rationale.clone(),
                });
                } // close `else` opened above the `let max_amt = ...` block
            }
        }
        Err(e) => {
            println!("  D03 verdict: UNKNOWN — RPC error: {e}");
            verdicts.push(DetectorVerdict {
                id: "d03_holder_concentration",
                fired: false,
                confidence: 0.0,
                severity_label: "INFO",
                rationale: format!("RPC error during Transfer-log scan: {e}"),
            });
        }
    }
    println!();

    // ------------------------------------------------------------------------
    // D05 — wash trading via ping-pong detection in the Transfer-log graph.
    // For each ordered pair (X, Y), count edges X→Y. Tight ping-pong is
    // when both X→Y and Y→X have ≥3 events — that's a clear "circular
    // volume" pattern. The number of distinct ping-pong pairs scales the
    // severity. Doesn't require a full SCC algorithm — simple pair-count
    // catches the most common wash setup.
    // ------------------------------------------------------------------------
    println!("== D05 wash_trading via Transfer-log ping-pong detection ==");
    if transfers_for_d05.len() < 30 {
        println!(
            "  D05 verdict: SKIPPED — only {} Transfer events; insufficient sample for wash-pattern detection",
            transfers_for_d05.len()
        );
        verdicts.push(DetectorVerdict {
            id: "d05_wash_trading",
            fired: false,
            confidence: 0.0,
            severity_label: "INFO",
            rationale: format!(
                "small sample ({} events); cannot reliably detect ping-pong patterns",
                transfers_for_d05.len()
            ),
        });
    } else {
        // Pair-count map: (from, to) → number of events. Skip self-loops
        // and skip mints / burns where either side is the zero address.
        let mut pair_count: std::collections::HashMap<(String, String), usize> =
            std::collections::HashMap::new();
        for t in &transfers_for_d05 {
            if t.from == "0x0000000000000000000000000000000000000000"
                || t.to == "0x0000000000000000000000000000000000000000"
            {
                continue;
            }
            *pair_count.entry((t.from.clone(), t.to.clone())).or_insert(0) += 1;
        }

        // Find ping-pong pairs (both directions ≥3, counted once per pair).
        let mut ping_pong: Vec<(String, String, usize, usize)> = Vec::new();
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        for ((from, to), forward_count) in &pair_count {
            if *forward_count < 3 {
                continue;
            }
            // Canonical ordering so we only count A↔B once, not twice.
            let canon = if from < to {
                (from.clone(), to.clone())
            } else {
                (to.clone(), from.clone())
            };
            if seen.contains(&canon) {
                continue;
            }
            let reverse_count = pair_count
                .get(&(to.clone(), from.clone()))
                .copied()
                .unwrap_or(0);
            if reverse_count >= 3 {
                seen.insert(canon.clone());
                ping_pong.push((from.clone(), to.clone(), *forward_count, reverse_count));
            }
        }

        let num_pairs = ping_pong.len();
        // Normalise by total Transfer events. On high-volume tokens
        // (USDC, WETH) thousands of "ping-pong" pairs appear naturally as
        // arbitrage between MEV bots and DEX pools — that's market
        // structure, not wash trading. Real wash on small memecoins
        // produces a high ratio of pairs to total events.
        let total_events = transfers_for_d05.len() as f64;
        let pair_ratio = num_pairs as f64 / total_events;
        println!(
            "  ping-pong pairs detected: {num_pairs} (ratio {:.2}% of {} total transfers)",
            pair_ratio * 100.0,
            total_events as usize
        );
        if num_pairs > 0 {
            ping_pong.sort_by(|a, b| (b.2 + b.3).cmp(&(a.2 + a.3)));
            for (from, to, fwd, rev) in ping_pong.iter().take(5) {
                println!("    {from} ↔ {to}  ({fwd} forward, {rev} back)");
            }
        }
        let (label, conf, rationale): (&str, f64, String) = if pair_ratio >= 0.07 {
            (
                "HIGH",
                0.70,
                format!(
                    "ping-pong pair ratio {:.2}% (≥7% threshold) — strong wash-trade pattern across {num_pairs} pairs",
                    pair_ratio * 100.0
                ),
            )
        } else if pair_ratio >= 0.03 {
            (
                "MEDIUM",
                0.55,
                format!(
                    "ping-pong pair ratio {:.2}% (3-7% range) — moderate wash-trade indication ({num_pairs} pairs)",
                    pair_ratio * 100.0
                ),
            )
        } else if pair_ratio >= 0.01 {
            (
                "LOW",
                0.30,
                format!(
                    "ping-pong pair ratio {:.2}% (1-3%) — minor wash signal but plausibly market structure",
                    pair_ratio * 100.0
                ),
            )
        } else if num_pairs > 0 {
            (
                "INFO",
                0.05,
                format!(
                    "{num_pairs} ping-pong pair(s) but only {:.2}% of transfers — typical for active market making",
                    pair_ratio * 100.0
                ),
            )
        } else {
            (
                "INFO",
                0.0,
                "no ping-pong patterns in the Transfer graph".to_owned(),
            )
        };
        let severity_obj = severity_from_confidence(conf);
        println!("  D05 verdict: {label} (severity={severity_obj:?}, confidence={conf:.2})");
        println!("    rationale: {rationale}");
        verdicts.push(DetectorVerdict {
            id: "d05_wash_trading",
            fired: num_pairs >= 1,
            confidence: conf,
            severity_label: label,
            rationale: rationale.clone(),
        });
    }
    println!();

    // ------------------------------------------------------------------------
    // D11 — synchronized activity via per-block Transfer-burst rate.
    // Bucket every Transfer by its block number, find the peak count of
    // transfers in a single block. Coordinated bot pumps / sniper waves
    // all swap into the same block to be in front of retail; that
    // produces unusually-high single-block bursts.
    // ------------------------------------------------------------------------
    println!("== D11 synchronized_activity via per-block burst rate ==");
    if transfers_for_d05.len() < 30 {
        println!(
            "  D11 verdict: SKIPPED — only {} Transfer events; insufficient sample for burst detection",
            transfers_for_d05.len()
        );
        verdicts.push(DetectorVerdict {
            id: "d11_synchronized",
            fired: false,
            confidence: 0.0,
            severity_label: "INFO",
            rationale: format!(
                "small sample ({} events); cannot detect synchronised bursts",
                transfers_for_d05.len()
            ),
        });
    } else {
        let mut per_block: std::collections::BTreeMap<u64, u64> =
            std::collections::BTreeMap::new();
        for t in &transfers_for_d05 {
            *per_block.entry(t.block_number).or_insert(0) += 1;
        }
        let total_events = transfers_for_d05.len();
        let blocks_observed = per_block.len();
        let mean_per_block = total_events as f64 / blocks_observed.max(1) as f64;
        let peak_block = per_block
            .iter()
            .max_by_key(|(_, c)| *c)
            .map(|(b, c)| (*b, *c))
            .unwrap_or((0, 0));
        let burst_ratio = if mean_per_block > 0.0 {
            peak_block.1 as f64 / mean_per_block
        } else {
            0.0
        };
        println!(
            "  blocks observed:    {} (avg {:.1} transfers/block)",
            blocks_observed, mean_per_block
        );
        println!(
            "  peak burst:         {} transfers in block {} ({:.1}× avg)",
            peak_block.1, peak_block.0, burst_ratio
        );

        // Burst-ratio thresholds. Normal market activity peaks 3-5× the
        // mean (a busy block). Coordinated pump waves push 10×+. The
        // raw peak count also matters — 100 transfers in a block is a
        // wave regardless of mean.
        // Use ONLY the ratio (peak / mean), not raw count. High-volume
        // tokens (USDT/USDC) have hundreds of transfers per block as
        // baseline — raw thresholds false-fire on them. The ratio
        // captures "this single block is unusually busy" regardless of
        // baseline.
        let (label, conf, rationale): (&str, f64, String) = if burst_ratio >= 15.0 {
            (
                "HIGH",
                0.65,
                format!(
                    "peak block had {} transfers ({:.1}× avg) — strong synchronised burst",
                    peak_block.1, burst_ratio
                ),
            )
        } else if burst_ratio >= 8.0 {
            (
                "MEDIUM",
                0.50,
                format!(
                    "peak block had {} transfers ({:.1}× avg) — elevated synchronised activity",
                    peak_block.1, burst_ratio
                ),
            )
        } else if burst_ratio >= 4.0 {
            (
                "LOW",
                0.25,
                format!(
                    "peak block had {} transfers ({:.1}× avg) — minor burst within normal trading variance",
                    peak_block.1, burst_ratio
                ),
            )
        } else {
            (
                "INFO",
                0.05,
                format!(
                    "peak block had {} transfers ({:.1}× avg) — distributed activity, no burst pattern",
                    peak_block.1, burst_ratio
                ),
            )
        };
        let severity_obj = severity_from_confidence(conf);
        println!("  D11 verdict: {label} (severity={severity_obj:?}, confidence={conf:.2})");
        println!("    rationale: {rationale}");
        verdicts.push(DetectorVerdict {
            id: "d11_synchronized",
            fired: matches!(label, "HIGH" | "MEDIUM" | "LOW"),
            confidence: conf,
            severity_label: label,
            rationale: rationale.clone(),
        });
    }
    println!();

    // ------------------------------------------------------------------------
    // D09 — deployer pattern via transaction-count nonce.
    // Find the FIRST mint event in the captured Transfer log (smallest
    // block_number with from = 0x0). The recipient is the deployer / first
    // holder. Their tx-count (eth_getTransactionCount, latest) is a strong
    // indicator: bot-deployers (Banana Gun, Maestro, Photon, etc.) send
    // thousands of outgoing tx because they relay user-launched memecoins
    // through the same hot wallet. nonce > 1000 + fresh token = serial-bot
    // pattern, often associated with rug-prep automation.
    // ------------------------------------------------------------------------
    println!("== D09 deployer_pattern via transaction-count nonce ==");
    let mint_event = transfers_for_d05
        .iter()
        .filter(|t| t.from == "0x0000000000000000000000000000000000000000")
        .min_by_key(|t| t.block_number)
        .cloned();
    if let Some(mint) = mint_event {
        let deployer = mint.to.clone();
        eprintln!(
            "[chain-adapter] calling eth_get_transaction_count(...) — deployer {} nonce probe",
            deployer
        );
        match eth_get_transaction_count(rpc, &deployer).await {
            Ok(nonce) => {
                let young = matches!(contract_age_days, Some(d) if d < 30);
                println!("  first mint observed:    block {} → recipient (deployer/first-holder) {}", mint.block_number, deployer);
                println!("  deployer tx count:      {} (nonce)", nonce);
                println!("  fresh token (<30d):     {young}");
                // Both extremes are signal:
                //  - Very low nonce (≤3) on a fresh token = single-use
                //    throwaway wallet, the modern rug-bot pattern (Banana
                //    Gun creates fresh EOA per launch to avoid linking).
                //  - Very high nonce (>5000) = old-school serial-bot
                //    operator who reuses the same wallet (Photon / older
                //    Maestro / etc).
                //  - Middle (50-1000) = likely real human deployer.
                let (label, conf, rationale): (&str, f64, String) = if young && nonce <= 3 {
                    (
                        "HIGH",
                        0.60,
                        format!("deployer nonce {nonce} on a fresh token — SINGLE-USE THROWAWAY WALLET pattern. Modern rug-bots create fresh EOAs per launch to break wallet-history linkage. Strong rug-prep automation signal."),
                    )
                } else if nonce > 5000 && young {
                    (
                        "HIGH",
                        0.60,
                        format!("deployer nonce {nonce} on a fresh token — high-volume serial-bot launcher pattern (Banana Gun / older Maestro / similar)."),
                    )
                } else if nonce > 1000 && young {
                    (
                        "MEDIUM",
                        0.40,
                        format!("deployer nonce {nonce} on a fresh token — automated launcher (medium-volume bot pattern)"),
                    )
                } else if young && (4..=50).contains(&nonce) {
                    (
                        "LOW",
                        0.20,
                        format!("deployer nonce {nonce} on a fresh token — modest manual activity, plausible real launch"),
                    )
                } else if nonce > 5000 {
                    (
                        "LOW",
                        0.20,
                        format!("deployer nonce {nonce} but token is mature — historic bot deployment, no immediate rug-prep signal"),
                    )
                } else {
                    (
                        "INFO",
                        0.05,
                        format!("deployer nonce {nonce} — within normal bounds for an established human deployer"),
                    )
                };
                let severity_obj = severity_from_confidence(conf);
                println!("  D09 verdict: {label} (severity={severity_obj:?}, confidence={conf:.2})");
                println!("    rationale: {rationale}");
                verdicts.push(DetectorVerdict {
                    id: "d09_deployer_pattern",
                    fired: matches!(label, "HIGH" | "MEDIUM" | "LOW"),
                    confidence: conf,
                    severity_label: label,
                    rationale: rationale.clone(),
                });
            }
            Err(e) => {
                println!("  D09 verdict: UNKNOWN — RPC error: {e}");
                verdicts.push(DetectorVerdict {
                    id: "d09_deployer_pattern",
                    fired: false,
                    confidence: 0.0,
                    severity_label: "INFO",
                    rationale: format!("RPC error during eth_getTransactionCount: {e}"),
                });
            }
        }
    } else {
        println!("  D09 verdict: SKIPPED — no mint event (from=0x0) captured in the 2000-block window");
        verdicts.push(DetectorVerdict {
            id: "d09_deployer_pattern",
            fired: false,
            confidence: 0.0,
            severity_label: "INFO",
            rationale: "no mint Transfer in window — deployer not identified".to_owned(),
        });
    }
    println!();

    // ------------------------------------------------------------------------
    // D08 — sybil-light. Take the top-N net-flow accumulators (already
    // computed for D03, post-suppression). Probe each one's nonce. A
    // cluster of throwaway-wallet buyers (nonce ≤ 2) on a fresh token
    // strongly suggests batch-funded sybil distribution — typically the
    // deployer or a coordinated buy-bot operator funded many fresh EOAs
    // and used them to fake distributed buying pressure.
    // ------------------------------------------------------------------------
    println!("== D08 sybil-light via top-holder nonce cluster ==");
    let sybil_candidates: Vec<String> = transfers_for_d05
        .iter()
        .filter(|t| {
            t.from == "0x0000000000000000000000000000000000000000"
                || t.from != t.to
        })
        .fold(
            std::collections::HashMap::<String, u128>::new(),
            |mut acc, t| {
                if t.to == "0x0000000000000000000000000000000000000000" {
                    return acc;
                }
                *acc.entry(t.to.clone()).or_insert(0) += t.amount;
                acc
            },
        )
        .into_iter()
        .collect::<Vec<_>>()
        .into_iter()
        .filter(|(addr, _)| addr != "0x0000000000000000000000000000000000000000")
        .map(|(addr, amt)| (addr, amt))
        .collect::<Vec<_>>()
        .into_iter()
        .map(|(addr, _)| addr)
        .take(10)
        .collect();
    if sybil_candidates.is_empty() {
        println!("  D08 verdict: SKIPPED — no candidate holders to probe");
        verdicts.push(DetectorVerdict {
            id: "d08_sybil_throwaway",
            fired: false,
            confidence: 0.0,
            severity_label: "INFO",
            rationale: "no holders in transfer log to classify".to_owned(),
        });
    } else {
        eprintln!(
            "[chain-adapter] calling eth_getTransactionCount(...) — nonce probe for top {} holders",
            sybil_candidates.len()
        );
        let mut throwaway = 0_usize;
        let mut probed = 0_usize;
        for addr in &sybil_candidates {
            match eth_get_transaction_count(rpc, addr).await {
                Ok(nonce) => {
                    probed += 1;
                    if nonce <= 2 {
                        throwaway += 1;
                    }
                }
                Err(_) => continue,
            }
        }
        println!(
            "  top-{} holders probed:    {} (probed {} successfully)",
            sybil_candidates.len(),
            probed,
            probed
        );
        println!(
            "  throwaway wallets (nonce ≤ 2): {} of {}",
            throwaway, probed
        );
        let young = matches!(contract_age_days, Some(d) if d < 30);
        let (label, conf, rationale): (&str, f64, String) = if probed < 5 {
            (
                "INFO",
                0.0,
                format!("only {probed} holders probed — sample too small"),
            )
        } else if throwaway >= 7 && young {
            (
                "HIGH",
                0.65,
                format!(
                    "{throwaway}/{probed} top holders are throwaway wallets (nonce ≤ 2) on a fresh token — strong batch-funded sybil cluster"
                ),
            )
        } else if throwaway >= 4 && young {
            (
                "MEDIUM",
                0.45,
                format!(
                    "{throwaway}/{probed} top holders are throwaway wallets — likely sybil distribution"
                ),
            )
        } else if throwaway >= 2 {
            (
                "LOW",
                0.20,
                format!(
                    "{throwaway}/{probed} top holders are throwaway wallets — minor cluster, plausibly MEV / sniper bots"
                ),
            )
        } else {
            (
                "INFO",
                0.05,
                format!(
                    "{throwaway}/{probed} throwaway wallets — no sybil cluster pattern"
                ),
            )
        };
        let severity_obj = severity_from_confidence(conf);
        println!("  D08 verdict: {label} (severity={severity_obj:?}, confidence={conf:.2})");
        println!("    rationale: {rationale}");
        verdicts.push(DetectorVerdict {
            id: "d08_sybil_throwaway",
            fired: matches!(label, "HIGH" | "MEDIUM" | "LOW"),
            confidence: conf,
            severity_label: label,
            rationale: rationale.clone(),
        });
    }
    println!();

    // Detectors not reached in v1 EVM CLI — surfaced in coverage gap.
    for (id, why) in [
        ("d12_permit2_drainer", "decoder shipped; needs log scan"),
        ("d13_sandwich_mev", "decoder shipped; needs mempool"),
    ] {
        verdicts.push(DetectorVerdict {
            id,
            fired: false,
            confidence: 0.0,
            severity_label: "INFO",
            rationale: why.to_owned(),
        });
    }

    let chain_label = match chain {
        ChainKind::Ethereum => "Ethereum",
        ChainKind::Bsc => "BSC",
        ChainKind::Base => "Base",
        ChainKind::Arbitrum => "Arbitrum",
        ChainKind::Optimism => "Optimism",
        ChainKind::Polygon => "Polygon",
        ChainKind::Avalanche => "Avalanche",
        ChainKind::Solana => "Solana",
    };
    print_composite(token_addr, chain_label, &verdicts);
    Ok(())
}

// ===========================================================================
// Discovery mode — list newly-listed tokens on Uniswap V2 / Pancake V2
// ===========================================================================

async fn run_discover(
    rpc: &str,
    is_bsc: bool,
    lookback_blocks: u64,
    chain: ChainKind,
    analyze: bool,
    top: usize,
) -> anyhow::Result<()> {
    let chain_label = match chain {
        ChainKind::Ethereum => "Ethereum",
        ChainKind::Bsc => "BSC",
        ChainKind::Base => "Base",
        ChainKind::Arbitrum => "Arbitrum",
        ChainKind::Optimism => "Optimism",
        ChainKind::Polygon => "Polygon",
        ChainKind::Avalanche => "Avalanche",
        ChainKind::Solana => "Solana",
    };
    let chain_arg = match chain {
        ChainKind::Ethereum => "ethereum",
        ChainKind::Bsc => "bsc",
        ChainKind::Base => "base",
        ChainKind::Arbitrum => "arbitrum",
        ChainKind::Optimism => "optimism",
        ChainKind::Polygon => "polygon",
        ChainKind::Avalanche => "avalanche",
        ChainKind::Solana => "solana",
    };
    println!("== onchain-check-token discover ==");
    println!("chain:           {chain_label}");
    println!("lookback blocks: {lookback_blocks}");
    if analyze {
        println!("analyse mode:    on — running analytics on the top-{top}");
    }
    println!();
    eprintln!("[chain-adapter] calling discover_recent_pairs(...) — eth_getLogs PairCreated");
    let pairs = discover_recent_pairs(rpc, is_bsc, lookback_blocks)
        .await
        .context("discover_recent_pairs failed")?;
    if pairs.is_empty() {
        println!("no new {} pairs in the last {lookback_blocks} blocks", chain_label);
        println!("(try increasing --blocks if the chain is quiet)");
        return Ok(());
    }
    println!(
        "  found {} newly-listed token(s) (paired with {}) — newest first",
        pairs.len(),
        if is_bsc { "WBNB" } else { "WETH" }
    );
    println!();
    println!(
        "{:>4}  {:<10}  {:<42}  {:<42}",
        "#", "block", "token", "pair"
    );
    for (i, p) in pairs.iter().take(30).enumerate() {
        println!(
            "{:>4}  {:<10}  {}  {}",
            i + 1,
            p.block_number,
            p.token,
            p.pair
        );
    }
    if pairs.len() > 30 {
        println!("    … and {} more (showing newest 30)", pairs.len() - 30);
    }
    println!();

    if !analyze {
        println!("Next step: pick an interesting one and run");
        println!("  onchain-check-token --chain {chain_arg} <token-address>");
        println!("Or re-run with `--analyze` to get a risk-differentiation table for the top tokens.");
        return Ok(());
    }

    // ---- Matrix mode: analytics per token via child-process spawn -------
    let exe = std::env::current_exe()
        .context("could not resolve current executable for child-process analytics")?;
    let take_n = top.min(pairs.len());
    println!("== analytics matrix (top {take_n}) ==");
    println!();
    println!(
        "{:>3}  {:<8}  {:<16}  {:<44}  {:<10}  {:<5}  {:<10}  {:<12}  {:<8}",
        "#", "sym", "name", "token", "verdict", "conf", "owner", "spike", "sim-sell"
    );
    println!(
        "{:>3}  {:<8}  {:<16}  {:<44}  {:<10}  {:<5}  {:<10}  {:<12}  {:<8}",
        "---", "---", "----", "-----", "-------", "----", "-----", "-----", "--------"
    );

    let mut summary_rows: Vec<MatrixRow> = Vec::with_capacity(take_n);
    for (idx, p) in pairs.iter().take(take_n).enumerate() {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg(&p.token);
        cmd.arg("--chain").arg(chain_arg);
        eprintln!("[matrix] {} / {} → {}", idx + 1, take_n, p.token);
        let output = cmd
            .output()
            .with_context(|| format!("spawn analytics for {}", p.token))?;
        let stdout_text = String::from_utf8_lossy(&output.stdout);

        let row = parse_analytics_output(&p.token, &stdout_text);
        println!(
            "{:>3}  {:<8}  {:<16}  {:<44}  {:<10}  {:<5}  {:<10}  {:<12}  {:<8}",
            idx + 1,
            truncate_for_col(&row.symbol, 8),
            truncate_for_col(&row.name, 16),
            row.token,
            row.verdict,
            format!("{:.2}", row.confidence),
            row.owner_status,
            row.spike_status,
            row.sim_sell
        );
        summary_rows.push(row);
    }

    // Risk highlight: tokens with active owner are in the rug-prep window.
    let active_owner: Vec<&MatrixRow> = summary_rows
        .iter()
        .filter(|r| r.owner_status == "active")
        .collect();
    println!();
    if !active_owner.is_empty() {
        println!(
            "RUG-PREP WATCH: {} of {} fresh tokens have an ACTIVE owner (not renounced):",
            active_owner.len(),
            take_n
        );
        for r in &active_owner {
            println!("  - {} (verdict {}, spike {})", r.token, r.verdict, r.spike_status);
        }
        println!();
    }
    let renounced: Vec<&MatrixRow> = summary_rows
        .iter()
        .filter(|r| r.owner_status == "renounced")
        .collect();
    if !renounced.is_empty() {
        println!(
            "Renounced ({}): less rug-prep risk but still fresh-launch CRITICAL on D10",
            renounced.len()
        );
    }
    println!();
    println!("Run `onchain-check-token --chain {chain_arg} <addr>` on any standout above for a full ladder + recommendation.");
    Ok(())
}

async fn run_discover_base(
    rpc: &str,
    lookback_blocks: u64,
    analyze: bool,
    top: usize,
) -> anyhow::Result<()> {
    const UNI_V3_FACTORY_BASE: &str = "0x33128a8fc17869897dce68ed026d694621f6fdfd";
    const WETH_BASE: &str = "0x4200000000000000000000000000000000000006";

    println!("== onchain-check-token discover (Base / Uniswap V3) ==");
    println!("lookback blocks: {lookback_blocks}");
    if analyze {
        println!("analyse mode:    on — running analytics on the top-{top}");
    }
    println!();

    eprintln!("[chain-adapter] calling discover_recent_v3_pools(...) — eth_getLogs PoolCreated");
    let pools =
        discover_recent_v3_pools(rpc, UNI_V3_FACTORY_BASE, WETH_BASE, "WETH", lookback_blocks)
            .await
            .context("discover_recent_v3_pools failed")?;
    if pools.is_empty() {
        println!("no new Base V3 pools paired with WETH in the last {lookback_blocks} blocks");
        println!("(try increasing --blocks; Base produces fewer pools per block than ETH mainnet)");
        return Ok(());
    }
    println!(
        "  found {} newly-listed Base token(s) (paired with WETH on Uniswap V3) — newest first",
        pools.len()
    );
    println!();
    println!(
        "{:>4}  {:<10}  {:<42}  {:<42}",
        "#", "block", "token", "pool"
    );
    for (i, p) in pools.iter().take(30).enumerate() {
        println!(
            "{:>4}  {:<10}  {}  {}",
            i + 1,
            p.block_number,
            p.token,
            p.pair
        );
    }
    if pools.len() > 30 {
        println!("    … and {} more (showing newest 30)", pools.len() - 30);
    }
    println!();

    if !analyze {
        println!("Next step: pick an interesting one and run");
        println!("  onchain-check-token --chain base <token-address>");
        println!("Or re-run with `--analyze` to get a risk-differentiation table.");
        return Ok(());
    }

    let exe = std::env::current_exe()
        .context("could not resolve current executable for child-process analytics")?;
    let take_n = top.min(pools.len());
    println!("== analytics matrix (top {take_n}) ==");
    println!();
    println!(
        "{:>3}  {:<8}  {:<16}  {:<44}  {:<10}  {:<5}  {:<10}  {:<12}  {:<8}",
        "#", "sym", "name", "token", "verdict", "conf", "owner", "spike", "sim-sell"
    );
    println!(
        "{:>3}  {:<8}  {:<16}  {:<44}  {:<10}  {:<5}  {:<10}  {:<12}  {:<8}",
        "---", "---", "----", "-----", "-------", "----", "-----", "-----", "--------"
    );

    let mut summary_rows: Vec<MatrixRow> = Vec::with_capacity(take_n);
    for (idx, p) in pools.iter().take(take_n).enumerate() {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg(&p.token);
        cmd.arg("--chain").arg("base");
        eprintln!("[matrix] {} / {} → {}", idx + 1, take_n, p.token);
        let output = cmd
            .output()
            .with_context(|| format!("spawn analytics for {}", p.token))?;
        let stdout_text = String::from_utf8_lossy(&output.stdout);
        let row = parse_analytics_output(&p.token, &stdout_text);
        println!(
            "{:>3}  {:<8}  {:<16}  {:<44}  {:<10}  {:<5}  {:<10}  {:<12}  {:<8}",
            idx + 1,
            truncate_for_col(&row.symbol, 8),
            truncate_for_col(&row.name, 16),
            row.token,
            row.verdict,
            format!("{:.2}", row.confidence),
            row.owner_status,
            row.spike_status,
            row.sim_sell
        );
        summary_rows.push(row);
    }

    let active_owner: Vec<&MatrixRow> = summary_rows
        .iter()
        .filter(|r| r.owner_status == "active")
        .collect();
    println!();
    if !active_owner.is_empty() {
        println!(
            "RUG-PREP WATCH: {} of {} fresh Base tokens have an ACTIVE owner:",
            active_owner.len(),
            take_n
        );
        for r in &active_owner {
            println!("  - {} (verdict {}, spike {})", r.token, r.verdict, r.spike_status);
        }
    }
    println!();
    println!("Run `onchain-check-token --chain base <addr>` on any standout above for a full ladder + recommendation.");
    Ok(())
}

/// Generic Uniswap-V3-only discovery helper used by chains that share the
/// canonical V3 factory address (Optimism + Polygon both deployed V3 via
/// CREATE2 at `0x1F98431c…`). Skips the V2 path because OP lacks a
/// canonical V2 and Polygon's QuickSwap V2 has its own discovery topic
/// — V3 alone covers the bulk of fresh listings on both.
async fn run_discover_v3(
    rpc: &str,
    lookback_blocks: u64,
    analyze: bool,
    top: usize,
    chain_label: &str,
    chain_arg: &str,
    base_token_addr: &str,
) -> anyhow::Result<()> {
    const UNI_V3_FACTORY: &str = "0x1f98431c8ad98523631ae4a59f267346ea31f984";
    println!("== onchain-check-token discover ({chain_label} / Uniswap V3) ==");
    println!("lookback blocks: {lookback_blocks}");
    if analyze {
        println!("analyse mode:    on — running analytics on the top-{top}");
    }
    println!();
    eprintln!("[chain-adapter] calling discover_recent_v3_pools(...) — eth_getLogs PoolCreated");
    let pools =
        discover_recent_v3_pools(rpc, UNI_V3_FACTORY, base_token_addr, "WETH/native", lookback_blocks)
            .await
            .context("discover_recent_v3_pools failed")?;
    if pools.is_empty() {
        println!("no new {chain_label} V3 pools paired with WETH/native in the last {lookback_blocks} blocks");
        return Ok(());
    }
    println!(
        "  found {} newly-listed {chain_label} token(s) (Uniswap V3 / native) — newest first",
        pools.len()
    );
    println!();
    println!(
        "{:>4}  {:<10}  {:<42}  {:<42}",
        "#", "block", "token", "pool"
    );
    for (i, p) in pools.iter().take(30).enumerate() {
        println!(
            "{:>4}  {:<10}  {}  {}",
            i + 1,
            p.block_number,
            p.token,
            p.pair
        );
    }
    if pools.len() > 30 {
        println!("    … and {} more (showing newest 30)", pools.len() - 30);
    }
    println!();
    if !analyze {
        println!("Next step:");
        println!("  onchain-check-token --chain {chain_arg} <token-address>");
        return Ok(());
    }
    let exe = std::env::current_exe()
        .context("could not resolve current executable for child-process analytics")?;
    let take_n = top.min(pools.len());
    println!("== analytics matrix (top {take_n}) ==");
    println!();
    println!(
        "{:>3}  {:<8}  {:<16}  {:<44}  {:<10}  {:<5}  {:<10}  {:<12}  {:<8}",
        "#", "sym", "name", "token", "verdict", "conf", "owner", "spike", "sim-sell"
    );
    println!(
        "{:>3}  {:<8}  {:<16}  {:<44}  {:<10}  {:<5}  {:<10}  {:<12}  {:<8}",
        "---", "---", "----", "-----", "-------", "----", "-----", "-----", "--------"
    );
    let mut summary_rows: Vec<MatrixRow> = Vec::with_capacity(take_n);
    for (idx, p) in pools.iter().take(take_n).enumerate() {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg(&p.token);
        cmd.arg("--chain").arg(chain_arg);
        eprintln!("[matrix] {} / {} → {}", idx + 1, take_n, p.token);
        let output = cmd
            .output()
            .with_context(|| format!("spawn analytics for {}", p.token))?;
        let stdout_text = String::from_utf8_lossy(&output.stdout);
        let row = parse_analytics_output(&p.token, &stdout_text);
        println!(
            "{:>3}  {:<8}  {:<16}  {:<44}  {:<10}  {:<5}  {:<10}  {:<12}  {:<8}",
            idx + 1,
            truncate_for_col(&row.symbol, 8),
            truncate_for_col(&row.name, 16),
            row.token,
            row.verdict,
            format!("{:.2}", row.confidence),
            row.owner_status,
            row.spike_status,
            row.sim_sell
        );
        summary_rows.push(row);
    }
    let active_owner: Vec<&MatrixRow> = summary_rows
        .iter()
        .filter(|r| r.owner_status == "active")
        .collect();
    println!();
    if !active_owner.is_empty() {
        println!(
            "RUG-PREP WATCH: {} of {} fresh {chain_label} tokens have an ACTIVE owner:",
            active_owner.len(),
            take_n
        );
        for r in &active_owner {
            println!(
                "  - {} (verdict {}, spike {})",
                r.token, r.verdict, r.spike_status
            );
        }
    }
    println!();
    println!(
        "Run `onchain-check-token --chain {chain_arg} <addr>` on any standout for a full ladder."
    );
    Ok(())
}

async fn run_discover_arbitrum(
    rpc: &str,
    lookback_blocks: u64,
    analyze: bool,
    top: usize,
) -> anyhow::Result<()> {
    // Arbitrum's Uniswap V3 deployment uses the SAME factory address as
    // L1 mainnet (0x1F98431c…) — Uniswap deployed canonical V3 at the
    // same address across chains via CREATE2.
    const UNI_V3_FACTORY: &str = "0x1f98431c8ad98523631ae4a59f267346ea31f984";
    const WETH_ARB: &str = "0x82af49447d8a07e3bd95bd0d56f35241523fbab1";

    println!("== onchain-check-token discover (Arbitrum / Uniswap V3) ==");
    println!("lookback blocks: {lookback_blocks}");
    if analyze {
        println!("analyse mode:    on — running analytics on the top-{top}");
    }
    println!();
    eprintln!("[chain-adapter] calling discover_recent_v3_pools(...) — eth_getLogs PoolCreated");
    let pools = discover_recent_v3_pools(rpc, UNI_V3_FACTORY, WETH_ARB, "WETH", lookback_blocks)
        .await
        .context("discover_recent_v3_pools failed")?;
    if pools.is_empty() {
        println!("no new Arbitrum V3 pools paired with WETH in the last {lookback_blocks} blocks");
        return Ok(());
    }
    println!(
        "  found {} newly-listed Arbitrum token(s) (Uniswap V3 / WETH) — newest first",
        pools.len()
    );
    println!();
    println!(
        "{:>4}  {:<10}  {:<42}  {:<42}",
        "#", "block", "token", "pool"
    );
    for (i, p) in pools.iter().take(30).enumerate() {
        println!(
            "{:>4}  {:<10}  {}  {}",
            i + 1,
            p.block_number,
            p.token,
            p.pair
        );
    }
    if pools.len() > 30 {
        println!("    … and {} more (showing newest 30)", pools.len() - 30);
    }
    println!();
    if !analyze {
        println!("Next step: pick an interesting one and run");
        println!("  onchain-check-token --chain arbitrum <token-address>");
        return Ok(());
    }
    let exe = std::env::current_exe()
        .context("could not resolve current executable for child-process analytics")?;
    let take_n = top.min(pools.len());
    println!("== analytics matrix (top {take_n}) ==");
    println!();
    println!(
        "{:>3}  {:<8}  {:<16}  {:<44}  {:<10}  {:<5}  {:<10}  {:<12}  {:<8}",
        "#", "sym", "name", "token", "verdict", "conf", "owner", "spike", "sim-sell"
    );
    println!(
        "{:>3}  {:<8}  {:<16}  {:<44}  {:<10}  {:<5}  {:<10}  {:<12}  {:<8}",
        "---", "---", "----", "-----", "-------", "----", "-----", "-----", "--------"
    );
    let mut summary_rows: Vec<MatrixRow> = Vec::with_capacity(take_n);
    for (idx, p) in pools.iter().take(take_n).enumerate() {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg(&p.token);
        cmd.arg("--chain").arg("arbitrum");
        eprintln!("[matrix] {} / {} → {}", idx + 1, take_n, p.token);
        let output = cmd
            .output()
            .with_context(|| format!("spawn analytics for {}", p.token))?;
        let stdout_text = String::from_utf8_lossy(&output.stdout);
        let row = parse_analytics_output(&p.token, &stdout_text);
        println!(
            "{:>3}  {:<8}  {:<16}  {:<44}  {:<10}  {:<5}  {:<10}  {:<12}  {:<8}",
            idx + 1,
            truncate_for_col(&row.symbol, 8),
            truncate_for_col(&row.name, 16),
            row.token,
            row.verdict,
            format!("{:.2}", row.confidence),
            row.owner_status,
            row.spike_status,
            row.sim_sell
        );
        summary_rows.push(row);
    }
    let active_owner: Vec<&MatrixRow> = summary_rows
        .iter()
        .filter(|r| r.owner_status == "active")
        .collect();
    println!();
    if !active_owner.is_empty() {
        println!(
            "RUG-PREP WATCH: {} of {} fresh Arbitrum tokens have an ACTIVE owner:",
            active_owner.len(),
            take_n
        );
        for r in &active_owner {
            println!("  - {} (verdict {}, spike {})", r.token, r.verdict, r.spike_status);
        }
    }
    println!();
    println!("Run `onchain-check-token --chain arbitrum <addr>` on any standout above for a full ladder + recommendation.");
    Ok(())
}

async fn run_discover_solana(
    rpc: &str,
    analyze: bool,
    top: usize,
) -> anyhow::Result<()> {
    println!("== onchain-check-token discover (Solana / Pump.fun) ==");
    if analyze {
        println!("analyse mode:    on — running analytics on the top-{top}");
    }
    println!();
    let http_url = Url::parse(rpc).with_context(|| format!("invalid --rpc URL: {rpc}"))?;
    let ws_url = Url::parse("ws://127.0.0.1:8900")
        .expect("hard-coded ws placeholder must parse");
    let config = SolanaAdapterConfig {
        http_url,
        ws_url,
        auth_token: None,
        commitment: CommitmentConfig::Confirmed,
        reconnect: ReconnectPolicy::default(),
        filters: SubscribeFiltersConfig::default(),
        checkpoint_path: "/tmp/onchain-check-token-noop.checkpoint".to_owned(),
    };

    eprintln!("[chain-adapter] calling discover_pumpfun_recent(...) — getSignaturesForAddress + getTransaction(jsonParsed)");
    let tokens = discover_pumpfun_recent(&config, 100)
        .await
        .context("discover_pumpfun_recent failed")?;
    if tokens.is_empty() {
        println!("no fresh Pump.fun tokens decoded in the last 100 signatures");
        println!("(public RPC throttles aggressively here — try --rpc <self-hosted>)");
        return Ok(());
    }
    println!(
        "  found {} fresh Pump.fun token(s) in the last 100 program signatures — newest first",
        tokens.len()
    );
    println!();
    println!(
        "{:>4}  {:<20}  {:<46}  {:<88}",
        "#", "block_time", "mint", "signature"
    );
    let chrono_now = chrono::Utc::now().timestamp();
    for (i, t) in tokens.iter().take(30).enumerate() {
        let ago = match t.block_time {
            Some(bt) => format!("{}m ago", (chrono_now - bt) / 60),
            None => "unknown".to_owned(),
        };
        println!(
            "{:>4}  {:<20}  {:<46}  {}",
            i + 1,
            ago,
            t.mint,
            t.signature
        );
    }
    if tokens.len() > 30 {
        println!("    … and {} more (showing newest 30)", tokens.len() - 30);
    }
    println!();

    if !analyze {
        println!("Next step: pick an interesting one and run");
        println!("  onchain-check-token --chain solana <mint-address>");
        println!("Or re-run with `--analyze` to get a risk-differentiation table.");
        return Ok(());
    }

    let exe = std::env::current_exe()
        .context("could not resolve current executable for child-process analytics")?;
    let take_n = top.min(tokens.len());
    println!("== analytics matrix (top {take_n}) ==");
    println!();
    println!(
        "{:>3}  {:<46}  {:<10}  {:<5}  {:<14}",
        "#", "mint", "verdict", "conf", "top signal"
    );
    println!(
        "{:>3}  {:<46}  {:<10}  {:<5}  {:<14}",
        "---", "----", "-------", "----", "----------"
    );

    for (idx, t) in tokens.iter().take(take_n).enumerate() {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg(&t.mint);
        cmd.arg("--chain").arg("solana");
        eprintln!("[matrix] {} / {} → {}", idx + 1, take_n, t.mint);
        let output = cmd
            .output()
            .with_context(|| format!("spawn analytics for {}", t.mint))?;
        let stdout_text = String::from_utf8_lossy(&output.stdout);
        let row = parse_analytics_output(&t.mint, &stdout_text);
        println!(
            "{:>3}  {:<46}  {:<10}  {:<5}  {}",
            idx + 1,
            row.token,
            row.verdict,
            format!("{:.2}", row.confidence),
            row.spike_status
        );
    }
    println!();
    println!("Run `onchain-check-token --chain solana <mint>` on any standout above for a full ladder + recommendation.");
    Ok(())
}

#[derive(Debug, Clone)]
struct MatrixRow {
    token: String,
    /// Token symbol (e.g. "USDT") parsed from `evm_token_metadata.symbol`.
    /// Empty when the metadata call reverted / the token doesn't implement
    /// `symbol()` (rare, but seen on some legacy proxies).
    symbol: String,
    /// Token name (e.g. "Tether USD"). Truncated for display.
    name: String,
    verdict: String,
    confidence: f64,
    owner_status: &'static str, // "renounced" / "active" / "n/a"
    spike_status: String,        // "NORMAL 0.79×" / "HIGH 4.22×" / "no swaps" / "—"
    sim_sell: String,
}

/// Parse the captured stdout of a child `onchain-check-token <addr>` run
/// into a compact one-row summary. Memecoin trader cares about the
/// differentiators, not the always-CRITICAL D10 fresh-launch signal — so
/// we extract D02 owner status, D04 spike, and simulate-sell outcome
/// instead.
/// Truncate a free-text column value (e.g. token name like
/// "USD Coin (PoS)") to fit a fixed-width column without breaking
/// alignment. Adds an ellipsis when shortened.
fn truncate_for_col(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

fn parse_analytics_output(token: &str, stdout: &str) -> MatrixRow {
    let mut verdict = "?".to_owned();
    let mut confidence = 0.0_f64;
    let mut owner_status: &'static str = "n/a";
    let mut spike_status = "—".to_owned();
    let mut sim_sell = "?".to_owned();
    let mut name = String::new();
    let mut symbol = String::new();

    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("name:") {
            name = rest.trim().to_owned();
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("symbol:") {
            symbol = rest.trim().to_owned();
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("composite verdict:") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if !parts.is_empty() {
                verdict = parts[0].to_owned();
            }
            if let Some(start) = rest.find("confidence ") {
                let tail = &rest[start + "confidence ".len()..];
                let num: String = tail.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
                confidence = num.parse().unwrap_or(0.0);
            }
        } else if trimmed.contains("d02_ownable_owner") {
            // Distinguish: "owner() = 0x000…000 → ownership renounced"
            //          vs: "owner() = 0x… (non-zero) → active EOA/contract"
            //          vs: "owner() reverted → not Ownable"
            if line.contains("ownership renounced") {
                owner_status = "renounced";
            } else if line.contains("not Ownable") {
                owner_status = "n/a";
            } else if line.contains("active EOA") || line.contains("active") {
                owner_status = "active";
            }
        } else if trimmed.starts_with("spike ratio")
            || trimmed.contains("d04_pump_dump")
            || trimmed.contains("D04 verdict:")
        {
            // Capture the most informative D04 line.
            if line.contains("no swaps") {
                spike_status = "no swaps".to_owned();
            } else if let Some(idx) = line.find("spike ratio ") {
                // e.g. "spike ratio 4.22× — elevated above trailing baseline"
                let tail = &line[idx + "spike ratio ".len()..];
                let ratio: String = tail
                    .chars()
                    .take_while(|c| !c.is_whitespace() && *c != '—')
                    .collect();
                let label = if line.contains("HIGH") {
                    "HIGH"
                } else if line.contains("MEDIUM") {
                    "MED"
                } else if line.contains("NORMAL") {
                    "norm"
                } else if line.contains("LOW") {
                    "low"
                } else {
                    "?"
                };
                spike_status = format!("{label} {ratio}");
            }
        } else if let Some(rest) = trimmed.strip_prefix("simulate-sell:") {
            let r = rest.trim();
            sim_sell = if r.starts_with('✓') {
                "✓".to_owned()
            } else if r.starts_with('✗') {
                "REVERT".to_owned()
            } else if r.starts_with("skipped") {
                "skip".to_owned()
            } else if r.starts_with("RPC") {
                "rpc-err".to_owned()
            } else {
                r.split_whitespace().next().unwrap_or("?").to_owned()
            };
        }
    }
    MatrixRow {
        token: token.to_owned(),
        symbol,
        name,
        verdict,
        confidence,
        owner_status,
        spike_status,
        sim_sell,
    }
}

/// Format a u256-as-decimal-string with the given decimals as a UI value.
/// Pure string surgery to avoid pulling in num-bigint just for display.
fn format_decimal_with_decimals(raw: &str, decimals: u8) -> String {
    let d = decimals as usize;
    if raw.len() <= d {
        let pad = "0".repeat(d - raw.len());
        return format!("0.{pad}{raw}").trim_end_matches('0').trim_end_matches('.').to_owned();
    }
    let split = raw.len() - d;
    let (int_part, frac_part) = raw.split_at(split);
    let frac_trimmed = frac_part.trim_end_matches('0');
    if frac_trimmed.is_empty() {
        int_part.to_owned()
    } else {
        format!("{int_part}.{frac_trimmed}")
    }
}
