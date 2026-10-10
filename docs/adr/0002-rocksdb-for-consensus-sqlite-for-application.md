# ADR 0002 — RocksDB for consensus state, SQLite for application state

Status: accepted. Date: 2026-09-18.

## Context

This system stores two kinds of data that look similar and behave nothing
alike.

Consensus state is what the state root commits to: accounts and balances,
positions, market definitions, order books, blocks, quorum certificates and the
merkle trie nodes underneath them. It is written once per block by exactly one
process, read by key, and iterated in byte order. Iteration order is not a
convenience here — the trie's structure and therefore the state root depend on
it. Nobody ever asks it a relational question. Nobody ever migrates its schema;
a change to the encoding is a chain restart.

Application state is everything the exchange needs that the chain does not care
about: user records, sessions, faucet claim history, candle aggregates,
leaderboard snapshots, audit trails. It is written by a web service, queried by
time range and by foreign key, and it changes shape as the product changes, so
it needs migrations. It is not consensus-critical: losing it would be painful,
but it would not fork anything.

Using one engine for both means either running relational queries against a
key-value store by hand, or putting the state root's iteration order at the
mercy of a query planner.

## Decision

Consensus state lives in RocksDB, inside the node process, in a single data
directory with 21 column families (`accounts`, `orders`, `book_index`,
`markets`, `blocks`, `block_by_hash`, `meta`, `mempool`, `trie_nodes`,
`validators`, `qc`, `mirror_blocks`, `positions`, `margin_accounts`,
`funding_indices`, `perp_markets`, `futures_markets`, `oracle_state`,
`liquidation_queue`, `perp_orders`, `perp_book_index`). Column families are
tuned individually: LZ4 on the hot small-value families, ZSTD on the large
ones, bloom filters on the point-lookup families.

Application state lives in SQLite, inside the gateway process, with versioned
migrations.

Neither is reachable by name from business logic. RocksDB sits behind
`cb_ports::KvStore`, whose documentation states the ordering requirement
explicitly: `scan_prefix` must yield ascending lexicographic byte order, and an
implementation that does not will fork the chain. SQLite sits behind
`cb_ports::AppDatabase` and its per-aggregate repository traits.

The two stores are in different processes on purpose. A single transaction can
never span both.

## Consequences

Each engine does the job it is good at. The trie gets ordered iteration and
batched atomic writes; the gateway gets `WHERE`, `ORDER BY`, `JOIN` and a
migration story.

The port boundary is what makes the choice reversible. Swapping SQLite for
PostgreSQL is an implementation of `AppDatabase` and one line in a composition
root. Nothing above the port knows which one it got.

`cb-ports` documents what may *not* move across that line: balances, orders and
positions are consensus state and are read through the node's RPC. A repository
method returning them would create a second source of truth, and the two would
drift.

The costs:

**No transaction spans the two stores.** A faucet claim writes a ledger row in
SQLite and submits a `Transfer` to the chain. A crash between the two leaves
them disagreeing. The gateway therefore has to be written for it — idempotent
claim ids, reconciliation on restart — and that is ongoing work, not a solved
problem.

**Two things to operate.** Two backup procedures, two restore procedures, two
sets of disk-full failure modes, two lock files to be confused by.

**SQLite has one writer.** It is right for a single gateway process and wrong
the moment someone wants two of them behind a load balancer. That is the day
the PostgreSQL implementation has to be written, and the port only makes it
cheap, not free.

**RocksDB state grows without bound.** The merkle trie never deletes superseded
nodes: `trie_nodes` grows monotonically, and there is no garbage collection.
The `--pruning` flag prunes *block bodies* only; accounts, trie nodes and meta
are never pruned. A long-running chain will fill a disk, and the fix is a new
genesis.

**One process per data directory.** RocksDB takes an exclusive lock. Two nodes
pointed at the same `--data-dir` is a common operator mistake, and the error
message it produces is about a lock file, not about what the operator did
wrong. See the runbook.
