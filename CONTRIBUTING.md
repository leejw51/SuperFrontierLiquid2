# Contributing

This is a personal research prototype, shared for learning and local
experiments. Contributions should preserve that scope and use disposable test
data. There is no company sponsorship or exchange affiliation. Hyperliquid is
an architectural reference in the design notes.

## Getting set up

You need a Rust toolchain (`rust-toolchain.toml` pins stable and adds the
`wasm32-unknown-unknown` target), the Cap'n Proto compiler `capnp`, Python 3,
and `curl`. For the frontend, `cargo install trunk`. For the three-node cluster,
`pip3 install supervisor`.

```bash
cd rust
make build          # release cb-node
make test           # unit tests
make               # the menu; every target has a help line
```

The first release build takes several minutes. RocksDB and the Cap'n Proto
codegen dominate it, and it is a cold-cache cost only.

## Build, test, format, lint

From the repository root, the two you need before every pull request:

```bash
make format         # rustfmt over the workspace and cb-web
make ci             # format-check, clippy -D warnings, cargo test --workspace:
                    # exactly what .github/workflows/ci.yml runs
```

The finer-grained targets live in `rust/Makefile`:

```bash
make check          # cargo check --workspace --all-targets
make test           # cargo test --workspace --lib --bins
make integrationtest # cargo test --tests, then the multi-node sync test
make testall        # everything, including the end-to-end scripts
make fmt            # cargo fmt --all, workspace and cb-web
make fmt-check      # the same, read-only — what CI should run
make lint           # cargo clippy --workspace --all-targets -- -D warnings
```

`cb-web` is **outside** the Cargo workspace: it needs its own release profile and
its own lockfile. `cargo test --workspace` does not see it. `make fmt` and
`make fmt-check` cover it explicitly; build it with `make web-build`.

`make testall` runs the cargo suites in parallel, and RocksDB opens a lot of
files. If it fails with "Too many open files", raise `ulimit -n` or use
`make testallslow`, which runs serially.

Tests that need a chain start and stop their own node. If a run is interrupted
and leaves processes behind, `make stop` clears them and `make clean` also wipes
the data directories.

## Commits

Conventional commits, lowercase, imperative mood, with a scope when there is an
obvious one:

```
feat(ports): define the replaceability seam for every external dependency
build(web): prove the Rust to WebAssembly frontend chain end to end
docs: add build plan and frozen gateway API contract
fix(engine): stop funding accrual double-counting at an interval boundary
```

Common types: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `build`,
`chore`. The scope is usually a crate name without the `cb-` prefix.

The subject line says what the commit does, not what you did. The body is for
why, and for anything a reviewer would otherwise have to reconstruct.

## What a good pull request looks like

One concern per pull request. A refactor and a behaviour change in the same
diff is two pull requests.

The description says what changed and why, and names the invariant you thought
hardest about. If you changed something a reviewer would not think to check —
an encoding, an ordinal, an ordering rule — say so in the first paragraph.

New behaviour comes with a test that fails without the change. Bug fixes come
with a regression test. If the change is in `cb-engine`, the test is a pure unit
test or a proptest, because nothing in that crate needs a database.

Documentation changes in the same commit as the code. `docs/api-contract.md` is
frozen: changing it requires the code change that implements it in the same
commit, and both owners told. A decision worth arguing about gets an ADR in
`docs/adr/` — see the README there.

Before you open it:

```bash
make fmt-check && make lint && make test
```

## Invariants a change must not break

These are the things that do not fail loudly at the point of the mistake. Each
one has bitten a system like this before.

**Consensus enums are append-only.** `AssetId` and `MarketId` in `cb-types` are
closed enums whose declaration order is load-bearing three times over: `bincode`
encodes a variant as its index, so reordering changes the signing hash of every
historical transaction; `as_u8()` keys the balance maps inside the accounts
trie, so renumbering moves somebody's balance to a different asset; and it
prefixes the order-book column-family keys. Append at the end. Never reorder,
never renumber, never remove. The Cap'n Proto enums in `cb-schema` mirror the
same ordinals, and tests pin the correspondence — if one of those tests fails,
it is not the test that is wrong.

**`ASSET_SPECS` is the only place decimals live.** A hard-coded decimal
somewhere else is a silent hundred-fold error in a balance.

**Execution is deterministic.** Two nodes replaying the same blocks must reach
the same state root. That means: no wall-clock time, no random numbers, no
environment reads, and no iteration over a `HashMap` whose order can reach the
output. Order transactions only through `order_for_block`. Derive identifiers
from `(block height, transaction index, intra-transaction counter)`, never from
a counter in memory. If you need a timestamp, it is a field on the block header,
set once by the proposer.

**`cb-engine` stays pure.** Its dependencies are `cb-types` and `thiserror`.
Adding an I/O crate, an async runtime or a clock to that manifest is the change
that quietly ends replica agreement — the reviewer should reject it on the
manifest diff alone.

**`KvStore::scan_prefix` returns ascending byte order.** The trie's structure,
and therefore the state root, depend on it. An implementation that iterates in
another order forks the chain.

**The state root preimage is a consensus format.** It is
`keccak256(accounts_root || positions_root || markets_root)`. Adding a fourth
trie, reordering the three, or changing a trie's node encoding makes every
existing database incompatible, with no migration path. That may be the right
call; it is never an incidental one.

**The signed transaction preimage is `bincode`.** Cap'n Proto is the wire
format and nothing else. Changing how a `Transaction` serialises changes every
sighash.

**Nothing above `cb-ports` names a concrete backend, and `cb-ports` names none
at all.** `cb-gateway` depends on `AppDatabase`, never on SQLite. If a type in
`cb-ports` needs something from a backend, the abstraction is wrong. Wiring
happens once, in a binary's composition root.

**Consensus state has one home.** Balances, orders and positions live on chain
and are read through the node's RPC. A repository method in `cb-store-sqlite`
that returns one of them creates a second source of truth, and the two will
drift.

**The exchange's accounting invariants.** The simulation asserts these
continuously, and a change that breaks one is a bug even if every test passes:
total units of each asset are conserved except at explicit mint and burn points;
no balance underflows; an account's locked balance equals its margin plus its
resting-order collateral; best bid is strictly below best ask at every block
boundary; long and short open interest are equal on every derivative market;
funding transfers sum to zero per market per tick net of the insurance share;
and a dated future at expiry closes every position and leaves zero open
interest.

## Where things are

`docs/architecture.md` explains the mechanism end to end. `docs/runbook.md` is
the operator's guide. `docs/PLAN.md` holds the plan and the milestones.
`docs/adr/` records the decisions and what they cost. The crate map is in the
README.

## Reporting bugs

Say what you ran, what you expected and what happened, and include the
`state_root` and `head_height` from `/api/status` if a chain was involved. A
determinism bug — two nodes on the same binary and the same genesis reaching
different state roots — is the highest-severity report this project can
receive; include both roots and the height.

## Licence

Dual MIT / Apache-2.0. Contributions are accepted under the same terms.
