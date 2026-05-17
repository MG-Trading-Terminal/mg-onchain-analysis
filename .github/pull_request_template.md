## Summary

<!-- 1-3 sentences: what changed and why -->

## Changes

- [ ] New detector / signal
- [ ] New feature
- [ ] Bug fix
- [ ] Refactor
- [ ] Documentation
- [ ] Dependencies

## Detector Checklist

<!-- Required for changes that add or modify a detector -->

- [ ] Source cited in `REFERENCES.md`
- [ ] Thresholds live in `config/detectors.toml` with rationale comments — no magic numbers
- [ ] Known-positive and known-negative fixtures added under `tests/fixtures/`
- [ ] Emits `AnomalyEvent` with a confidence score (not a boolean)
- [ ] Output is deterministic for a given block-range input

## Code Quality

- [ ] No `f64` for prices, amounts, supplies, or liquidity (`Decimal` / `U256` / `u128`)
- [ ] Addresses normalized to chain-canonical form at the boundary
- [ ] No new `unwrap()` in production paths
- [ ] `cargo fmt` and `cargo clippy` clean

## Testing

- [ ] `cargo test --workspace` passes
- [ ] New tests added for changed behaviour

## Changelog

<!-- Add a dated, categorized entry to CHANGELOG.md -->
