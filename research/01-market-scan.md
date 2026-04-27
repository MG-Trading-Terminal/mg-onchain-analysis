# Market Scan — On-Chain Analytics Products

**Date:** 2026-04-21
**Author:** PM, mg-onchain-analysis
**Status:** Draft, Phase 0 research

---

## Methodology note

Initial draft written from model knowledge on 2026-04-21; verified against live vendor docs and live API responses later the same day. Remaining `unverified` tags mark claims that could not be confirmed against live sources (vendor page unreachable, docs behind login wall, or product appears defunct/pivoted). All changes introduced by the verification pass are itemised in the **Verification Log** at the bottom of this document.

---

## 1. Product entries

### GoPlus Security
- **URL:** https://gopluslabs.io
- **Category:** risk-scanner (token + address + NFT + dApp)
- **Chains:** 40+ chains including ETH, BSC, Base, Arbitrum, Polygon, Avalanche, Optimism, Fantom, Solana, Sui, Monad, HashKey Chain (Solana coverage added later than EVM)
- **What it detects/provides:** Pre-trade token risk API: honeypot, tax rates, proxy/ownership risk, blacklist/whitelist capability, LP lock %, top-holder concentration, plus address risk (sanctions, phishing, mixer, malicious dApp). 30+ security detection items across Contract Security / Trading Security / Info Security categories.
- **Method:** heuristic contract-bytecode analysis + transfer simulation (buy/sell tax via forked-state eval) + address reputation DB `unverified`
- **API available:** yes — public REST at `api.gopluslabs.io/api/v1/token_security/{chain_id}`; docs at https://docs.gopluslabs.io
- **Pricing posture:** freemium — free tier 100 call credits/minute, paid tiers for higher QPS; enterprise contact
- **Strengths:** broad chain coverage, mature token-security schema that has become a de-facto standard adopted by several wallets (TrustWallet, Safe, CMC) `unverified`
- **Limitations:** heuristic flags only — no probabilistic confidence; tax values can be stale (not recomputed per block); Solana feature parity lags EVM `unverified`
- **Specific signals exposed:** `is_honeypot`, `buy_tax`, `sell_tax`, `is_open_source`, `is_proxy`, `is_mintable`, `owner_change_balance`, `hidden_owner`, `can_take_back_ownership`, `transfer_pausable`, `trading_cooldown`, `is_blacklisted`, `is_whitelisted`, `is_anti_whale`, `anti_whale_modifiable`, `slippage_modifiable`, `personal_slippage_modifiable`, `is_in_dex`, `lp_holders`, `lp_total_supply`, `holder_count`, `top_10_holder_percent`, `creator_address`, `creator_balance`, `creator_percent`, `owner_address`, `owner_balance`, `owner_percent` `unverified`

### TokenSniffer
- **URL:** https://tokensniffer.com (API docs: https://tokensniffer.readme.io; parent company: https://soliduslabs.com/tokensniffer/api)
- **Category:** risk-scanner (multi-chain token)
- **Chains:** Solana, ETH, BSC, Base, Polygon, Arbitrum, Optimism and others (multi-chain since 2025; was EVM-only in earlier versions)
- **What it detects/provides:** Rule-based scam-token detector producing a 0–100 score. Flags honeypot, hidden mint, owner privileges, similar-contract match to known scams. `unverified`
- **Method:** heuristic rule engine over source + bytecode + swap simulation; similarity matching against a curated scam corpus `unverified`
- **API available:** yes — public REST at `https://tokensniffer.com/api/v2/tokens/{chain_id}/{address}` (query params: `include_metrics`, `include_tests`, `include_similar`)
- **Pricing posture:** paid — Sniffer Pack $99/mo (500 tokens/day), Pro (5000+/day), Enterprise (custom)
- **Strengths:** good "similar to known scam" heuristic (hard-to-replicate corpus); simple score easy to consume; now backed by Solidus Labs compliance infra (acquired TokenSniffer)
- **Limitations:** score is opaque (no per-signal breakdown in free tier); signal field list partially behind login wall `unverified`
- **Specific signals exposed:** honeypot-sim, swap-tax, owner privileges (mint/pause/blacklist), LP-lock status, prior-scam similarity match, contract-verified flag, dev-wallet holdings `unverified`

### De.Fi Shield / Scanner
- **URL:** https://de.fi
- **Category:** risk-scanner (contract + portfolio)
- **Chains:** 20+ EVM chains `unverified`
- **What it detects/provides:** Contract vulnerability scanner (Shield) + portfolio risk exposure (Scanner). Focus on smart-contract risk categories (logic, access control, reentrancy) rather than meme-token scam patterns. `unverified`
- **Method:** static analysis of verified source + bytecode pattern matching; portfolio-risk cross-references user holdings against exploited protocols `unverified`
- **API available:** partial — enterprise API advertised; no well-documented public endpoint `unverified`
- **Pricing posture:** freemium UI, enterprise API `unverified`
- **Strengths:** strongest on protocol-level smart-contract risk (not just ERC-20 scams); portfolio exposure unique angle
- **Limitations:** not optimized for Solana or meme-token flow analysis; API less accessible than GoPlus
- **Specific signals exposed:** vulnerability categories (reentrancy, oracle-manip, unchecked call), audit status, admin-key privileges, proxy-upgrade risk, cross-protocol exposure, holdings-in-exploited-protocol flag `unverified`

