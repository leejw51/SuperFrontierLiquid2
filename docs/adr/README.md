# Architecture decision records

One file per decision. Each records the context that forced a choice, the
choice, and what it costs — including the parts that are inconvenient.

| # | Decision | Status |
|---|---|---|
| [0001](0001-capnp-rpc-for-node-communication.md) | Cap'n Proto RPC for node communication | accepted |
| [0002](0002-rocksdb-for-consensus-sqlite-for-application.md) | RocksDB for consensus state, SQLite for application state | accepted |
| [0003](0003-custodial-wallets-in-v1.md) | Custodial wallets in v1 | accepted |
| [0004](0004-enumerated-spot-market-registry.md) | An enumerated, not dynamic, spot-market registry | accepted |
| [0005](0005-rust-and-webassembly-for-the-frontend.md) | Rust and WebAssembly for the frontend | accepted |
| [0006](0006-decimal-strings-on-the-wire.md) | Decimal strings on the wire, not JSON numbers | accepted |
| [0007](0007-in-memory-state-rocksdb-as-checkpoint.md) | One sequencer, state in memory, a hash chain instead of merkle tries | accepted in part; decision 6 superseded by 0008 |
| [0008](0008-bft-ordering-execute-after-commit.md) | Byzantine-fault-tolerant ordering, execution after commit | accepted |
| [0009](0009-audit-risk-and-block-identity.md) | Audit remediation: account liabilities, maker risk and block identity | accepted |

## Writing a new one

Copy the shape of an existing record: a numbered file, a one-line status and
date, then **Context**, **Decision**, **Consequences**.

Number files sequentially and do not renumber. A decision that replaces an
earlier one does not edit it — write a new record, and change the old record's
status line to say which one superseded it.

Record a decision when undoing it would be expensive: anything that touches the
consensus encoding, the storage engines, the wire formats, the trust model, or a
dependency that would be painful to swap. Do not record routine library choices.

The consequences section is the one that earns the file. Write the bad ones
plainly; a record that only lists benefits is advertising, and nobody trusts it
six months later when the cost shows up.
