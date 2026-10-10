@0xa1b2c3d4e5f60001;

struct H256 {
  bytes @0 :Data;
}

struct Address {
  bytes @0 :Data;
}

struct U128 {
  hi @0 :UInt64;
  lo @1 :UInt64;
}

# Sign-magnitude i128. `sign` is one of -1, 0, +1; when `sign == 0` the magnitude must be 0.
# This is the canonical on-wire form for any signed value persisted on chain.
struct SignedI128 {
  sign @0 :Int8;
  abs  @1 :U128;
}

# Asset universe. Cap'n Proto enums evolve ADDITIVELY ONLY: new enumerants are
# appended with fresh ordinals and existing ones are never reordered or
# renumbered. These ordinals are consensus-load-bearing — they are the
# `AssetId::as_u8()` values used as `AccountState.balances` map keys and as the
# bincode variant indices that every transaction signature commits to. Renumber
# one and every historical signature and state root becomes unverifiable.
#
# btc/eth/usd are the genesis-era assets. sol + the four equities were appended
# when the tradable universe widened; see `cb-types::ids::ASSET_SPECS` for the
# authoritative decimals and classification of each.
enum AssetId {
  btc  @0;
  eth  @1;
  usd  @2;
  sol  @3;
  aapl @4;
  tsla @5;
  nvda @6;
  spy  @7;
}

# Spot markets. Same additive-only rule as `AssetId`. Every listed spot market
# is quoted in `usd`; the enum stays closed by design (see `docs/PLAN.md` §7 —
# a dynamic spot registry is a consensus-touching follow-on). Derivatives do
# NOT appear here; they grow freely via `DerivId`.
enum MarketId {
  btcUsd  @0;
  ethUsd  @1;
  solUsd  @2;
  aaplUsd @3;
  tslaUsd @4;
  nvdaUsd @5;
  spyUsd  @6;
}
enum Side { buy @0; sell @1; }
enum Tif { gtc @0; ioc @1; fok @2; }

# Discriminator for `DerivId`. Spot markets stay on `MarketId`.
enum MarketKind { perp @0; future @1; }

# 4-byte derivative-market identifier. See `cb-types::DerivId` for the canonical
# byte layout — `[kind: u8][index_lo: u8][index_hi: u8][reserved: u8 = 0]`.
# We expose it on the wire as a 4-byte blob to keep canonical encoding boring.
struct DerivId {
  bytes @0 :Data;
}
