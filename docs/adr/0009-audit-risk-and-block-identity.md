# 0009 — Audit remediation: account liabilities, maker risk and block identity

Status: accepted for the prototype, 2026-09-30. Tracks GitHub issue #1.

## Rules

Cash collateral minus `bad_debt` is the account's signed cash balance. A
negative cash balance is retained while **any** nonzero position remains.
Closing gains repay it before creating positive collateral. Insurance may
extinguish only the deficit remaining after all positions are flat. This
deliberately postpones coverage during partial liquidation and funding: one
losing leg of a solvent hedge is not a bankruptcy. Ordinary complete closes,
zero-PnL final closes and dated settlement all reach residual-debt handling.
Insurance exhaustion and auto-deleveraging remain separate open work.

Every maker fill that can increase/open exposure is simulated against live
positions and signed cash, including earlier planned fills for that owner,
realized PnL, closing-lot funding and positive maker fees. A failing maker is
cancelled and the matcher continues to other liquidity. Genuine reducing fills
remain available subject to the market's price/freshness checks. Planned fills
are realized in the matcher's deterministic price/time order.

For each market separately, initial margin is the maximum of the piecewise
default requirement, configured initial-margin requirement and configured
maximum-leverage requirement. Maintenance margin is the maximum of default and
configured maintenance requirements. Portfolio requirements sum these per-market
maxima. Perpetual `max_position_notional` bounds potential exposure, including
ordinary resting orders. Futures have no persisted notional-cap field and
continue to use their margin and leverage limits. Reductions do not need to
restore initial margin merely to close risk. Invalid risk configuration fails
closed when admitting increased exposure.

The consensus freshness budget is 600 blocks. Marks and oracle submissions
from the future are refused. The older of a mark's computation height and its
available oracle source height determines age, so recomputing a mark cannot
refresh old input. Opening exposure requires fresh prices for its whole margin
portfolio; liquidation also requires fresh portfolio marks. All derivative
fills, including voluntary closes, require a fresh market mark. Cancellation
and oracle updates remain available during an outage. Funding and final dated
settlement are not redesigned here; they still need a fuller outage policy.

Block identity is:

```
keccak256("CB-BLOCK-ID-V2\0" || bincode((height, prev_hash, tx_root,
                                      state_root, timestamp, view)))
```

Signatures and certificates remain verified, but are excluded from identity.
Different valid vote permutations or quorum subsets therefore identify the
same executed block. BFT sync requires exact equality between the block and
signed proposal timestamps. This fixes witness malleability; a complete
two-chain commit proof is separate work and is still listed in SECURITY.md.

## Compatibility and operational consequences

These change consensus execution and block hashing. Start a fresh prototype
chain with a new chain identity and run the same version on every validator,
replica and client that computes block IDs. Do not point this version at an
old live database or combine old and new validators. No existing database,
funds or running process is reset by this change, and no automatic migration is
provided.

Maker revalidation adds execution work. Prior throughput results predate these
checks and must not be presented as measurements of this version. A longer
oracle outage blocks voluntary closes as well as new trades until a fresh
price arrives; users can cancel orders throughout. Accounts may carry negative
cash while positions remain, even when insurance is funded. That liability is
counted in margin and PnL conservation; it is not an unrecorded loss.

## Verification

`cb-block/tests/audit_20260929.rs` exercises the original economic witnesses,
risk limits and freshness boundaries. `cb-node/tests/bft_execution.rs` checks
vote permutations, extra valid votes, alternate quorum subsets, timestamp
tampering, and continuing sync against honest successors. Existing accounting,
liquidation, matching, consensus and sync suites provide the broader checks.
