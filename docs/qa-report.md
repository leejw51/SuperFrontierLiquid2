# QA report — `cb-simgame`

> **Status note, added by the core-fixes engineer.** Findings F1–F10 are fixed;
> F11 is not, and §4.6 of `docs/architecture.md` records why and what closing it
> would take. A twelfth defect turned up while confirming the fixes, in a
> scenario this report did not run: a `LiquidationTrigger` naming a dated future
> was rejected with `UnknownMarket`, because the handler resolved its market
> through `PerpMarkets` alone, so a leveraged futures position could never be
> liquidated. `cargo run -p cb-simgame -- run --blocks 400 --scenario
> "calm:100,crash:150,calm" --future-expiry 300 --seed 11` showed 1 536 rejected
> triggers and violated invariant 6 twenty-eight times. It is fixed, with a
> regression test in `cb-block/tests/futures_settle.rs`. Worth adding
> `--future-expiry` to the crash runs in `cb-simgame/tests`: listing a future is
> what exposed it. The report below is left as QA wrote it, because it is the record
> of what the simulation found and how, not a task list to be edited as items
> close. Three things it says are no longer true of the code: the state root now
> has a fourth slot over margin accounts (F3), the chain derives its own funding
> premium so `--no-premium` no longer means "no funding" (F9), and spot survives
> a full run, so the `--spot` default in `cb-simgame` is QA's to flip when they
> next touch the harness. Every reproduction under `cb-block/tests/qa_*.rs` has
> been replaced by a test asserting the corrected behaviour, under a name that
> still points back to the finding.

Owner: QA. Covers milestone M6 (simulation game) against the L1 core as it
stands today: spot, perpetuals and dated futures, `cb-block`'s executor and
`cb-engine`'s margin, funding, liquidation and matching maths.

---

## 1. What was built

`cb-simgame` is a living exchange rather than a load generator. A population
of bots with distinct, adversarial strategies trades against itself (and, in
networked mode, against any human who joins), a scenario engine drives the
price feed through scripted regimes, and an invariant checker re-derives all
nine invariants from `docs/PLAN.md` §6 out of raw column-family state at
**every block boundary** — not on a sample, and never from a running total the
simulation keeps for itself.

| Piece | Where | What it does |
|---|---|---|
| World | `src/world.rs` | Markets, accounts, the block loop, in-process execution against `cb-block` |
| Scenario engine | `src/scenario.rs` | `calm`, `trend_up`, `trend_down`, `crash`, `squeeze`, `chop`, schedulable as a timeline |
| Bots | `src/bots/` | Market maker, momentum, noise, whale, liquidator, arbitrageur |
| Ledger | `src/ledger.rs` | The conserved quantity and its rounding budget |
| Invariant checker | `src/invariants.rs` | The nine invariants, one function each |
| Report | `src/report.rs` | Trades, volume, liquidations, funding, per-bot P&L, invariant status |
| Networked mode | `src/net.rs` | The same bots driving a live node over Cap'n Proto RPC. **Compiles; not driven against a running node in this work** — see §4. |
| Binary | `src/main.rs` | `cb-simgame run` (in process) and `cb-simgame live` (networked) |

```
# a rally into a crash, which is what makes liquidations cascade rather than
# happen one at a time
cargo run -p cb-simgame -- run --blocks 400 --scenario "calm:60,trend_up:110,crash:90,calm" --seed 1
# reproduce the spot lock defects (F6 panics the executor within ~100 blocks)
cargo run -p cb-simgame -- run --blocks 400 --scenario crash --spot --fail-fast
# shipped behaviour, with no injected funding premium
cargo run -p cb-simgame -- run --blocks 2000 --scenario calm --no-premium
cargo run -p cb-simgame -- live --sequencer 127.0.0.1:7700 --drive-oracle
```

### The conservation check, specifically

"Conservation failed" is a useless report, and a mark-to-market equity total
drifts with every price move and every per-position rounding step. So the
derivative side is checked against a quantity that is exact and
price-independent:

```
V = Σ_accounts (collateral − bad_debt)·1e10  +  Σ_positions s·qty·entry_px_q8
      where s = −1 for a long, +1 for a short
```

`ΔV = 0` across opens, closes, flips, funding, the liquidation fee split,
bad-debt absorption and settlement (the derivation is in the module docs of
`src/ledger.rs`). Every value sink is therefore accounted for explicitly:
liquidation fees are a transfer into the insurance fund and the keeper,
spot taker fees are a transfer into `Address::INSURANCE`'s spot balance, and
bad debt is subtracted so an uncovered loss cannot masquerade as value. The
only permitted movement is integer truncation, budgeted per block and six
orders of magnitude below a one-dollar leak.

Spot is checked separately and exactly: the sum of every asset's balances
across every account never changes, to the unit.

---

## 2. Invariant status

Measured over the runs in `crates/cb-simgame/tests/`: a 2 000-block calm run,
a 400-block crash with a cascade, a 380-block squeeze, a settlement run, and
several funding runs.

| # | Invariant | Status |
|---|---|---|
| 1 | Conservation | **Holds** as shipped. **Violated** once funding actually flows — see finding F2. |
| 2 | No negative balances | Holds. |
| 3 | Lock consistency | Holds on derivatives (nothing is locked there — see F11). **Violated on spot** — F6, F7. |
| 4 | Book integrity | Holds. Best bid was never at or above best ask on any market at any block boundary. |
| 5 | Position netting | Holds. Long and short open interest matched on every derivative market at every boundary. |
| 6 | Margin soundness | Holds. Every account below maintenance reached the liquidation queue within the grace period and was cleared while the book could absorb it. **Caveat:** the bot population almost never rests an order on a leveraged account, so the "queued but never cleared" half of this check does not reach F5's path. The unit test is F5's only coverage. |
| 7 | Funding neutrality | Holds vacuously as shipped (funding never moves). **Violated** with a premium — F2. |
| 8 | Determinism | **Holds.** Same seed, same per-block state root, same per-block ledger digest, same fills, same per-bot P&L. The ledger digest is compared alongside the root because the root does not cover margin accounts (F3). |
| 9 | Settlement | Holds. Expiry closed every position and left zero open interest, and did not touch the perpetual books. |

Determinism reproduces: `tests/determinism.rs` runs the same seed twice and
compares the state root after *every* block, not just the last one, so a
divergence localises to the height it started at. It also confirms a different
seed produces a different chain — otherwise the test would pass for the wrong
reason.

---

## 3. Findings

Eleven. Nine of them have a minimal reproduction under
`crates/cb-block/tests/`, written to **fail when the defect is fixed** with an
assertion message that says so. The remaining two — F9 and F11 — are
behavioural gaps rather than wrong arithmetic; F9 is asserted at simulation
scale in `cb-simgame/tests/funding.rs`, and F11 is a design gap recorded for
the record.

| ID | Severity | Summary | Test |
|---|---|---|---|
| F1 | **Critical** | A reduce-only order can flip a position and skips the initial-margin gate | `qa_perp_findings.rs` |
| F2 | **High** | `Position.entry_funding_index` is never initialised; funding mints and burns collateral | `qa_perp_findings.rs` |
| F3 | **High** | Margin accounts are outside the state root | `qa_state_root_margin_gap.rs` |
| F4 | **High** | Bad debt is recorded and then ignored by everything | `qa_perp_findings.rs` |
| F5 | **High** | An account can block its own liquidation with a resting order | `qa_perp_findings.rs` |
| F6 | **High** | A resting spot buy filled in pieces over-unlocks and panics the executor | `qa_spot_findings.rs` |
| F7 | Medium | Spot price improvement is locked forever | `qa_spot_findings.rs` |
| F8 | Medium | Perpetual trading fees are declared but never charged | `qa_perp_findings.rs` |
| F9 | Medium | The funding premium is never computed, so funding is inert and mark ignores the book | (behavioural; asserted in `cb-simgame/tests/funding.rs`) |
| F10 | Medium | A zero-quantity perpetual order aborts the executor under debug assertions | `qa_perp_findings.rs` |
| F11 | Low | Perpetual resting orders reserve no collateral | (documented; no test) |