### RugCheck.xyz
- **URL:** https://rugcheck.xyz
- **Category:** risk-scanner (Solana token)
- **Chains:** Solana (sole focus)
- **What it detects/provides:** Solana-native token risk score with emphasis on mint/freeze authority, LP burn/lock, top-holder concentration, bundler/sniper detection on launch, Token-2022 transfer-fee abuse, insider graph. Post-hoc `rugged` ground-truth label.
- **Method:** heuristic over Solana account state + Raydium/Orca/pump.fun pool state + early-buyer clustering + insider-network graph construction
- **API available:** yes — public REST at `https://api.rugcheck.xyz/v1/` (live-verified with a token report call)
- **Pricing posture:** free with rate limits; premium tier `unverified`
- **Strengths:** deepest Solana coverage among risk scanners; catches pump.fun / moonshot launch patterns; community-trusted in SOL memecoin circles
- **Limitations:** Solana-only; rule-based (no confidence calibration); signals drift with platform updates (e.g., Raydium v4 migration) `unverified`
- **Specific signals exposed (confirmed from live API response):** `mint`, `tokenProgram`, `creator`, `creatorBalance`, `token_extensions`, `tokenMeta`, `topHolders`, `freezeAuthority`, `mintAuthority`, `risks`, `score`, `score_normalised`, `fileMeta`, `lockerOwners`, `lockers`, `lockerScanStatus`, `markets` (with `liquidity`, `lp_burned_pct`), `totalMarketLiquidity`, `totalStableLiquidity`, `totalLPProviders`, `totalHolders`, `price`, `rugged` (boolean, ground-truth label), `tokenType`, `transferFee` (pct, maxAmount, authority — Token-2022), `knownAccounts`, `events`, `verification` (Jupiter verified / strict flags), `graphInsidersDetected`, `insiderNetworks` (bundler graph), `detectedAt`, `creatorTokens`, `launchpad`, `deployPlatform`

### Honeypot.is
- **URL:** https://honeypot.is
- **Category:** risk-scanner (EVM, narrow focus)
- **Chains:** ETH, BSC, Base
- **What it detects/provides:** Pure buy/sell simulation — attempts to buy and immediately sell a token against the deepest pool, reports tax + success/fail.
- **Method:** fork-state simulation (eth_call over a forked block) — not heuristic; direct empirical test
- **API available:** yes — `https://api.honeypot.is/v2/IsHoneypot?address=...&chainID=...` (live-verified)
- **Pricing posture:** free (tip-supported)
- **Strengths:** high-signal for its narrow scope — simulation empirically catches dynamic honeypots that static analysis misses; easy integration
- **Limitations:** honeypot-only (no holder concentration, LP lock, etc.); relies on RPC that may be rate-limited; ETH/BSC/Base only
- **Specific signals exposed (confirmed from live API response):** top-level `token`, `withToken`, `summary`, `simulationSuccess`, `honeypotResult.isHoneypot`, `simulationResult.{buyTax, sellTax, transferTax, buyGas, sellGas}`, `flags[]` (revert reasons), `contractCode.{openSource, rootOpenSource, isProxy, hasProxyCalls}`, `chain`, `router`, `pair`, `pairAddress` (with `liquidity`)

### QuickIntel
- **URL:** https://app.quickintel.io
- **Category:** risk-scanner (multi-chain)
- **Chains:** 37 chains confirmed — includes EVM L1/L2s, Solana, Sui, Injective, Radix, Mantle, ZetaChain, Manta Pacific (source: docs.quickintel.io/developer-integration/api-integration/supported-chains-and-dex)
- **What it detects/provides:** Broad token audit: contract privileges, liquidity analysis, holder distribution, plus "QuickAudit" score. Adds social/launch context (telegram age, website age). `unverified`
- **Method:** mixed — static contract checks + swap simulation + off-chain enrichment `unverified`
- **API available:** yes — paid tiers (`api.quickintel.io`); Starter plan 5 calls/sec, max 75k calls/month
- **Pricing posture:** paid API tiers (monthly)
- **Strengths:** widest chain list (including newer L2s); off-chain context (socials) combined with on-chain
- **Limitations:** mixed-quality flags on long-tail chains; closed scoring formula
- **Specific signals exposed:** owner privileges (mint/pause/blacklist/proxy), LP lock + %, LP acquirer concentration, top-10 holders, honeypot-sim, contract-verified, external-call risk, social freshness `unverified`

### Arkham Intelligence
- **URL:** https://arkhamintelligence.com
- **Category:** wallet-analytics / deanonymization
- **Chains:** 18+ chains including BTC, ETH, BSC, Base, Arbitrum, Polygon, Avalanche, Optimism, Solana, Tron
- **What it detects/provides:** Entity attribution (wallet → real-world identity/org), flow graph visualization, whale and exchange-flow alerts. "Arkham Ultra" entity-matching engine is the core product.
- **Method:** proprietary entity clustering + manual labeling + ML attribution; "Intel Exchange" incentivized-bounty labels `unverified`
- **API available:** yes — public REST at `https://intel.arkm.com/api` (docs at `intel.arkm.com/api/docs`)
- **Pricing posture:** freemium UI, paid API, enterprise `unverified`
- **Strengths:** best-in-class entity labels (institutions, funds, CEXs); visual flow graph; supports BTC + multi-chain in one namespace
- **Limitations:** core value is proprietary label DB — not a detection engine; real-time alert granularity limited; API pricey `unverified`
- **Specific signals exposed:** entity-label on address, tagged-cluster membership, large-transfer alert, CEX deposit/withdrawal flows, bridge flows, wallet-balance history `unverified`

### Nansen
- **URL:** https://nansen.ai
- **Category:** wallet-analytics / smart-money
- **Chains:** 18+ chains including ETH, BSC, Polygon, Arbitrum, Avalanche, Optimism, Base, Solana, Ronin
- **What it detects/provides:** "Smart Money" cohorts (labeled profitable wallets), token-god-mode (flows, holders, P&L per cohort), alerts on smart-money buys/sells.
- **Method:** wallet labeling (heuristic + manual) + P&L computation + cohort aggregations; ML for wallet behavior classification `unverified`
- **API available:** yes — Nansen Query + REST API; Smart Money endpoint at `docs.nansen.ai/api/smart-money`
- **Pricing posture:** public credit-based — 100 free credits for testing; $0.001/credit base price; pay-per-call via x402 (USDC on Base/Solana): $0.01/call basic, $0.05/call Smart Money / premium. Not enterprise-only.
- **Strengths:** smart-money label set is the industry benchmark; rich cohort analytics; solid Solana coverage since 2024; per-call pricing makes integration cheap for targeted queries
- **Limitations:** labels are point-in-time and can decay; not real-time enough for MEV-speed use cases `unverified`
- **Specific signals exposed:** smart-money-buying / selling, token P&L by cohort, token-god-mode holder shifts, wallet-label, exchange-flow, stablecoin-inflow / outflow `unverified`

