# ADR 0001 — Cap'n Proto RPC for node communication

Status: accepted. Date: 2026-09-18.

## Context

Three kinds of traffic reach a node. Clients submit signed transactions and
read account, market and book state. Replicas stream every block the sequencer
produces and send votes back. Operators and tools read chain status.

The block stream is the demanding one. At the benchmark block interval of 50 ms
with up to 50,000 transactions per block, a replica has to decode, verify and
persist a large message every few tens of milliseconds. Whatever the wire
format is, parsing must not become the bottleneck, and the replica must be able
to keep several requests in flight.

Three options were on the table.

JSON-RPC is universally debuggable and universally slow. Every block would be
re-encoded into text and re-parsed, and the 256-bit integers this chain traffics
in do not survive JSON numbers.

gRPC solves the encoding problem but brings HTTP/2, TLS termination and a code
generator that produces a large surface. Its streaming model is server push,
which would have to be bolted onto a system whose replicas want to pull at their
own rate.

Cap'n Proto reads messages in place — a field access is a pointer offset, not a
parse — and its RPC layer passes capabilities, so a call can return an object
the caller then drives.

An important constraint shaped the choice: the wire format is *not* the format
that gets hashed. Transactions, block headers and quorum certificates are
serialised with `bincode`, and signatures and hashes are taken over those bytes.
Whatever the RPC format is, it is a transport, and it must never become a second
definition of what a transaction *is*.

## Decision

Node communication uses Cap'n Proto RPC over plain TCP, through
`capnp-rpc`'s two-party vat network. Schemas live in
`rust/crates/cb-schema/schema/` (`types.capnp`, `tx.capnp`, `block.capnp`,
`rpc.capnp`) and are compiled by `cb-schema`'s build script.

One interface, `SuperFrontierLiquid`, carries the methods: `submitTx`,
`getAccount`, `getBlock`, `getOrderBook`, `streamBlocks`, `streamMarket`,
`submitVote`, `streamBlocksRaw`, `chainInfo`, and the derivatives reads
(`getPositions`, `getMarginAccount`, `getFundingIndex`, `getOraclePrice`,
`getMarkPrice`, `getPerpMarket`, `listPerpMarkets`, `getFutureMarket`,
`listFutureMarkets`, `getLiquidationQueue`, `getPerpOrderBook`).

Streaming is capability-based and pull-driven. `streamBlocks` returns a
`BlockSub`; the caller calls `next()` as fast as it wants to consume, and the
replica pipelines several `next()` calls at once. `streamBlocksRaw` returns
pre-encoded block bytes plus a small metadata envelope so a mirroring replica
can write them straight to disk.

`bincode` remains the canonical encoding for persistence and for every hash
preimage. Cap'n Proto enum ordinals mirror the Rust enum ordinals in `cb-types`
exactly, and tests pin that correspondence.

## Consequences

The block path is cheap. A replica in fast-mirror mode never deep-decodes a
block at all: it takes the bytes from `streamBlocksRaw` and writes them, which
is only possible because the transport hands over framed, addressable buffers.

Back-pressure is free. Because the client pulls, a slow replica simply calls
`next()` less often. There is no push queue to size and no slow-consumer policy
to write.

Schema evolution is disciplined. Field numbers and enum ordinals are explicit in
the `.capnp` file, which makes "append only, never renumber" a property someone
can check by reading the schema, rather than a convention.

Now the costs, and they are real.

**A browser cannot speak it.** This is the single largest consequence: it is the
reason `cb-gateway` exists at all. Every browser-facing feature needs a
translation layer that re-implements, in JSON, a subset of what the RPC already
offers. Two API surfaces now have to be kept honest with each other, and the
JSON one is frozen in `docs/api-contract.md` precisely because it is a second
seam that can drift.

**You cannot curl it.** Debugging a live node means using `cb-client`, the
operator dashboard, or writing Rust. There is no equivalent of pasting a request
into a terminal, and that friction is felt every time something is wrong in
production-shaped conditions.

**It adds a build dependency.** `capnp` (the schema compiler) must be installed
before the workspace builds. That is one more thing between a new contributor
and their first successful `cargo build`.

**Two encodings must be kept in step.** A transaction has a Cap'n Proto shape
and a `bincode` shape, and the signature is over the second. A mismatch between
them is not a compile error; it is a wrong sighash. Tests pin the ordinals, and
those tests are load-bearing.

**The schema can promise more than the server does.** `streamMarket` and the
`MarketEvent` union are defined in `rpc.capnp`, but the server implementation
returns an error — the method is not implemented. A schema is a contract only
where someone has written the code behind it.