---

### F1 — A reduce-only order can flip a position, and skips the margin gate

**Severity: critical.** Unbounded leverage with no margin check.

**What breaks.** `apply_place_perp_order` admits a reduce-only order on the
sole condition that an *opposing position exists*
(`cb-block/src/executor.rs`, step 4); it never compares the order's quantity
against the position's. Step 5 then skips `passes_initial_margin` entirely for
reduce-only orders. `cb_engine::book::match_order` does not revalidate either,
despite the module documentation claiming that "reduce-only orders that would
grow the taker's absolute position size are silently cancelled". A resting
reduce-only GTC order is likewise never revalidated when it is hit as a maker,
so it can open a position long after the one it was meant to close is gone.

**Smallest reproduction.**
`cb-block/tests/qa_perp_findings.rs::reduce_only_order_flips_the_position_and_bypasses_initial_margin`

1. Alice holds **one base unit** of long and has $10 of collateral.
2. She sends `PlacePerpOrder { side: Sell, qty: 10 BTC, reduce_only: true }`.
3. The identical order without the flag is rejected with `InsufficientMargin`.
   With the flag it is accepted, and Alice is now short 10 BTC — half a
   million dollars of notional against ten dollars — and immediately
   liquidatable.

**Where I believe the cause is.** Two places, and both need fixing:
`cb-block/src/executor.rs` step 4 must clamp or reject when
`qty > pre_position.qty`, and `cb_engine::book::match_order` must stop filling
a reduce-only taker once the fill would cross zero (and must apply the same
rule to a reduce-only maker). The margin-gate skip in step 5 is only sound
once the flag is actually enforced.

---

### F2 — A new position's funding index is never initialised

**Severity: high.** Funding stops being zero-sum and mints or burns collateral.

**What breaks.** `cb_engine::perps_position::apply_fill_to_position`'s
flat-to-open case sets `entry_funding_index: prev_funding_idx`, which is
`Signed::ZERO` for an account that has never held a position on that market —
not the market's live `FundingIndex.cumulative`. `settle_market_funding` then
charges `qty * (cumulative − entry) / RATE_DEN_Q9`, so a position opened after
the index started accruing is charged for the entire history before it
existed. Longs and shorts end up carrying different snapshots, the flows stop
cancelling, and total collateral changes with no counterparty. The same
applies to a `qty == 0` sentinel row, which is skipped by the settlement walk
and so keeps a stale snapshot for a later reopen.

**Smallest reproduction.**
`cb-block/tests/qa_perp_findings.rs::funding_is_not_zero_sum_because_new_positions_inherit_a_zero_funding_index`

Alice long and Bob short from block 2; two funding ticks let the index accrue;
Eve then opens a fresh long against Bob at block 11 and is stamped with index
`0`. At the next tick total collateral across the three accounts moves by
exactly `qty * cumulative_at_open / 1e9` with nobody on the other side.

At simulation scale this fires as a *simultaneous* violation of invariant 1
and invariant 7 on the same funding-boundary block. On
`--seed 1 --scenario "calm:60,trend_up:110,crash:90,calm" --blocks 400
--funding-interval 25`, the first funding boundary at which the index has
accrued (height 75) moves 44 947 670 micro-USD out of accounts and 44 951 274 in: **3 604
micro-USD created with nobody on the other side**, against a per-position
rounding allowance of 36. That the imbalance registers as a conservation
failure as well as a neutrality failure is the point: it is collateral
appearing from nowhere, not an artefact of how funding is measured.

**Where I believe the cause is.**
`cb-engine/src/perps_position.rs`, case 1 (flat → open): the caller must pass
the market's current cumulative index and the position must be stamped with
it. `cb-block/src/executor.rs::apply_fills_with_pnl` is the caller that has to
supply it.

