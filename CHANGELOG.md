# Changelog

All notable changes to Lumora / StellarAid contracts are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
How versions are stored, queried, and constrained is defined in [docs/VERSIONING.md](docs/VERSIONING.md).

## [Unreleased]

### Fixed

- **#790** `platform_config::ConfigError`: `AddressNotRegistered` and `InvalidTier` both had
  discriminant `6`. Renumbered to unique values 6–10 (`AddressNotRegistered=6`, `InvalidTier=7`,
  `InvalidPromotion=8`, `InvalidReferralBps=9`, `PromotionNotActive=10`). Callers who matched on
  numeric error codes must update for `InvalidTier` (7 → was 6) and `InvalidPromotion` (8 → was 7),
  `InvalidReferralBps` (9 → was 8), and `PromotionNotActive` (10 → was 9).

- **#992** `search::search`: Replaced O(m log m) full-match-set sort with a bounded top-k selection
  (`select_page`). Cost is now O(m · k) where k = `start + page_size` (at most 2 × `MAX_PAGE_SIZE`).
  Output is identical to the previous implementation for the same input, including tie-breaking.

- **Workflows** `security.yml`: `rustsec/audit-check@v2` and `EmbarkStudios/cargo-deny-action@v2`
  were being used as install steps but each ran a full audit/check internally, then the explicit
  `run:` step ran the same command again. Replaced both with `taiki-e/install-action@v2` (install
  only) so each tool runs exactly once via the explicit step. Fixed concurrency group key to include
  `github.workflow` (matching the pattern in `ci.yml`/`typescript-ci.yml`).

### Added

- **#990** Paginated list-getter variants for seven contracts that returned unbounded `Vec`s. Each
  new `*_page(start, limit)` function routes through `shared::pagination::paginated` and returns
  `(Vec<T>, shared::PageInfo)`:
  - `competitions::get_history_page`
  - `creator_fund::get_allocations_page`, `get_growth_page`
  - `revenue_sharing::get_history_page`
  - `subscription::get_payments_page`
  - `verification::get_history_page`, `get_artist_badge_types_page`, `get_badge_history_page`
  - `messaging::get_messages` now uses `shared::pagination::clamp_limit` instead of a manual
    `.min(MAX_HISTORY)`.
  Existing signatures are unchanged; callers relying on unbounded results should migrate to the
  `*_page` variants.

### Notes

- **#991** `platform_config::compute_fees` caching: this issue is currently blocked because
  `platform_config` does not compile (pre-existing `lib.rs` parse errors noted in the `ci.yml`
  BASELINE block). No changes were made. The issue can be addressed once the workspace compile
  blockers are resolved.

## [0.1.0] — 2026-08-27

Initial documented baseline for every workspace contract (`shared`, `platform_config`, `escrow`, `commission_agreement`, `dispute_arbiter`, `messaging`, `subscription`, `competitions`, `verification`, `revenue_sharing`, `recruitment`, `creator_fund`, `campaign`, `donation`, `withdrawal`).

### Added

- Semantic version metadata in each `contracts/*/Cargo.toml` (`[package.metadata.stellar-aid]`).
- On-chain `get_version`, `get_version_metadata`, and `is_version_compatible` on all contracts (#682).
- `shared::version` constraint helpers and `CURRENT_STORAGE_SCHEMA`.
- Maintenance window, pause, backup, upgrade, and communication procedures (#681).

[Unreleased]: https://github.com/EDOHWARES/stellarAid-contract/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/EDOHWARES/stellarAid-contract/releases/tag/v0.1.0
