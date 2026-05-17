# Security Policy

## Reporting a Vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

Report privately through GitHub's
[private vulnerability reporting](https://github.com/MG-Trading-Terminal/mg-onchain-analysis/security/advisories/new)
(the **Security** tab → *Report a vulnerability*).

Please include:

- A description of the vulnerability and its impact.
- Reproduction steps or a proof of concept.
- Affected component (crate, binary, or endpoint).

You can expect an acknowledgement within **48 hours** and a remediation timeline
within **7 days**.

## Threat Surface

MG Onchain Analysis does **not** custody funds or hold private keys. Its security
boundary is about *trustworthy verdicts* and *service integrity*:

- **Untrusted input.** All on-chain data (blocks, transactions, account state) is
  attacker-controlled. Decoders and detectors must not panic, over-allocate, or
  enter unbounded loops on malformed input.
- **Detector integrity.** A manipulated verdict is a security issue — a suppressed
  rug-pull signal can cause direct financial loss to a consumer system. Reports of
  detector evasion or bypass techniques are in scope.
- **API exposure.** The `gateway` REST/WebSocket surface must validate input at the
  boundary and enforce rate limits.
- **RPC credentials.** Node endpoints and tokens are supplied via environment
  variables and must never be logged or committed.

## What Is Not a Vulnerability

- Detector false positives or false negatives that are within the documented
  confidence model (file a normal issue instead).
- Findings that require a compromised RPC provider or operator host.

## Secure Configuration

- Never commit real credentials. `config/*.toml` files ship with placeholder
  localhost defaults; production values are supplied via environment variables.
- Run the service behind a trusted network boundary; the gateway is not hardened
  for direct public internet exposure.