---

### F3 — Margin accounts are outside the state root

**Severity: high.** Consensus cannot detect a divergence about solvency.

**What breaks.** `cb_state::compute_state_root` hashes exactly three inputs —
the accounts trie, the positions trie and the markets trie.
`CF_MARGIN_ACCOUNTS` feeds none of them and no other trie covers it.
`cb-state/src/lib.rs` documents what it deliberately excludes — funding
indices, mark and oracle prices, the liquidation queue, the perp orderbook,
the mark-history buffer — all justified as "auxiliary, they derive from the
txs and block height". Margin accounts are not on that list and are not
auxiliary: they hold every trader's collateral, every unpaid loss, and the
insurance fund.

Two replicas can therefore disagree about who is solvent while reporting
byte-identical state roots. Nothing in the hash chain commits to
genesis-seeded collateral either. The first symptom in production is a
liquidation one node performs and another refuses — after the divergence has
been finalised.

**Smallest reproduction.**
`cb-block/tests/qa_state_root_margin_gap.rs::two_replicas_disagreeing_on_every_margin_account_still_report_the_same_state_root`

Two fresh databases, the same market, the same two blocks, the same
transactions — and different starting collateral for every account plus an
emptied insurance fund on one of them. Positions come out identical; state
roots come out identical. The file carries a positive control showing that a
*position* difference does change the root, so the result is a real gap rather
than an artefact of how the test drives the executor.

**Where I believe the cause is.** `cb-state` needs a fourth trie over
`CF_MARGIN_ACCOUNTS` (the `Positions`/`MarketsTrie` pair is the template,
including the `BlockState::touch_*` plumbing), and `compute_state_root` needs
a fourth slot appended. That is a hard fork of the root preimage, which the
existing doc comment already anticipates — "future surfaces get appended to
the tail". The executor already writes margin rows through
`MarginAccounts::put` from six call sites, all of which would need a
`touch_margin`.

---

### F4 — Bad debt is recorded and then ignored by everything

**Severity: high.** Silent, permanent protocol insolvency.

**What breaks.** Three paths write `MarginAccount.bad_debt`: realised PnL on a
fill (`credit_realized_pnl`), a funding tick (`settle_market_funding`), and
futures settlement (`apply_futures_settle`). Exactly one path reads it —
`apply_bad_debt_absorption`, reachable only from the tail of a
`LiquidationTrigger`, and only for the single account that trigger names.

Nothing in `cb-engine` reads it at all: `compute_equity`, `is_liquidatable`
and `passes_initial_margin` are written in terms of `collateral` alone. So an
account that owes the protocol money is treated as one with *no* money rather
than *negative* money. The debt is never absorbed by the insurance fund, never
socialised, never repaid, and never restricts the account that owes it — and
the reduce-only halt that is supposed to fire on insurance exhaustion never
fires for this class of debt either.

**Smallest reproduction.**
`cb-block/tests/qa_perp_findings.rs::bad_debt_from_an_ordinary_fill_is_never_absorbed_and_never_repaid`

Alice goes long 1 BTC at 50 000 on $1 500 of collateral, the market falls to
45 000, and she closes into a bid herself. She realises a $5 000 loss, her
collateral is wiped and $3 500 lands in `bad_debt`. The insurance fund — which
holds a million dollars — is untouched, because no liquidation was triggered.
`compute_equity` reports `0` for the account. Funded again, she passes the
initial-margin gate and opens a fresh position with the debt still
outstanding.

At simulation scale, a 380-block crash run ends with outstanding bad debt and
an insurance fund that could have covered it many times over
(`cb-simgame/tests/crash_run.rs::bad_debt_from_ordinary_fills_is_never_absorbed_by_the_insurance_fund`).

