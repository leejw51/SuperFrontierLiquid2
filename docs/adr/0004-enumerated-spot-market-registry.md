# ADR 0004 — An enumerated, not dynamic, spot-market registry

Status: accepted. Date: 2026-09-18.

## Context

A spot market's identity is load-bearing in three independent places, and all
three key off the *declaration order* of a Rust enum.

A transaction is signed over its `bincode` encoding, and `bincode` writes an
enum as its zero-based variant index. Reordering `AssetId` would change the
signing hash of every historical transfer.

An account's balances are a map keyed by `AssetId::as_u8()`, `bincode`-encoded
into the accounts trie. Renumbering an asset silently moves somebody's balance
to a different asset.

Order-book keys are built from `MarketId::as_u8()`, widened to eight big-endian
bytes as the `book_index` column-family key prefix.

So the question "should markets be listable at runtime?" is really the question
"should market identity be data on the consensus path?". A dynamic registry
means a listing transaction, a rule for who may send it, an ordering rule for
concurrent listings, and a migration for the key layout when identifiers become
variable-length.

Derivatives already answer this differently. A `DerivId` is four bytes —
kind, a little-endian index, and a reserved byte — so perpetuals and dated
futures are listed from a genesis file without any of the above.

## Decision

`AssetId` and `MarketId` in `cb-types` stay closed enums. They are append-only:
never reorder, never renumber, never remove. A single authoritative table,
`ASSET_SPECS`, is the only place an asset's decimals, class and display symbol
are written down, indexed by the same ordinal.

The Cap'n Proto enums in `cb-schema` mirror the ordinals one for one, and tests
(`variant_indices_match_as_u8`, plus an ordinal test in `cb-rpc::conv`) fail CI
if an insertion breaks the correspondence.

Derivative markets keep their genesis-driven registry. The asymmetry is
deliberate: spot identity is baked into key layouts and sighashes; derivative
identity is not.

## Consequences

There is no listing transaction to design, no listing authority to gate, and no
ordering question when two listings race. The set of markets on a chain is
knowable by reading one file.

Wrong identifiers become compile-time or CI failures rather than silent fund
movement. A new asset added in the wrong position trips a test instead of
crediting someone else's balance.

`ASSET_SPECS` gives the decimals problem exactly one home, which matters because
the decimals genuinely differ: BTC 8, ETH 18, USD 6, SOL 9, the synthetic
equities 6.

The costs:

**Listing a market is a software release.** Adding a spot pair means editing an
enum, rebuilding, shipping binaries to every node and, in practice, a new
genesis. It cannot be done by an operator, let alone by a third party, and it
cannot be done quickly.

**The enum is a wide blast radius.** Every exhaustive `match` on `AssetId` or
`MarketId` anywhere in the workspace must be updated when a variant is appended.
This is not hypothetical: extending the universe to SOL and four synthetic
equities left non-exhaustive matches in `cb-dashboard` and `cb-sim`, which broke
the workspace build until they were filled in. The type system catches it — that
is the point — but "append a variant" is a change that touches many crates.

**Two registries to learn.** A newcomer reasonably expects markets to be listed
one way. Here, spot is compile-time and derivatives are genesis-time, and the
reason is invisible until you know how the book keys are built.

**Some scale assumptions rode along with the enum.** Because the listed set was
small and BTC-shaped, notional maths hard-codes divisors — the perpetual
notional formula assumes an 8-decimal underlying, and the spot notional formula
divides by 1e8 without consulting `AssetId::decimals()`. Those are separate bugs
waiting for a non-8-decimal underlying to be traded, and a closed registry made
them easy to leave in place.