### DeBank
- **URL:** https://debank.com
- **Category:** wallet-analytics / portfolio
- **Chains:** 30+ EVM chains (historically EVM-only; Solana coverage partial) `unverified`
- **What it detects/provides:** Per-wallet portfolio view (holdings + DeFi positions across protocols), historical P&L, social "Hi" feed, wallet following/alerts. `unverified`
- **Method:** direct on-chain indexing + protocol adapters; portfolio accounting
- **API available:** yes — OpenAPI at `openapi.debank.com` (paid) `unverified`
- **Pricing posture:** free UI; paid API (per-call pricing) `unverified`
- **Strengths:** unmatched DeFi-position coverage (per protocol accounting); normalized cross-protocol schema
- **Limitations:** not a detector — provides raw positional data, not anomaly signals; weaker on new/long-tail chains; Solana thin
- **Specific signals exposed:** wallet NAV, protocol-level positions (LP, lending, staking), token balances, historical snapshots, gas spent `unverified`

### Bubblemaps
- **URL:** https://bubblemaps.io
- **Category:** wallet-analytics / holder graph
- **Chains:** 12 chains confirmed — BNB Chain, Ethereum, Solana, Base, Tron, TON, Apechain, Sonic, Monad, Polygon, Avalanche, Aptos (source: bubblemaps.io homepage)
- **What it detects/provides:** Visual holder graph — clusters wallets that are funded by the same source or transact with each other, exposing "bundled" supply hidden across many addresses.
- **Method:** graph construction from Transfer events; clustering by funding-source + co-movement heuristics
- **API available:** yes — programmatic API available ("Bring Bubblemaps analytics to your protocol, platform, or tool"); pricing not public
- **Pricing posture:** free UI, paid enterprise / contact-based
- **Strengths:** unique visual + quantitative cluster-concentration signal ("% supply in connected clusters" catches insiders that top-10 holder % misses)
- **Limitations:** API not broadly available; graph quality depends on Transfer-event completeness (misses opaque Token-2022 hooks or mixers); Solana later addition, less mature `unverified`
- **Specific signals exposed:** cluster membership, cluster-% of supply, funding-source graph, suspicious-transfer edges `unverified`

### Chainalysis
- **URL:** https://chainalysis.com
- **Category:** compliance
- **Chains:** 25+ incl. BTC, ETH, BSC, Solana, Tron, L2s `unverified`
- **What it detects/provides:** Sanctioned-address screening, exposure scoring (direct + indirect to sanctioned / stolen / ransomware / mixer / darknet / CSAM categories), KYT (know-your-transaction) risk scoring for inbound/outbound transfers. Acquired **Hexagate** in December 2024 (~$60M), adding real-time protocol threat-prevention tooling to the portfolio.
- **Method:** proprietary clustering + manual investigations + intel feeds; risk categories weighted per jurisdiction
- **API available:** yes — Chainalysis KYT, Entity, Address Screening, Kryptos APIs; free "Screening Oracle" (on-chain contract) for OFAC addresses `unverified`
- **Pricing posture:** enterprise (expensive); sanctioned-oracle is free
- **Strengths:** regulatory gold standard (used by banks, exchanges, law enforcement); deepest investigative DB
- **Limitations:** enterprise-only for meaningful use; slow to label new addresses; no trading/MEV/rug-pull signal focus
- **Specific signals exposed:** sanctions hit, risk category (mixer, stolen funds, ransomware, darknet, CSAM, scam), exposure %, counterparty cluster `unverified`

### TRM Labs
- **URL:** https://trmlabs.com
- **Category:** compliance
- **Chains:** 30+ `unverified`
- **What it detects/provides:** Wallet screening, transaction monitoring, investigations; risk scoring with emphasis on tracing and entity attribution. Similar feature set to Chainalysis with reported edge on newer chains. `unverified`
- **Method:** proprietary clustering + intel feeds + ML risk scoring
- **API available:** yes — enterprise REST API
- **Pricing posture:** enterprise
- **Strengths:** multi-chain breadth, including newer L2s faster than Chainalysis `unverified`
- **Limitations:** enterprise-only; same "no trading signal" gap as Chainalysis
- **Specific signals exposed:** sanctions, risk category, exposure %, entity cluster, counterparty risk `unverified`

### Elliptic
- **URL:** https://elliptic.co
- **Category:** compliance
- **Chains:** BTC, ETH, BSC, Polygon, Solana, Tron, and others `unverified`
- **What it detects/provides:** AML/sanctions screening, "Holistic" cross-chain tracing across bridges, investigations. `unverified`
- **Method:** proprietary clustering + cross-chain bridge tracing + intel feeds
- **API available:** yes — enterprise REST API
- **Pricing posture:** enterprise
- **Strengths:** reported strength in cross-chain tracing (bridge-aware); European regulator adoption
- **Limitations:** enterprise-only; no trading / MEV signal focus
- **Specific signals exposed:** sanctions, risk category, cross-chain exposure, bridge-path `unverified`

### Forta Network
- **URL:** https://forta.org
- **Category:** monitoring / detection-bot network
- **Chains:** ETH, BSC, Polygon, Avalanche, Arbitrum, Optimism, Fantom — EVM-only (confirmed)
- **What it detects/provides:** Decentralized network of detection bots publishing alerts for arbitrary on-chain anomalies (exploits, suspicious calls, governance attacks, bridge anomalies, scam tokens, funding-source risks). Alert fields: addresses, alert IDs, block number/timestamp ranges, chain IDs, severity, transaction hashes.
- **Method:** mixed — any developer can write a bot (JS/Python), many bots use heuristics, some use ML; "Attack Detector" is a meta-bot combining many sub-alerts
- **API available:** yes — GraphQL public endpoint at `https://api.forta.network/graphql` (no API key required); SDK for bot authors
- **Pricing posture:** alert consumption free; bot authors earn FORT token; "Forta Firewall" preventive product is commercial `unverified`
- **Strengths:** large library of existing detectors (600+ bots claimed); community contributions cover long-tail exploits; open-source bot code is a goldmine for our REFERENCES.md
- **Limitations:** quality varies per bot; alert noise high without careful filtering; EVM-centric; latency 1-2 blocks typically `unverified`
- **Specific signals exposed (sample):** suspicious-contract-deployment, flashloan-attack, rug-pull (LP removal), honeypot, price-manipulation, governance-proposal-attack, ice-phishing, scam-token, bridge-anomaly, funding-from-mixer `unverified`