**Where I believe the cause is.** Two separate fixes.
`cb-engine/src/margin.rs::compute_equity` should subtract `bad_debt` so the
margin system can see it, and absorption needs to run wherever bad debt is
created — an end-of-block sweep in `cb-block/src/block.rs` would cover all
three writers, rather than the current single call site.

---

### F5 — An account can block its own liquidation with a resting order

**Severity: high.** A trivially available defence against being liquidated.

**What breaks.** `apply_liquidation_trigger` submits its reduce-only tranches
through the ordinary matcher with `SelfTradeAction::CancelTaker`. If the
account being liquidated has a resting order on the closing side at a crossing
price, `match_order` hits it first in price-time order, sets `self_trade_seen`
and returns `RejectedSelfTrade` with **no fills** — discarding any fills it had
already planned against other makers. The liquidation loop never inspects
`result.outcome`; it applies an empty mutation list, sums a zero notional,
charges a zero fee and returns `Ok(())`. All three ladder tranches walk the
same book, so a single resting order defeats all of them. The account's
resting orders are never cancelled on liquidation either.

**Smallest reproduction.**
`cb-block/tests/qa_perp_findings.rs::a_resting_order_lets_an_account_block_its_own_liquidation`

Alice goes long 1 BTC at 50 000 and, while still healthy, rests a small bid of
her own at 49 000. Bob stacks real bids at 48 500 and 48 000. The oracle drops
to 48 000 and Alice is liquidatable. The keeper's trigger returns `Ok(())`,
closes nothing, and pays the keeper nothing. Cancel Alice's bid and the
identical trigger one block later closes the position and pays the bounty.

**Where I believe the cause is.**
`cb-block/src/executor.rs::apply_liquidation_trigger` must cancel the owner's
resting orders on the market before generating tranches (which is what a real
venue does), and must treat a non-`Ok` `MatchOutcome` as a failed tranche
rather than as no liquidity. Using `SelfTradeAction::CancelMaker` for
liquidation-generated orders would also work and is arguably more correct: the
victim's own quote should be consumed, not honoured.

---

### F6 — A resting spot buy filled in pieces over-unlocks and panics the executor

**Severity: high.** A panic inside block execution, reachable by ordinary use.

**What breaks.** `apply_place_spot_order` locks a buy **once**, over the whole
order, as `floor(notional) + ceil(fee)`. It releases **per fill**, as
`floor(notional_i) + ceil(fee_i)`. Because the fee reserve rounds up, the sum
of the per-fill ceilings can exceed the single ceiling taken at placement, so
the releases total more than was ever locked. `AccountState::unlock` is a
`checked_sub().expect("unlock underflow")`, so the overshoot is not a silent
inconsistency — it is a panic in the middle of applying a block.

**Smallest reproduction.**
`cb-block/tests/qa_spot_findings.rs::a_resting_buy_filled_in_two_pieces_over_unlocks_and_panics_the_executor`

With `fee_bps = 10`, `tick = 1`, `min_qty = 1` and a price of `1e8`:

```
lock at placement    = 2000 + ceil(2000·10/10000) = 2002
release on fill 1001 = 1001 + ceil(1001·10/10000) = 1003
release on fill  999 =  999 + ceil( 999·10/10000) = 1000
                                         total     = 2003  >  2002
```

A resting buy of 2 000, taken by two sells of 1 001 and 999, aborts the
executor with `unlock underflow`.

This is not a corner case: `cb-simgame` hit it unprompted at block 113 of a
default spot run the first time spot was switched on. Spot is off by default
in the harness for that reason — `--spot` reproduces it.

**Where I believe the cause is.**
`cb-block/src/executor.rs`, the fill loop of `apply_place_spot_order`. The
lock must be reconciled against the *remaining* order rather than released
incrementally: compute the lock the order still needs after the fills and
release the difference, once.

---

### F7 — Spot price improvement is locked forever

**Severity: medium.** Permanently unspendable balance; no panic.

**What breaks.** A taker buy locks against its **limit** price; each fill
releases against the **fill** price. When the taker gets price improvement the
releases fall short, and step 10 of `apply_place_spot_order` deliberately
leaves the whole remainder locked when the order rests. Cancelling later
releases only what the remainder reserves *at the limit price*, so the
improvement is stranded: the account's locked balance never returns to zero
and that quote is unspendable for the life of the account. The code comment at
step 10 anticipates "small rounding residue ... bounded by the fee_reserve
ceiling"; the residue is in fact bounded by the price improvement, which is
unbounded.

**Smallest reproduction.**
`cb-block/tests/qa_spot_findings.rs::price_improvement_on_a_partially_filled_buy_is_locked_forever`

A maker offers 400 at 1.0. A taker buys 1 000 with a limit of 2.0: 400 fills at
1.0 and 600 rests. 1 601 stays locked where the resting remainder only
reserves 1 202. Cancelling the remainder leaves 399 locked with no order
behind it at all.

**Where I believe the cause is.** Same site and same fix as F6 — reconcile the
lock against the remaining order instead of releasing per fill.

---

### F8 — Perpetual trading fees are declared but never charged

**Severity: medium.** Revenue that exists in the API and not in the executor.

**What breaks.** `PerpMarket` carries `taker_fee_bps` and
`maker_fee_bps_signed`, the Cap'n Proto surface publishes both
(`rpc.capnp` fields 7 and 8), the dashboard shows them and the design document
specifies them — but no path in `cb-block` reads either field. `grep -rn
"taker_fee\|maker_fee" cb-block/src` matches only the spot handler's
`MarketConfig.fee_bps`. On the perpetual side only the liquidation fee is ever
charged.

**Smallest reproduction.**
`cb-block/tests/qa_perp_findings.rs::perpetual_fills_charge_no_taker_fee_and_pay_no_maker_rebate`

A 1 BTC fill at 50 000 — $50 000 of notional, three basis points of which is
$15 — leaves the taker's collateral, the maker's collateral and the insurance
fund all exactly where they were.

**Why it matters to QA specifically.** The conservation invariant passes on the
perpetual side partly because there is no fee sink to account for. When fees
are implemented, `src/ledger.rs` needs no change if they are a transfer into
an accounted address (the insurance fund, as the spot side already does), but
it *will* need a mint/burn allowance if any fee is destroyed or paid out of
band. Worth coordinating when the fee path lands.

**Where I believe the cause is.** A missing step between
`apply_fills_with_pnl` and the nonce advance in `apply_place_perp_order`. The
maker rebate also needs the "rested for ≥ 1 block" wash-trade guard that
`PerpOrder.placed_at_height` was added for and that nothing currently reads.

---

### F9 — The funding premium is never computed, so funding is inert

**Severity: medium.** A specified mechanism that does nothing.