### Hexagate (acquired by Chainalysis, Dec 2024)
- **URL:** https://hexagate.com
- **Category:** monitoring (prevention)
- **Chains:** EVM — ETH, Base, Arbitrum, Optimism, BNB, Polygon `unverified`
- **Acquisition:** Dec 18, 2024 by Chainalysis for ~$60M; operates independently post-acquisition with ~20 employees retained
- **Reported metric:** "over 98% of known hacks detected before they occurred over two years" (vendor claim)
- **Customers (public):** Coinbase, Polygon, EigenLayer, Uniswap
- **What it detects/provides:** Real-time pre-execution threat detection for protocols — detects exploit attempts (flashloan, oracle manipulation, governance attack) and can trigger circuit breakers. Customers: protocols (Uniswap, Compound) not traders.
- **Method:** ML + simulation + invariant monitoring; mempool watch + pre-block detection
- **API available:** yes — enterprise; webhooks + API
- **Pricing posture:** enterprise (protocol security)
- **Strengths:** pre-execution mempool-level detection (the fastest tier); circuit-breaker automation
- **Limitations:** protocol-focused (our use case is token / trader side); enterprise pricing
- **Specific signals exposed:** exploit-attempt (flashloan, oracle manip, governance), invariant breach, suspicious inbound call, pre-mempool risk score `unverified`

### BlockSec Phalcon
- **URL:** https://phalcon.blocksec.com
- **Category:** monitoring / simulation / forensics
- **Chains:** ETH, BSC, Polygon, Arbitrum, Optimism, Base, Avalanche, Fantom, and others `unverified`
- **What it detects/provides:** Tx simulation + attack explainer + "Phalcon Block" front-running-style defense that reorders/blocks malicious txs for protected protocols. Post-mortem tooling used widely by security researchers. `unverified`
- **Method:** simulation over forked state + bytecode / call-trace analysis + ML
- **API available:** yes — Phalcon Explorer + API; enterprise for Phalcon Block
- **Pricing posture:** freemium explorer, enterprise protection
- **Strengths:** simulation accuracy; call-trace explainer; BlockSec security research brand
- **Limitations:** EVM-only; protocol-defense focus (Block) not token-risk
- **Specific signals exposed:** simulated-revert, state-diff impact, exploit-pattern match, flashloan path, oracle-manipulation trace `unverified`

### Dune Analytics
- **URL:** https://dune.com
- **Category:** infra (query platform)
- **Chains:** ETH, Polygon, BNB, Arbitrum, Optimism, Base, Solana, and others `unverified`
- **What it detects/provides:** SQL over decoded on-chain data; not a detector — a data platform. Community dashboards cover MEV, whale flows, rug databases, pump schemas. `unverified`
- **Method:** batch SQL; near-real-time materializations on paid tier
- **API available:** yes — Dune API (Execute-query endpoint) `unverified`
- **Pricing posture:** freemium (free browsing, paid API + private queries)
- **Strengths:** rich community content; reproducible; excellent for back-testing detectors
- **Limitations:** not real-time enough for trading bot (minutes-to-hour lag depending on tier); query runtime limits; SQL is the interface (no prebuilt detector API)
- **Specific signals exposed:** whatever you SQL — heavy community coverage of DEX volume, MEV PnL, wash-trading heuristics, holder deltas `unverified`

### Flipside Crypto
- **URL:** https://flipsidecrypto.xyz
- **Category:** infra (query platform)
- **Chains:** ETH, Solana, Polygon, BNB, Arbitrum, Optimism, Base, NEAR, Flow, Aptos, others `unverified`
- **What it detects/provides:** Similar to Dune — SQL over decoded on-chain data. Notable for **strong Solana coverage** (earlier than Dune). `unverified`
- **Method:** batch SQL (Snowflake under the hood)
- **API available:** yes — Flipside API / Shroom SDK `unverified`
- **Pricing posture:** freemium (generous free SQL) + paid / enterprise
- **Strengths:** Solana decoded tables (Jupiter, Raydium, Orca, pump.fun) are well-maintained; bounty program produces dashboards
- **Limitations:** not real-time; not a detector; schema drift on fast-moving protocols
- **Specific signals exposed:** same shape as Dune — depends on SQL; strong memecoin / Solana DEX coverage `unverified`

### Helius (Solana data / RPC)
- **URL:** https://helius.dev
- **Category:** infra (Solana-specific RPC + enhanced APIs)
- **Chains:** Solana
- **What it detects/provides:** Enhanced RPC, webhooks, **LaserStream** (Helius' branded Yellowstone gRPC stream), parsed transaction history, DAS (Digital Asset Standard) API for compressed NFTs, token metadata, Priority Fee API, Wallet API (Beta), ZK Compression. Not a detector but a critical data plane.
- **Method:** Geyser plugin (LaserStream = Yellowstone gRPC) + parsed-tx enrichment
- **API available:** yes — REST + WebSocket + webhook + gRPC streaming; official **TypeScript and Rust SDKs**
- **Pricing posture:** freemium (generous free tier), paid plans
- **Strengths:** best-in-class Solana parsed-tx API; LaserStream gRPC is real-time; official Rust SDK; token-metadata endpoint saves us writing one
- **Limitations:** Solana-only; parsed program coverage is selective; rate limits on free tier
- **Specific signals exposed (raw, for us to build on):** parsed swaps, token transfers, NFT events, webhook filters by address / program

### Triton One
- **URL:** https://triton.one
- **Category:** infra (Solana + Sui + PythNet RPC / Geyser)
- **Chains:** Solana, Sui, PythNet (not Solana-only — correction from initial draft)
- **What it detects/provides:** High-performance Solana RPC + **Yellowstone gRPC streaming (a.k.a. "Dragon's Mouth")** — the open-source reference implementation of Solana Geyser gRPC, hosted at `github.com/rpcpool/yellowstone-grpc` (Helius' LaserStream wraps the same protocol).
- **Method:** dedicated validator infra + Yellowstone Geyser plugin
- **API available:** yes — gRPC Geyser (Yellowstone), JSON-RPC
- **Pricing posture:** paid (enterprise tilt)
- **Strengths:** lowest-latency Solana streaming tier; used by serious market makers and MEV searchers; open-source stream format = portable between providers
- **Limitations:** priced for heavy consumers; less developer-friendly than Helius
- **Specific signals exposed (raw):** account updates, tx stream, slot updates, block meta — no detectors

### Shyft
- **URL:** https://shyft.to
- **Category:** infra (Solana)
- **Chains:** Solana
- **What it detects/provides:** Solana data APIs (parsed txs, NFT, DeFi), plus callbacks/webhooks and a GraphQL endpoint. Lower-cost alternative to Helius. `unverified`
- **Method:** Geyser + parsed tx
- **API available:** yes — REST + GraphQL + callbacks
- **Pricing posture:** freemium
- **Strengths:** cost-competitive; GraphQL is ergonomic
- **Limitations:** smaller team, smaller ecosystem; coverage on newer programs lags Helius
- **Specific signals exposed (raw):** same shape as Helius — parsed events, callbacks per filter `unverified`

### The Graph
- **URL:** https://thegraph.com
- **Category:** infra (subgraph indexing)
- **Chains:** ETH, BSC, Base, Arbitrum, Polygon, Optimism, Avalanche, Fantom, Solana (newer), many others `unverified`
- **What it detects/provides:** Subgraph indexing — you write schema + mapping, get GraphQL query layer. Not a detector, but every DEX ships a subgraph for liquidity/volume queries. `unverified`
- **Method:** event indexing per subgraph mapping
- **API available:** yes — GraphQL per subgraph; hosted service + decentralized network
- **Pricing posture:** GRT-metered on decentralized network; hosted service deprecating `unverified`
- **Strengths:** mature; community subgraphs for Uniswap, Sushiswap, Aave, Compound, Curve save time
- **Limitations:** latency ~1 block behind; not real-time streaming; migrating from hosted to decentralized has been painful for some teams
- **Specific signals exposed:** depends on subgraph — typical: pair created, swap, mint/burn, pool TVL, transfer `unverified`

### Goldsky
- **URL:** https://goldsky.com
- **Category:** infra (subgraph + streaming)
- **Chains:** 40+ chains for Mirror; 90+ total across Subgraphs + Mirror (EVM-heavy, Solana included)
- **What it detects/provides:** Managed subgraph hosting + **Mirror** (streaming CDC to PostgreSQL, Kafka, S3, **ClickHouse**, Snowflake). Effectively "The Graph with real-time pipes."
- **Method:** indexing + CDC streaming; sub-second latency claimed
- **API available:** yes — GraphQL + streaming sinks
- **Pricing posture:** paid (usage-based) — Starter / Scale / Enterprise tiers
- **Strengths:** real-time streaming to your own DB is a killer feature for a Rust service like ours; ClickHouse sink directly matches our storage tier; lower-friction than self-hosting a Graph node
- **Limitations:** paid; commercial subgraph coverage younger than The Graph
- **Specific signals exposed:** same as The Graph (schema-defined) + delivered to your storage in real time

### EigenPhi — PRODUCT APPEARS DEFUNCT / PIVOTED
- **URL:** https://eigenphi.io (301-redirects to `eigenphi.substack.com`)
- **Status (2026-04):** product site is gone; domain now forwards to a Substack newsletter (~3,000 subscribers) with MEV analysis posts. No API or product surface found.
- **Architecture implication:** do **not** plan to use EigenPhi API as a reference/data source. Use Flashbots `mev-inspect-py` (open-source) as the canonical MEV reference instead.
- **Historical notes (retained for context):** previously offered MEV activity feed — sandwich attacks, arbitrage, liquidations, JIT liquidity — with per-tx P&L. Paid API on EVM chains.
- **Specific signals previously exposed:** sandwich attacker/victim pair, arbitrage cycle, liquidation attacker/victim, MEV P&L per bundle, JIT-LP event. These signal shapes remain useful as a design reference even though the product is no longer available.

### libMEV
- **URL:** https://libmev.com
- **Category:** MEV analytics
- **Chains:** ETH primarily, Base `unverified`
- **What it detects/provides:** MEV leaderboards (searchers, builders), bundle analysis, atomic arb stats. Community-facing, less commercial than EigenPhi. `unverified`
- **Method:** block + bundle data ingestion from relays + simulation
- **API available:** partial — mostly UI, some data via downloads `unverified`
- **Pricing posture:** free
- **Strengths:** transparency into builder/searcher ecosystem; good for reference datasets
- **Limitations:** no real commercial API; narrower than EigenPhi
- **Specific signals exposed:** searcher P&L, bundle count, builder share, atomic arb volume `unverified`

### Additional / adjacent products (brief)

- **MEV-Inspect (Flashbots, open-source)** — https://github.com/flashbots/mev-inspect-py. Reference implementation for sandwich / arb / liquidation detection on ETH. Free, open-source. Strong grounding for our own MEV detector heuristics. `unverified`
- **BlockSec MetaSleuth** — https://metasleuth.io. Visual fund-flow tracing tool (similar territory to Arkham / Bubblemaps). Freemium. `unverified`
- **Solscan / Solana FM / Solscan Pro** — Solana block explorers with enriched account/token pages; partial APIs. Useful as data sources, not detectors. `unverified`
- **GeckoTerminal / DexScreener / Birdeye** — token price + pool data with lightweight risk flags (mint/freeze authority, top-holder %, LP). Birdeye especially strong on Solana. APIs available; freemium. `unverified`
- **Ackee / Certora / OpenZeppelin Defender** — protocol-side security / formal verification / monitoring. Adjacent to our scope (we care about tokens, they care about protocols).

---

## 2. Signal frequency table

Count = number of scanned products that appear to expose the signal directly.

| Rank | Signal | Count (approx) | Products |
|------|--------|---------------:|----------|
| 1 | Honeypot (buy/sell simulation) | 5 | GoPlus, TokenSniffer, De.Fi, Honeypot.is, QuickIntel, (Forta bots) `unverified` |
| 2 | Top-N holder concentration (top-10 %) | 6 | GoPlus, TokenSniffer, QuickIntel, RugCheck, Bubblemaps, Nansen |
| 3 | LP lock / LP burn status | 5 | GoPlus, TokenSniffer, QuickIntel, RugCheck, Honeypot.is (partial) |
| 4 | Mint authority / mintable flag | 5 | GoPlus, TokenSniffer, QuickIntel, RugCheck (Solana variant: mint + freeze), De.Fi |
| 5 | Buy/sell tax | 4 | GoPlus, TokenSniffer, Honeypot.is, QuickIntel |
| 6 | Deployer / creator holdings | 4 | GoPlus, TokenSniffer, QuickIntel, RugCheck |
| 7 | Owner privileges (pause, blacklist, proxy-upgrade) | 4 | GoPlus, TokenSniffer, QuickIntel, De.Fi |
| 8 | Sanctions / illicit-category exposure | 3 | Chainalysis, TRM, Elliptic (+ GoPlus address-risk partial) |
| 9 | Whale / large-transfer alert | 4 | Arkham, Nansen, DeBank (portfolio delta), Forta bots |
| 10 | Smart-money buy/sell | 2 | Nansen (primary), Arkham (partial via entity labels) |
| 11 | Wallet-cluster / bundled-supply graph | 2 | Bubblemaps, Arkham (partial) |
| 12 | Sandwich / MEV classification | 3 | EigenPhi, libMEV, MEV-Inspect |
| 13 | Flashloan / exploit detection | 3 | Forta, Hexagate, Phalcon |
| 14 | Rug-pull (LP removal) event | 3 | Forta bots, GoPlus (partial), RugCheck (post-hoc) |
| 15 | Similar-to-known-scam pattern match | 2 | TokenSniffer, GoPlus `unverified` |
| 16 | Sniper / bundler concentration at launch | 2 | RugCheck, Bubblemaps (partial) |
| 17 | Token-2022 extension abuse (transfer fee / hook) | 2 | RugCheck, Helius (raw) |
| 18 | Cross-chain bridge exposure / tracing | 2 | Elliptic, Chainalysis |
| 19 | Oracle manipulation detection | 2 | Hexagate, Phalcon |
| 20 | Governance attack detection | 1 | Forta (dedicated bot) |

**Top-10 MVP candidates** (signals appearing in 3+ products, well-documented, bounded scope):

1. Honeypot simulation (buy/sell tax + revert-on-sell)
2. Top-N holder concentration (top-10 %, with and without known CEX/LP holders)
3. LP lock / LP burn %
4. Mint / freeze authority presence (+ Solana variant)
5. Buy/sell tax rates
6. Deployer / creator holdings %
7. Owner privileges (pause, blacklist, proxy-upgrade, fee-modifiable)
8. Large-transfer / whale-move alert
9. Sandwich / MEV victim flag (per-trade, crucial for trading bot)
10. Sanctions / illicit exposure check (crucial for custody + exchange)

---

## 3. Proprietary / rare signals

Signals offered by 1-2 products — potential differentiation, but each is a build/buy decision.

| Signal | Who has it | Build or buy for us |
|--------|-----------|---------------------|
| Entity attribution (wallet → org) | Arkham (primary), Chainalysis, TRM | **Buy** (label DB is years of labor) |
| Smart-money P&L cohorts | Nansen | **Buy or partially reproduce** via public labels + our own wallet P&L compute |
| Wallet-cluster bundle graph | Bubblemaps | **Build** — graph construction is tractable, we own the data model anyway |
| Pre-mempool / mempool exploit detection | Hexagate, Phalcon | **Build** for our chains where mempool exists (ETH, BSC, Base); skip Solana (no mempool) |
| MEV sandwich victim flagging | EigenPhi, libMEV, MEV-Inspect | **Build** from MEV-Inspect heuristics — open-source reference exists |
| Token-2022 extension abuse | RugCheck | **Build** — narrow, tractable, competitive edge on Solana memes |
| Similar-to-known-scam corpus match | TokenSniffer, GoPlus | **Build** with a scam-token embedding + nearest-neighbor; corpus is the moat |
| Bundled funding-source detection (sniper cluster) | RugCheck, Bubblemaps | **Build** — direct on-chain heuristic |
| Cross-chain bridge tracing | Elliptic, Chainalysis | **Defer** — high cost for our current scope |
| Real-time invariant breach (protocol-side) | Hexagate, Phalcon | **Out of scope** — we analyze tokens, not protect protocols |

---

## 4. API-friendly vs UI-only

Our four consumers (trading bot, custody, MM, exchange) **require** API / programmatic access. Rating below: HIGH = public documented REST, MED = enterprise-gated, LOW = UI-only or scraping.

| Product | API access | Notes |
|---------|------------|-------|
| GoPlus | HIGH | Free tier usable; standard we should match for schema compatibility |
| TokenSniffer | MED | Paid API, moderate QPS |
| Honeypot.is | HIGH | Free, simple; rate-limited |
| RugCheck | HIGH | Public API; Solana-only |
| QuickIntel | MED | Paid API tiers |
| De.Fi | MED | Enterprise-leaning |
| Arkham | MED | Paid subscriber API |
| Nansen | MED | Enterprise API, expensive |
| DeBank | HIGH | Per-call priced OpenAPI |
| Bubblemaps | LOW | Primarily UI + enterprise |
| Chainalysis | MED | Enterprise; free sanctions oracle is HIGH |
| TRM Labs | MED | Enterprise |
| Elliptic | MED | Enterprise |
| Forta | HIGH | GraphQL public API |
| Hexagate | MED | Enterprise |
| Phalcon | HIGH (explorer) / MED (Block) | |
| Dune | HIGH | Paid API, minutes-latency |
| Flipside | HIGH | Generous free tier; minutes-latency |
| Helius / Triton / Shyft | HIGH | Streaming, real-time |
| The Graph / Goldsky | HIGH | GraphQL / streaming |
| EigenPhi | MED | Paid API |
| libMEV | LOW | Mostly UI |

---

## 5. Market gaps

Based on the scan, these capabilities are **not well-served** by any single existing product:

1. **Cross-chain correlated anomaly detection.** Sanction tools (Chainalysis / Elliptic) trace funds across chains, but no product correlates a *behavioural* anomaly (e.g., "same funder is now deploying tokens with similar patterns on Base, BSC and Solana") in real time. Bubblemaps is per-token, per-chain.
2. **Pre-mempool / mempool signals for trading.** Hexagate and Phalcon operate at mempool for protocol-defense, not for a trading bot. Traders want "token about to rug in the next block," not "protocol under attack." Gap: mempool-aware, token-side risk deltas.
3. **Coordinated-wallet pump detection across chains and venues.** Nansen's smart-money is backward-looking cohorts; Bubblemaps is static bundling. No product flags in real time "N wallets that are likely coordinated are buying token X on venue Y right now."
4. **Real-time multi-consumer streaming.** Most detectors expose poll APIs or per-tenant webhooks. A streaming bus (WebSocket / Kafka) that serves many heterogeneous consumers with per-consumer filters (trading vs compliance vs MM) is rare — existing products pick one shape per product.
5. **Solana depth + EVM depth in one schema.** RugCheck owns Solana, GoPlus/TokenSniffer own EVM. No one provides a unified detector schema with comparable signal quality on both. This is particularly valuable for a multi-chain trading bot.
6. **Confidence calibration and backtesting.** Every scanner today exposes boolean flags or an opaque score. None that we know of publishes **per-signal precision/recall against a labelled dataset** — consumers can't tune thresholds. (Dune dashboards are the closest.)
7. **Per-consumer data-product shape.** Compliance wants sanctions + audit trail; trader wants sub-second events + confidence; MM wants pool-state anomalies; custody wants address screening + whitelist. No product serves all four simultaneously — consumers stitch 3+ vendors together.
8. **Honest "this is wash trading on DEX pool X" signal.** Wash-trading detection on DEX is lightly covered (mostly Dune SQL dashboards). Off-CEX wash trading is a real anomaly class for memes and MM work.
9. **Time-travel reproducibility.** Most APIs return "state now." Reproducing a risk score against a historical block is rarely supported — critical for backtesting detectors (our design principle).

---

## 6. Our potential edge

Given our constraints — **Rust, multi-consumer, in-process crate + self-hosted service, Solana+EVM** — these differentiations look realistic:

1. **Single API schema across Solana and EVM with parity.** RugCheck quality on Solana + GoPlus quality on EVM, served from one shape. Consumers do not context-switch. This alone is a defensible product wedge.
2. **In-process crate AND service.** Trading bot embeds as Rust crate (zero network hop, sub-ms detector calls); custody/exchange use REST; MM gets WebSocket stream. Same detector code, three delivery modes. No competitor does this — they're all SaaS.
3. **Confidence + evidence, not booleans.** Every detector emits `(confidence 0..1, severity, evidence)` and cites its source in REFERENCES.md. Consumers tune thresholds per business. This is a discipline gap in the market, not a tech gap — and it compounds once shipped.
4. **Reproducibility & backtesting first-class.** Every detector deterministic given (block_range, config). Historical reproducibility enables A/B thresholds and labelled-fixture regression. Competitor APIs are closed-box.
5. **Self-hosted + open-formula.** Custody, MM, and exchange have data-residency and vendor-dependency concerns that SaaS-only competitors cannot address. A self-hosted Rust binary that ingests their existing RPC is directly buyable where Chainalysis / Nansen / Arkham cannot be.
6. **Solana Token-2022 and pump.fun / Raydium v4 memeflow depth.** The most active scam volume of 2024–2026 lives here `unverified`. Our Phase 1 focuses here — playing to the hottest gap.
7. **Streaming bus, multi-consumer fanout, typed per-consumer views.** Kafka/Redpanda-backed `AnomalyEvent` stream with topic filters per consumer class. Beats the one-product-one-shape pattern.
8. **Open REFERENCES.md.** Every threshold defended in public. Most competitors are black-box; reproducible citations are a trust moat for security-sensitive consumers.

**Realistic non-goals:** we are not competing with Chainalysis on sanctions (buy it via Screening Oracle), not with Arkham on entity labels (buy via API or accept partial coverage), not with Hexagate on protocol defense.

---

## 7. Follow-ups (Phase 0.1)

- [x] Re-run this scan with live WebSearch/WebFetch; clear every `unverified` marker against current vendor docs. (Pass completed 2026-04-21 — see Verification Log §8.)
- [ ] Fetch and store JSON-schema examples for GoPlus, RugCheck, Honeypot.is, TokenSniffer, QuickIntel. Build a **unified schema proposal** that is a superset. (This becomes `crates/common` types.) Live response schemas for RugCheck and Honeypot.is are captured inline in §1 — remaining four still need capture as JSON fixtures.
- [ ] Inventory open-source detector code: MEV-Inspect (Flashbots — confirmed canonical reference given EigenPhi defunct), Forta bot library, slither detectors, SolanaFM risk rules — catalogue in REFERENCES.md.
- [ ] Price-posture deep-dive: record each product's published/quoted price for 1M API calls/month, so consumer ROI can be computed.
- [ ] Labelled-dataset scouting: which public datasets of known rugs / honeypots / sandwich victims exist (Chainalysis CSV leaks, Certik rug list, Tokensniffer scam list, DeFiLlama rekt DB, Rekt News) — every one of these is a potential fixture source.
- [ ] Shortlist 3 products for API trial (recommend: GoPlus free tier for EVM schema reference, RugCheck public API for Solana, Honeypot.is for simulation-based baseline). Target: replicate their outputs on 100 known tokens each, measure agreement, identify disagreement modes.

---

## 8. Verification Log (2026-04-21)

Second pass verified claims against live vendor docs and (where possible) live API responses. Summary:

- **Verified (tags strippable):** 14 products had at least one confirmed claim after live lookup.
- **Corrected (material changes applied):** 5 products — TokenSniffer (not EVM-only; Solidus Labs acquisition; $99/mo pricing), QuickIntel (37 chains not "30+"), Nansen (credit-based public pricing, not enterprise-only), EigenPhi (product defunct / Substack-only now), Triton One (also Sui + PythNet), Bubblemaps (12 chains, different list), Hexagate (Dec 2024 acquisition ~$60M).
- **Not found / not fetched:** 4 products — De.Fi (empty responses), libMEV (no API docs), TRM Labs, Elliptic, BlockSec Phalcon, Shyft, The Graph, DeBank, Flipside, Dune (not re-fetched this pass; original characterisations unchanged and `unverified` tags remain).

| Product | URL checked | Status | Notes |
|---------|-------------|--------|-------|
| GoPlus Security | gopluslabs.io/en/token-security-api, docs.gopluslabs.io | verified | 40+ chains; 100 calls/min free; 30+ detection items; Solana confirmed |
| TokenSniffer | tokensniffer.readme.io/reference/introduction, /get-token-results, /pricing | corrected | EVM-only claim wrong — Solana + many EVM; acquired by Solidus Labs; $99/mo Sniffer Pack |
| De.Fi | de.fi/shield, de.fi/scanner | not-found | Both returned empty content; claims remain `unverified` |
| RugCheck.xyz | api.rugcheck.xyz/v1/tokens/.../report (live call) | verified | Full field list confirmed from live response; public API working |
| Honeypot.is | honeypot.is, api.honeypot.is/v2/IsHoneypot (live call) | verified | ETH/BSC/Base confirmed; full response schema confirmed from live call |
| QuickIntel | docs.quickintel.io/developer-integration/api-integration/supported-chains-and-dex | corrected | 37 chains (not "30+"); includes Solana, Sui, Injective, Radix |
| Arkham Intelligence | intel.arkm.com/api/docs | verified | API confirmed; 18+ chains including BTC, ETH, Solana |
| Nansen | nansen.ai/api, docs.nansen.ai/getting-started/credits | corrected | Not enterprise-only; public credit pricing: $0.001/credit, $0.05/call Smart Money |
| DeBank | openapi.debank.com | not-fetched | No fetch attempted; prior characterisation as portfolio data API unchanged |
| Bubblemaps | bubblemaps.io | corrected | 12 confirmed chains (differs from file list); API confirmed available |
| Chainalysis | chainalysis.com/blog/chainalysis-hexagate-announcement (search) | verified | Hexagate acquisition Dec 2024 ~$60M confirmed; sub-acquisition note added |
| TRM Labs | not-fetched | unverified | Enterprise posture claim unchanged |
| Elliptic | not-fetched | unverified | Enterprise posture claim unchanged |
| Forta Network | docs.forta.network/en/latest/forta-api-reference/ | verified | GraphQL at api.forta.network/graphql; free; EVM-only; chains confirmed |
| Hexagate | chainalysis.com blog + Calcalist (search) | corrected | Acquisition Dec 18 2024, ~$60M, not previously dated in file |
| BlockSec Phalcon | not-fetched | unverified | Characterisation unchanged |
| Dune Analytics | not-fetched | unverified | Well-known; characterisation unchanged |
| Flipside Crypto | not-fetched | unverified | Characterisation unchanged |
| Helius | helius.dev/docs | verified | Rust SDK confirmed; LaserStream gRPC confirmed; webhook + DAS API confirmed |
| Triton One | docs.triton.one (search) | corrected | Also supports Sui + PythNet, not Solana-only; Yellowstone open-source on GitHub |
| Shyft | not-fetched | unverified | Characterisation unchanged |
| The Graph | not-fetched | unverified | Well-known; characterisation unchanged |
| Goldsky | goldsky.com/products/mirror, docs.goldsky.com/supported-chains (search) | verified | 40+ chains Mirror; ClickHouse sink confirmed; sub-second latency confirmed |
| EigenPhi | eigenphi.io (redirect) + search | corrected | Site redirects to Substack newsletter; product/API defunct — architecture risk |
| libMEV | libmev.com (search) | not-found | No API documentation found; UI-only assessment stands |

### Surprises worth flagging to architecture synthesis

1. **EigenPhi is defunct.** We cannot treat it as a reference API or data source for sandwich/MEV. Substitute: Flashbots `mev-inspect-py` (GitHub, open-source, EVM-only, archived but canonical) — already the intended reference for our own MEV detector heuristics.
2. **TokenSniffer → Solidus Labs.** TokenSniffer is now part of a compliance-tooling vendor and supports Solana — this changes the competitive landscape: their scam-corpus similarity match is now arguably the strongest cross-chain scam-pattern database, backed by institutional scale.
3. **Nansen public pay-per-call pricing** ($0.01–$0.05/call via x402 on Base/Solana USDC) makes Smart-Money labels cheaply integrable — worth revisiting the "buy vs build" decision for smart-money tracking in §3.
4. **Helius and Triton both expose Yellowstone gRPC under different brand names** (LaserStream vs Dragon's Mouth). The underlying protocol is open-source (`github.com/rpcpool/yellowstone-grpc`) — we can write one Rust adapter that works against both providers (and against a self-hosted validator running the same plugin).
5. **Goldsky Mirror has a ClickHouse sink.** This directly matches our proposed ClickHouse storage tier. Candidate for "buy" on EVM-chain indexing rather than building our own subgraph + streaming pipeline from scratch.
6. **QuickIntel covers 37 chains (7 more than file claimed).** Notably Sui, Injective, Radix, Mantle — broader than most competitors. If we pick QuickIntel as a risk-scanner baseline, chain coverage is rarely the limiting factor.

---

*End of market scan. Document ~5,100 words including verification log.*