**What breaks.** `apply_oracle_update` sets the new `MarkPrice`'s
`premium_ema_q9` by carrying the previous value forward ("Premium EMA is a
no-op pre-M3"), and calls `compute_mark_price(px_q8, None, last_q8, k)` — with
`mid` hard-coded to `None` on every path. Nothing else in the tree writes the
field (`grep -rn "premium_ema_q9" cb-block/src cb-engine/src cb-state/src`
returns only reads and that carry-forward). The field therefore starts at zero
and stays there, so `compute_funding_rate` always returns zero and every
funding tick moves nothing.

Two consequences beyond funding itself: the mark price never sees the order
book, so the "median of three" construction degenerates to a median of
`(oracle, oracle, clamped last trade)`; and an arbitrageur bot has no
mark-versus-index spread to trade, which is why `cb-simgame`'s arbitrageur
compares the **book midpoint** against the oracle instead.

**Reproduction.** `cb-simgame/tests/funding.rs::as_shipped_every_funding_tick_moves_nothing`
— three hundred blocks across several funding boundaries, total funding paid
exactly zero.

**How the harness handles it.** `WorldConfig::inject_premium` writes the
premium the chain should have derived (book midpoint against oracle,
exponentially smoothed at 1/8) directly into `OracleState` after the oracle
updates land. It is a simulation input, not a fix: deterministic, applied
identically on every run of a seed, and clearly labelled. Without it the
funding path is untestable; with it, F2 surfaces. Tests that assert "every
invariant holds" run with it **off**, so they assert against shipped
behaviour.

**Where I believe the cause is.** `cb-block/src/executor.rs::apply_oracle_update`
needs to read the book midpoint (via `PerpBookIndex`, which it already has
access to in the same crate) and fold it into `premium_ema_q9`, and pass that
midpoint as `mid_q8` to `compute_mark_price`.

---

### F10 — A zero-quantity perpetual order aborts the executor under debug assertions

**Severity: medium.** Liveness, reachable only from a Byzantine proposer.

**What breaks.** The pre-trade margin gate calls
`simulate_worst_case_position_set` — and through it `apply_fill_to_position` —
*before* the matcher's `BookError::ZeroQty` guard runs. With `qty == 0` the
`debug_assert!(fill_qty > 0, ...)` fires and the process aborts.
`cb_tx::validate_static` rejects such a transaction at mempool admission, so
the ordinary path is shielded; but the executor is the consensus-critical
layer, and a replica re-executing a block a malicious proposer built panics
instead of rejecting the block.

**Smallest reproduction.**
`cb-block/tests/qa_perp_findings.rs::zero_quantity_perp_order_aborts_the_executor_under_debug_assertions`

**Where I believe the cause is.** `cb-block/src/executor.rs` should reject
`qty == 0` (and `price_q8 == 0`) in `apply_place_perp_order`'s admission block,
before step 5. Consensus code should not rely on a `debug_assert` for input it
can receive from the network.

---

### F11 — Perpetual resting orders reserve no collateral

**Severity: medium.** Design gap rather than a crash; recorded for the record.

Invariant 3 is "locked balance equals the sum of margin plus resting order
collateral for that account". On the perpetual side the second term does not
exist: `PlacePerpOrder` locks nothing, and the initial-margin gate simulates a
worst-case fill of **that single order** against the account's current
positions. N resting orders can each pass the gate independently and then all
fill, leaving an account far past the leverage any one of them implied.

The invariant checker therefore verifies the derivative contribution is
exactly zero and says so in `check_lock_consistency`, rather than silently
assuming it. A fix would mean reserving initial margin per resting order and
releasing it on fill or cancel — a real design change, so it is flagged rather
than filed as a bug.

---

## 4. Notes on checks I considered weakening, and did not

* **Funding neutrality on a settlement block.** A block that both ticks
  funding and settles a dated future moves collateral for two unrelated
  reasons, and the funding flow cannot be isolated from the settlement by
  looking at collateral deltas. The checker does not evaluate neutrality on
  such a block, and says why. This is a genuine limitation of the measurement,
  not a defect being hidden: conservation *is* still evaluated there, and it is
  the stronger statement — it says the settlement created and destroyed
  nothing.

* **Funding-boundary blocks are quiet.** The world submits only oracle updates
  and the funding ticks at a boundary height, so the collateral delta across
  that block *is* the funding flow. That is a property of the harness, not of
  the chain, and `funding.rs` has a test that guards it.

* **Invariants 1 and 7 with a premium injected.** These fail, and the tests
  assert that they fail, in the specific bounded way F2 predicts — on funding
  boundaries, and only those two invariants. The helper is
  `assert_only_known_funding_defect`, which still fails the test on *anything*
  else. It does not say "ignore 1 and 7".

* **Spot is off by default.** Not to make tests pass: F6 panics the executor
  within a few hundred blocks, which ends every run before the derivative
  invariants have been exercised. `--spot` turns it on, and both spot defects
  have their own regression tests.

* **Networked mode is unexercised.** `cb-simgame live` builds and its Cap'n
  Proto calls type-check against the current schema, but no node was stood up
  to run it during this work. Two things to check first when someone does:
  `--drive-oracle` only works if `--oracle-seed` derives the key that genesis
  named as each market's `oracle_authority`, and the oracle heights this
  session submits continue from the node's stored `source_height` (read at
  startup) because `check_freshness` rejects a post below it. Invariants are
  deliberately not checked in this mode: the checker needs a full ledger scan,
  which RPC does not expose in one call, and correctness is proven in process
  where the run is reproducible.

* **The dust budget.** The conservation check allows integer-rounding drift
  scaled by the number of value-moving events and by open position size (the
  volume-weighted-entry residue is proportional to position size, not
  constant). The budget is roughly `1e-4` dollars against a one-dollar leak at
  `1e16` units, so it cannot mask anything meaningful. It is derived in
  `src/ledger.rs::DustBudget`.

## 5. Coordination notes

* **The universe grew during this work** and the checker was adapted for it.
  `AssetId` is now eight assets (BTC, ETH, USD, SOL, AAPL, TSLA, NVDA, SPY)
  and `MarketId` seven spot markets. The checker no longer names any of them:
  book integrity and lock consistency walk `MarketId::ALL`, derivative markets
  are discovered by listing `CF_PERP_MARKETS` and `CF_FUTURES_MARKETS`, and
  asset labels resolve through `AssetId::from_u8(..).symbol()`. Appending to
  either enum needs no change here. What `cb-simgame` *lists and trades* is
  still the 8-decimal set in `src/universe.rs`, deliberately — see the next
  point.
* Notional maths hard-codes an 8-decimal base scale:
  `position_notional_micro_usd` divides `qty * mark_q8` by `1e10`, documented
  in `cb-engine/src/margin.rs` as "specifically targets BTC-style 8-decimal
  underlyings". SOL is 9 decimals and the equities are 6, so margin and
  notional on those markets will be wrong by a power of ten. The simulation
  therefore lists only 8-decimal markets for now: a harness whose margin
  numbers are known to be wrong cannot tell a margin bug from a scaling bug.
  Once the per-market base-decimals knob lands, `src/universe.rs::SIM_MARKETS`
  is the one place to extend, and the invariant checker needs no change.
* F3 changes the `compute_state_root` preimage, so it wants sequencing with
  whatever else is in flight on consensus.
* F8 (perpetual fees) intersects the gateway's revenue reporting. If fees land
  as a transfer into `Address::INSURANCE`, the conservation check needs no
  change.

## 6. Test inventory

| Suite | Tests | Purpose |
|---|---|---|
| `cb-simgame` unit | 10 | Scenario timeline parsing, price-path determinism, crash severity, the `MarketDataFeed` adapter |
| `cb-simgame/tests/calm_run.rs` | 2 | 2 000 calm blocks hold every invariant; rejection paths leave no residue |
| `cb-simgame/tests/crash_run.rs` | 2 | A crash cascades liquidations (144 triggered, up to 16 in one block, 8 accounts liquidatable at once) and every invariant still holds; F4 at scale |
| `cb-simgame/tests/determinism.rs` | 3 | Same seed → identical per-block state roots; different seed differs; determinism survives settlement and squeeze |
| `cb-simgame/tests/funding.rs` | 3 | Funding is inert as shipped; with a premium it moves value and does not net to zero (F2) |
| `cb-simgame/tests/settlement.rs` | 2 | Expiry settles every position and leaves zero open interest; perpetuals keep trading |
| `cb-simgame/tests/squeeze.rs` | 1 | No account goes negative under a squeeze |
| `cb-block/tests/qa_perp_findings.rs` | 6 | F1, F2, F5, F8, F10, F4 |
| `cb-block/tests/qa_spot_findings.rs` | 2 | F6, F7 |
| `cb-block/tests/qa_state_root_margin_gap.rs` | 2 | F3, plus a positive control |

**33 tests**, all passing. `cargo fmt` clean, `cargo clippy -p cb-simgame
--all-targets --no-deps -- -D warnings` clean, `#![forbid(unsafe_code)]` and
`#![warn(missing_docs)]` on the crate.
