# Flow review

A senior review of every user-facing flow, end to end, across every process
boundary. Read-only on `rust/`: this document reports, it does not fix.

**Method.** Every finding was reached by reading the code. Seventeen were then
reproduced against a running stack — a scratch chain on `127.0.0.1:7730` with
its own data directory, a gateway on `127.0.0.1:8131`, started exactly as
`rust/Makefile`'s `gateway` target starts it. Where a finding says **verified
live**, the transcript that produced it is quoted. Where it says **verified by
reading**, I traced the code and did not execute it. Where it says
**suspected**, I am telling you what I believe, why, and that I could not
confirm it.

Findings are ordered by severity, not by flow. Anything that creates or destroys
money, or stops the chain, is at the top. §40 says which flows I judged
**sound**, and why — a review that only lists problems says nothing about
coverage. §41 says what I could not verify.

---

## 0. Summary

| | Finding | Severity | Confidence |
|---|---|---|---|
| 1 | A failed block leaves half its state committed; the resulting unbacked order kills the node | **Critical** | verified live |
| 2 | Anyone can become an operator by registering the handle `admin`, then mint unbounded money | **Critical** | verified live |
| 3 | One failing transaction destroys the whole block and every transaction drained with it | **Critical** | verified live |
| 4 | The block commit is three unbatched writes; a crash mid-commit forks the chain | **Critical** | verified by reading |
| 5 | The gateway's nonce cache advances on mempool acceptance, so one failed transaction bricks an account permanently | **Critical** | verified live |
| 6 | F12: funding clamped per hour, accrued per block, and three layers disagree about the unit | **Critical** | verified by reading (known, open) |
| 7 | `GET /account` omits both contract addresses, so the web app's Place Order button is disabled for every order | **Critical** | verified live + reading |
| 8 | The `ticker` frame is missing three of five fields, so every price on screen is frozen and flagged stale | **Critical** | verified by reading |
| 9 | A faucet account can never trade a derivative: spot balance is not margin collateral and nothing bridges them | **High** | verified live |
| 10 | A spot fill never updates the order, so the UI shows a phantom open order whose cancel kills a block | **High** | verified live |
| 11 | The faucet mints before it records, and `Deposit` has no on-chain idempotency | **High** | verified by reading |
| 12 | The state-root check runs *after* the state is written, so a divergent block corrupts the replica it was meant to protect | **High** | verified by reading |
| 13 | A peer can drive a node to memory exhaustion, and stall its sync un-bannably | **High** | verified by reading |
| 14 | A revoked or expired session keeps receiving private data for the life of its socket | **High** | verified by reading |
| 15 | The gateway fabricates a fill for every open position whenever its account mirror is cleared | **High** | verified by reading |
| 16 | Every `account` WebSocket frame fails to decode; two contract message types are emitted by nobody | **High** | verified by reading |
| 17 | Accounts with no resting order are never refreshed, so liquidation and funding are invisible | **High** | verified by reading |
| 18 | `POST /admin/markets/{sym}/settle` never sends the price, always fails on chain, and audits a settlement that did not happen | **High** | verified live |
| 19 | Halt and risk changes live only in gateway memory; the chain never hears about them | **High** | verified by reading |
| 20 | The gateway writes ledger entries for transfers and grants the chain never applied | **High** | verified live |
| 21 | Rotating the server secret orphans every account silently and strands every resting order | **High** | verified by reading |
| 22 | The executor gates margin on a hardcoded tier table; every genesis risk parameter is decorative | **High** | verified by reading |
| 23 | Funding indices, oracle state and the whole perp order book sit outside the state root | **High** | structure verified; exploit suspected |
| 24 | No mark-staleness check on the consensus path; a market with no mark is un-liquidatable | **High** | verified by reading |
| 25 | Insurance-fund exhaustion sets no circuit breaker on a dated future | **High** | verified by reading |
| 26 | Displayed liquidation price and leverage are structurally decoupled from real collateral | **High** | verified by reading |
| 27 | F11: perp resting orders reserve nothing, and nothing else bounds the position | **High** | verified by reading (known, open) |
| 28 | Pre-handshake connections are unbounded and never reaped | **High** | verified by reading |
| 29 | Serving a block range blocks the p2p event loop under the chain mutex | **High** | verified by reading |
| 30 | A panic in the verifier stops sync forever, silently | **High** | verified by reading |
| 31 | `transport.rs` documents a mutex the sequencer never takes | **High** | verified by reading |
| 32 | The first sign-in with an imported phrase always fails, and the retry burns the nonce | **High** | verified by reading |
| 33 | `opened_at` is always null, so the positions blotter and the liquidation banner never render | **High** | verified by reading |
| 34 | "The request never reached it" is asserted for every transport failure, and the retry mints a new `client_order_id` | **High** | verified by reading |
| 35 | The faucet error path re-arms the button without refreshing anything | **High** | verified by reading |
| 36 | An irreversible admin settle reports as a decode failure | **High** | verified by reading |
| 37 | The activity feed sends `cursor=` and gets a 400, every time | **High** | verified by reading |
| §38 | Twenty-four medium findings | Medium | see §38 |
| §39 | Thirteen low findings | Low | see §39 |

**Counts.** Seventy-four findings: 37 numbered above, 24 medium, 13 low.
Seventeen were reproduced against a running stack. Three carry a suspected
component, each labelled in place: §23 (the gap is structural; its
exploitability I did not demonstrate), §31 (the missing mutex is certain; its
reachability depends on §4 firing first), and the settlement wedge in §38 (the
executor behaviour is read; how it surfaces at the consensus layer is not).
§39 additionally lists four typed hazards I could find no producing path for,
and says so.

---

## 1. A failed block leaves half its state committed, and the result kills the node

**Critical. Destroys the chain. Reproduced live, three ordinary API calls, no
privileges. Not previously recorded anywhere.**

> **Read this with §4 and §12.** All three are the same underlying defect —
> **a block's state mutation is not transactional** — with three different
> consequences. This section is the consequence reachable by an ordinary user
> through the public API; §4 is the consequence of a crash mid-commit; §12 is
> the consequence on a replica verifying somebody else's block. Fixing them as
> three unrelated tickets will leave two of them open.

### What breaks

`apply_tx` writes to two places with different durability:

* **Account balances and locks** go into `BlockState`'s in-memory map via
  `state.put(...)`, flushed only by `BlockState::commit`
  (`cb-block/src/state.rs:224-239`).
* **Everything else** — order-book rows, position rows, margin rows, oracle
  state, market rows — is written **straight to RocksDB** by the executor. The
  comment on `touch_position` says so explicitly: "executor still calls
  `Positions::new(db).put` directly so reads later in the same block see the new
  value" (`cb-block/src/state.rs:186-192`). The spot path does the same at
  `executor.rs:2504-2526` (`orders_cf.put`, `book_index_cf.put`).

When any transaction later in the block fails, `build_and_apply` returns `Err`
with `?` (`block.rs:121-123`). The balance map is discarded. **The direct CF
writes are not.**

So a block that aborts leaves resting orders on the book whose collateral lock
was rolled back. PLAN §7 invariant 3 — "locked balance equals margin plus
resting collateral" — is violated in committed state, and nothing ever notices,
because `CF_ORDERS` and `CF_BOOK_INDEX` are outside the state root (§23).

Then somebody hits that order. The executor settles the fill and calls
`maker_state.unlock(cfg.base, maker_lock_release)` (`executor.rs:2463` /
`:2477`) against a lock that was never taken. `AccountState::unlock` is
`checked_sub(n).expect("unlock underflow")` (`cb-state/src/account.rs:38-41`).

**The sequencer process panics and dies. The chain stops.**

### Smallest reproduction

This is the transcript I ran. Two funded faucet accounts, one fresh one.

```
# 1. A post-only order (intra-block rank 1) and a FOK order that cannot fill
#    (rank 2), submitted together so they land in one block.
POST /orders {"symbol":"BTC-USD","side":"sell","price":"70000.00","qty":"0.2500",
              "tif":"gtc","post_only":true}          -> 201 "pending"
POST /orders {"symbol":"BTC-USD","side":"buy","price":"40000.00","qty":"0.2500",
              "tif":"fok"}                            -> 201 "pending"

node:   ERROR block build failed: FokUnfilled
chain:  height 11 before, 11 after.        The block was never produced.

# 2. But the order-book row survived the abort.
GET /markets/BTC-USD/orderbook
  -> {"bids":[],"asks":[["70000.000000","0.25000000"]]}
GET /account   (the seller)
  -> BTC free 0.75000000, locked 0.00000000     <- no lock was ever taken
     nonce 1                                    <- the tx never applied

# 3. A third, unrelated user hits the unbacked ask.
POST /orders {"symbol":"BTC-USD","side":"buy","price":"70000.00","qty":"0.2500"}
  -> 201

node:   thread 'main' panicked at crates/cb-state/src/account.rs:40:31:
        unlock underflow
        Error: task 2 panicked with message "unlock underflow"

GET /markets/BTC-USD/orderbook
  -> {"error":{"code":"NODE_UNAVAILABLE","message":"sequencer RPC is not connected"}}
```

The node process is gone. Every step used the public REST API with an ordinary
faucet account.

`order_for_block` (`cb-block/src/ordering.rs`) ranks cancels first, then
post-only placements, then everything else — which is what guarantees the
post-only executes *before* the failing transaction, so this ordering is not
luck. Any rank-2 failure after any rank-0 or rank-1 success does it:
`FokUnfilled`, `SelfTrade`, `Insufficient`, `BadNonce`, `OrderNotFound`,
`BadTick`, `BadLot`, `WrongChainId`, `NotAuthorized`.

### Where I believe the cause is

Two seams, and either one closed would break the chain of events.

1. `cb-block/src/block.rs:121-123` — a transaction-level error is treated as a
   block-level error (§3). Nothing rolls back the direct CF writes because
   nothing was ever designed to.
2. `cb-block/src/state.rs:186-192` — the executor writes CF rows outside the
   batch so that later transactions in the same block can read them. That is a
   legitimate requirement, but it makes the block's state mutation
   non-transactional, and the code has no compensating rollback.

The panic itself (`account.rs:38-41`) is the right instinct badly placed:
`expect` in consensus code turns a corrupted-state bug into a liveness failure
for the whole network. This is F6's panic reached by an entirely different route
than the one F6 closed.

---

## 2. Anyone can become an operator, and then mint money

**Critical. Creates money. Verified live.**

### What breaks

The operator role is decided by *handle*. `AuthService::roles_of`
(`cb-gateway/src/auth/mod.rs:503-510`) compares `effective_handle(user)` against
the `admin_handles` list, from `--admin-handle` (`config.rs:77-81`, wired at
`main.rs:100`). The shipped stack passes `--admin-handle admin`
(`rust/Makefile:361`).

Nothing reserves that handle. A first login creates the account with whatever
handle was asked for (`auth/mod.rs:339-366`), and the only check is "is it
already taken" — so the first party to ask for `admin` *is* the operator.
`AuthService::rename` (`auth/mod.rs:639-663`) has the same hole for an existing
account.

From there, `POST /admin/faucet/grant` mints arbitrary amounts with no cooldown
and no cap (`faucet.rs:252-266`). The grant is a real `TxBody::Deposit` signed by
the treasury key, so the money is consensus state, not a database row.

### Smallest reproduction

```
POST /api/v1/auth/dev-login {"mnemonic":"<any valid phrase>","index":7,"handle":"admin"}
  -> {"user_id":"usr_d5120df98d84","handle":"admin","session_token":"sess_1db1…"}

GET  /api/v1/auth/session
  -> {"roles":["user","admin"], …}

POST /api/v1/admin/faucet/grant
     {"user_id":"usr_d5120df98d84","grants":[{"asset":"USD","amount":"1000000000.00"}],
      "reason":"review repro"}
  -> 200 {"claim_id":"clm_3b46e17c1547","tx_hash":"0xf73b6366…"}

GET  /api/v1/account  ->  "free":"1000000000.00"
```

One billion dollars, on chain, by a party with no prior relationship to the
system.

`--dev-login` is loopback-only (`config.rs`, `dev_login_allowed`) but it is
**not** the vulnerability — it only shortens the repro. The ordinary signature
login takes the same `establish` path and accepts the same `handle`, so any
wallet can do this against any bind address.

### Where I believe the cause is

Identity and authorisation are the same string: `auth/mod.rs:503-510` treats a
user-chosen, user-renameable display name as a capability. The role should key on
something the user cannot choose — the login address — and registration of a
configured admin handle should be refused.

Blast radius beyond minting: halt, resume, settle, risk change, suspend and
`POST /admin/sim` share the gate, and the audit trail will faithfully record the
attacker's address as the acting operator.

---

## 3. One failing transaction destroys the whole block

**Critical. Destroys transactions, stops block production. Verified live.**

### What breaks

Three facts compose.

1. `SequencerServer::submit_tx` (`cb-rpc/src/server.rs:97-142`) validates almost
   nothing — `cb_tx::validate_static` covers zero amounts, zero prices and
   deriv-id shape (`cb-tx/src/validate.rs:27-80`), then it recovers the sender
   and puts the transaction into the mempool. **No nonce check, no balance check,
   no chain-id check, no market/tick/lot check, no authority check.**
2. `run_sequencer` (`cb-node/src/sequencer.rs:74-82`) drains the mempool and
   **deletes every drained entry** before it builds anything.
3. `build_and_apply` (`cb-block/src/block.rs:121-123`) applies with `?`. **Any**
   `ExecError` aborts the entire block.

One transaction that fails at apply takes the block with it, and every honest
transaction drained alongside it is already deleted. No re-queue, no partial
block, no receipt. They are gone. And per §1, the half of the state that was
written directly stays written.

### Smallest reproduction

Two requests in the same 100 ms window — an admin force-settle (which always
fails on chain, §18) and an ordinary user's spot limit order:

```
POST /admin/markets/ETH-260927/settle {"settlement_price":"2500.00","force":true}
  -> 200 {"tx_hash":"0x1e98558e…"}
POST /orders {"symbol":"BTC-USD","side":"buy","type":"limit","price":"50000.00","qty":"0.1000"}
  -> 201 {"status":"pending","tx_hash":"0x42e22674…","order_id":"0x42e22674…"}

node:  ERROR block build failed: NotAuthorized
chain: height 2 before, 2 after. mempool_depth 0.
GET /markets/BTC-USD/orderbook -> {"bids":[],"asks":[]}
GET /account/orders?status=all -> status "pending", indefinitely.
```

The user got `201 Created`, an order id and a transaction hash for an order the
chain never saw and never will. A second reproduction does it with no privileges
at all: a fill-or-kill order against an empty book, which the gateway correctly
cannot pre-check.

### Where I believe the cause is

`cb-block/src/block.rs:121-123`. The mempool delete at `sequencer.rs:80-82` makes
it unrecoverable rather than merely slow. Recording a transaction's failure *in*
the block — a receipt — rather than propagating it out fixes this, §1 and §5
together.

This is also why the suite does not catch it: `cb-simgame`'s in-process world and
every unit test apply transactions individually, so no test ever puts a good
transaction and a bad one in one block and asks what happened to the good one.

---

## 4. The block commit is three unbatched writes

**Critical. Forks the chain on a crash. Verified by reading.**

Both the sequencer and the replica syncer end a block with **three independent,
independently-durable operations**:

1. `state.commit()` — one `WriteBatch` (`cb-block/src/state.rs:224-239`)
2. `Blocks::put` — **two** separate `db.put` calls
   (`cb-state/src/blocks_cf.rs:15-22`)
3. `Meta::set_head` — **three** separate `db.put` calls
   (`cb-state/src/meta.rs:41-49`)

Six durable writes, no batch spanning them. `cb-block/src/block.rs:124-165` and
`cb-net/src/syncer.rs:222-246` have the identical pattern.

The author demonstrably knew this mattered: the *fast-mirror* path puts all three
in one `WriteBatch` and says why — "a crash leaves the node at either height or
height-1 — never half-applied" (`cb-state/src/mirror_blocks_cf.rs:41-68`). That
care was never applied to the verifying path.

**On a replica:** kill the process between (1) and (3). On restart the state
contains block N while `head_height` says N−1. The node re-applies N onto state
that already has it, gets a different root, and rejects it as
`StateRootMismatch` — forever. It never advances, and it scores down and bans
every honest peer that serves it the correct block (−40 each, three strikes).

**On the sequencer, worse:** same crash. On restart the state has N, head says
N−1, and `build_and_apply` builds a *different* N′ on top of state-N. Replicas
already at N reject N′ as `NonMonotonic` and everything after it as
`StateRootMismatch`. **The chain forks at the sequencer.**

The database is opened with default RocksDB write options — no `set_use_fsync`,
no explicit sync (`cb-state/src/db.rs:212-232`) — so this survives a power loss
even less well. A plain `kill` is enough to trigger it.

The fix shape already exists in the repository, in `mirror_blocks_cf.rs:48-68`.

---

## 5. One failed transaction bricks an account permanently

**Critical. Verified live.**

`TxSubmitter::submit` (`cb-gateway/src/signer.rs:82-115`) advances its cached
nonce on `SubmitOutcome::Accepted`. But `Accepted` means *the mempool took it*,
not *the chain applied it*. When the transaction later dies at block build (§3),
the chain's nonce never advances and the gateway's cache is one ahead. Every
subsequent transaction from that account is signed with a nonce the chain will
reject, is accepted into the mempool anyway, and kills the block it lands in.

The module documentation (`signer.rs:18-22`) says the sequencer reports
`badNonce` and the submitter re-reads once. That is unreachable: the RPC server
can return only `ok`, `InvalidAmount` or `BadSignature` (`cb-rpc/src/server.rs:107,
115, 140`). `TxSubmitter::invalidate`, the escape hatch, **is never called
anywhere** — I grepped the whole crate. The state is permanent for the life of
the gateway process.

```
POST /transfers x3  -> 200, 200, 200   (each with a tx_hash)

node:  block build failed: BadNonce { expected: 0, got: 2 }
       block build failed: BadNonce { expected: 0, got: 3 }
       block build failed: BadNonce { expected: 0, got: 4 }

chain height: unchanged.  recipient balance: unchanged.
```

Each `200 OK` also destroyed a block, so this is not only self-inflicted: one
bricked account, held by a bot or a retrying client, denies block production to
everybody. Until §3 is fixed, the honest behaviour is to re-read the nonce when a
submitted hash has not appeared within a window, or not to cache it.

---

## 6. F12: funding clamped per hour, accrued per block

**Critical. Known and open (PLAN §8). Verified by reading.**

* `FundingClamp::DEFAULT` documents `outer_q9` as "±0.5% **per hour**"
  (`cb-engine/src/funding.rs:32-44`; module header repeats it at `:7`).
* The executor accrues `rate * elapsed_blocks` (`cb-block/src/executor.rs:641-644`,
  helper at `funding.rs:90-98`), commenting that the protocol treats the rate as
  **per block**.
* Settlement lands on a 3,600-block boundary (`executor.rs:654-655`; genesis sets
  `funding_interval_blocks: 3600` at `cb-genesis/src/build.rs:544`).

**One correction to PLAN §8's arithmetic**, worth having right before anyone
decides this. The *shipped genesis* clamps at `funding_rate_cap_q9: 1_000_000`
(0.1%), applied at `executor.rs:638-639`. So the ceiling as shipped is
0.1%/block = **360% per hour**, not the 1,800% the plan quotes — the plan quotes
the protocol clamp, and the genesis clamp is tighter. Still catastrophic.

### What else it touches

There is a **third**, independent declaration of the unit, and it is the
canonical one: `FundingIndex.rate_q9` is documented as "Latest 1-hour funding
rate" at `cb-types/src/derivatives.rs:193-194`. Every downstream reader inherits
that claim from the type.

| Consumer | Where | Unit it implies |
|---|---|---|
| Accrual (the money move) | `executor.rs:644` | per block |
| Ticker `funding_rate` | `cb-gateway/src/http/market.rs:116-118` | bare number |
| "Next funding at" | `http/market.rs:80-85`, `markets.rs:254-256` | **per interval (3600 s)** |
| Funding history | `watcher.rs:317-326` → `http/market.rs:242-255` | per block, labelled as rate history |
| Type documentation | `cb-types/src/derivatives.rs:193` | **per hour** |

The sharpest disagreement is *inside one JSON object*: `GET
/markets/{symbol}/ticker` returns a per-block rate next to a one-hour
`next_funding_at`. A trader reading it concludes they will pay *r* over the next
hour. They will pay *r* × 3,600.

Two more consumers make it undetectable in advance:

* The displayed liquidation price has **no funding term at all**
  (`cb-gateway/src/http/account.rs:413-436`). A position funding is about to
  destroy shows an unchanged liquidation price until it is wiped.
* `funding_paid` on a position is **hardcoded `"0.00"`** (`http/account.rs:211`).
  A trader charged millions sees zero.

Bad debt (`executor.rs:1111-1123`) and the insurance fund
(`executor.rs:1744-1785`) inherit the per-block reading correctly, which is
exactly why the fund drains 3,600x faster than the parameters imply.

I agree with the plan that reading 2 (rate is per interval, accrual is wrong) is
the intent, and that it is a genesis-level decision rather than a patch.

---

## 7. `GET /account` omits both contract addresses, and it disables the trade button

**Critical. Verified live (the response) and by reading (the consequence).**

| | `login_address` | `trading_address` | `address` |
|---|---|---|---|
| Contract §5 | required | required | — |
| Client `cb-web/src/api/types.rs:519-534` | required `String` | required `String` | — |
| Mock `api/mock.rs:524-533` | ✔ | ✔ | — |
| **Gateway `http/dto.rs:462-472` + `http/account.rs:92-95`** | ✘ | ✘ | ✔ (holding the *trading* address) |

Confirmed live — the `GET /account` body in §2's transcript is
`{"address":"0x8F29…","nonce":0,"balances":[…],"portfolio":{…}}`, with neither
contract field.

`serde_json::from_str::<Account>` fails on the missing `login_address`, so
`api/http.rs:86` returns `ApiError::Decode`, and `lib.rs:251-253` swallows it
with `if let Ok`. `stores.account` stays `None` forever. Then:

```rust
// screens/terminal.rs:1183-1189
let available = move || {
    stores.account.read().as_ref()
        .map_or(Decimal::ZERO, |account| account.portfolio.available_margin)
};
```

`None` → `Decimal::ZERO` → `ticket::validate` (`ticket.rs:218-223`) returns
`Blocked::Margin { available: 0 }` for every non-zero order → `can_send()`
(`terminal.rs:1214`) is false. **The Place Order button is disabled for every
order in every market, under the hint "Needs 32,125.25 USD of margin, 0.00
free".** The portfolio equity panel, the equity curve, the balances list and the
account risk gauge go dark with it.

The REST API itself works — I placed and matched real orders with curl. It is the
browser client that cannot.

This is the clearest instance of the project's known failure pattern. The client
*and the mock* implement the contract faithfully; the gateway implements a
pre-two-address reading and names the field `address` while filling it with the
trading address. Contract §2 spends twenty lines explaining why there are two
addresses and why the distinction must be "visible in the API rather than
hidden" — and the gateway hides it. Both halves are internally consistent;
neither matches the other.

---

## 8. The `ticker` frame is missing three of five fields

**Critical. Verified by reading.**

* *The server violates the contract.* The ticker update carries only `symbol` and
  `last_price` (`watcher.rs:288-298`). The contract (§8) specifies `mark_price`,
  `change_24h_pct` and `funding_rate` as well.
* *The client cannot tolerate that.* `TickerUpdate.mark_price` and
  `.change_24h_pct` are plain `Decimal` with no default
  (`cb-web/src/ws/msg.rs:58-71`); only `funding_rate` has one.

So every live `ticker/update` frame decodes to `Unknown`. `stores.apply_ticker`
(`state.rs:433-449`) is the only writer of `Quote.at_ms`, so the quote is seeded
once from REST at bootstrap (`lib.rs:233-235`) and **never updated again**. After
five seconds `is_stale()` (`state.rs:92-94`) goes true and stays true:

* every price on the terminal wears the stale treatment permanently;
* the tick flash never fires;
* `reference()` — the price a **market order** is validated and sized against
  (`terminal.rs:1190-1194`, `ticket.rs:196`) — is frozen at page-load value;
* and the connection chip still reads **Live**, because `last_frame_ms` is bumped
  by every frame including pongs (`state.rs:523`, `ui/mod.rs:137-140`).

The indicator and the price disagree, and the indicator is the one users trust.
Separately, the ticker is published only when a *derivative* market prints a
trade (`watcher.rs:235, 287`), so spot markets never emit one at all.

---

## 9. A faucet account can never trade a derivative

**High. Verified live. Not previously recorded.**

Spot balances and margin collateral are two different ledgers, and no transaction
moves value between them.

* The faucet's `TxBody::Deposit` credits the **spot** `AccountState`
  (`cb-block/src/executor.rs:285-287`).
* Perp and futures margin is `MarginAccount.collateral`, a different column
  family. Every write to it in the executor (`executor.rs:752, 1066, 1237-1270,
  1715-1738, 1761-1763, 2102`) is a fill, funding, fee, liquidation or
  settlement. It is **created** only by genesis (`cb-genesis/src/build.rs:209,
  258`).
* `TxBody::AdjustMargin` — the one transaction that would bridge them — returns
  `ExecError::Unsupported` (`executor.rs:402-404`).

So every account created through the gateway has exactly zero collateral,
forever, and `passes_initial_margin` can never be satisfied. PLAN §3's player
loop — "open leveraged perpetual positions, hold a future to expiry" — is
unreachable for every account that arrives through the front door. Eleven of the
seventeen listed instruments are perps and futures.

```
POST /faucet/claim  -> granted 100000 USD + 1 BTC
GET  /account       -> "equity":"74975.00", "available_margin":"0.00"
POST /positions/margin {"symbol":"BTC-PERP","action":"add","amount":"1000.00"}
  -> 404 {"code":"NOT_FOUND","message":"no open position on BTC-PERP"}
```

The margin endpoint cannot help: it requires an open position to add margin to,
and you cannot open one without margin.

Either the faucet should mint collateral as well as spot balance, or
`AdjustMargin` needs implementing as a spot→collateral transfer. The derivative
side of the product is currently exercisable only by genesis-seeded accounts —
which is exactly the population `cb-simgame` uses, which is why no test notices.

---

## 10. A spot fill never updates the order

**High. Verified live.**

`OrderTracker::apply_fill` — the only thing that moves an order past `open` — is
called from one place: `watcher::record_position_fill` (`watcher.rs:554, 558`).
That function is driven by **position deltas** (`watcher.rs:448-477`), which
exist only on derivative markets. A spot fill changes balances and nothing else.

```
maker: POST /orders {"symbol":"BTC-USD","side":"sell","price":"50000.00","qty":"0.5000"}
taker: POST /orders {"symbol":"BTC-USD","side":"buy", "price":"50000.00","qty":"0.5000"}

chain:   maker BTC 1.0 -> 0.5, USD 100000 -> 125000.   The trade happened.
book:    {"bids":[],"asks":[]}                          Both orders are gone.
gateway: both orders "status":"open","filled_qty":"0.00000000"
         /account/fills -> {"fills":[]}

maker: DELETE /orders/0x2ae5e27a…  -> 200 {"status":"cancelled"}
node:  ERROR block build failed: OrderNotFound
```

The user cancels the order the UI still shows them.
`apply_cancel_spot_order` returns `ExecError::OrderNotFound`
(`executor.rs:2570`) — unlike the perp path, which is an idempotent no-op
(`executor.rs:1363-1373`) — and by §3 that kills the block.

The cause is `watcher.rs:426-482`. The module documentation (`watcher.rs:18-30`)
argues at length that fills must come from the user's own state rather than from
trade prints — a good argument that happens to cover only half the markets.
Balance deltas on a spot market have the same property and are not used.

---

## 11. The faucet mints before it records

**High. Creates money. Verified by reading.**

`FaucetService::mint` (`cb-gateway/src/faucet.rs:269-343`) submits one
`TxBody::Deposit` per granted asset in a loop (`283-328`), then writes the claim
rows and the ledger (`332-335`). The comment at `330-331` says the ordering
exists "so a rejected deposit never starts a cooldown the user got nothing for" —
sound for the failure it names, and wide open to the opposite one:

* **The loop fails halfway.** The default grant is two assets
  (`faucet.rs:83-92`). If the first `Deposit` is accepted and the second is not,
  the user has 100,000 USD on chain, **no claim row, and therefore no cooldown**.
  They retry and get another 100,000. Repeat.
* **The gateway dies between the mints and the writes.** Same outcome, both
  assets.
* **`record_claim` or `append_many` fails.** Same outcome: `?` at
  `faucet.rs:333` returns `Err` after the money is on chain. This is exactly the
  shape of the bug the regression test at `faucet.rs:476-510` documents — a
  multi-asset grant that minted and then failed its own bookkeeping — repaired
  for the primary-key collision but not for the class.

There is no on-chain defence. The executor destructures `Deposit`'s
`external_tx_ref` away with `..` (`executor.rs:274-289`), so the `claim_id` that
"ties the on-chain mint back to the off-chain grant record" (`faucet.rs:287-289`)
is never read by the chain. **The deposit is not idempotent.**

A weaker version always applies: `submitter.submit` returns on *mempool*
acceptance, so "Recorded only after the chain accepted the mints"
(`faucet.rs:330`) is not true. If the deposit then dies at block build (§3), the
cooldown starts and the user got nothing — the failure the ordering was chosen to
prevent, arriving by the other door.

### Which ordering is correct

Record first, mint second, reconcile third. Record-first can lose at most one
cooldown window; mint-first can lose unbounded money. Better still, make
`Deposit` idempotent on `external_tx_ref` in the executor, at which point the
ordering stops mattering and a retry becomes safe.

### Can two devices claim at once

**Within one gateway process, no — and that part is correct.** The per-user
`tokio::Mutex` spans read, submit and record (`faucet.rs:209-243`), demonstrated
by the test at `faucet.rs:551-588`. The lock is a `DashMap` in process memory, so
two gateway instances against one database would both grant; the deployment is
single-instance, so this is a scaling constraint rather than a live defect.

---

## 12. The state-root check runs after the state is written

**High. Verified by reading.**

`cb-net/src/syncer.rs:14-17` states the guarantee the whole design rests on:

> Every variant is a halt: partial state is never persisted, so a node that
> returns one of these is exactly where it was before the block arrived.

That is false for every variant raised after the transaction loop begins.
`apply_remote_block` runs `apply_tx` for every transaction — writing order-book,
position, margin, oracle and market rows **directly to RocksDB**, per §1 — then
calls `state.commit()`, which writes the four tries in a batch
(`syncer.rs:222-224`), and **only then** computes and compares the state root
(`syncer.rs:231-246`).

On `StateRootMismatch` it returns `Err` with the account trie, positions trie,
markets trie, margin trie and every directly-written CF row already committed,
while `Meta` still points at the previous height. The replica has adopted the
state of a block it just rejected.

It then re-applies the *correct* block N onto corrupted state, gets a different
root, rejects that as `StateRootMismatch` too, and bans every honest peer that
serves it — 40 points each, three strikes. It never syncs again.

The proposer-signature and height checks do run first (`syncer.rs:102-128`), so
reaching this needs a block signed by the sequencer's key. That is not a
comfortable bound: catching a divergent or buggy leader is the entire reason the
re-execution check exists (PLAN §11), and §4 gives a leader a mundane, non-
Byzantine way to produce one after a crash. A non-deterministic executor bug —
the thing the determinism tests exist to catch — triggers it on an honest
network.

**The check the design exists to preserve is performed after the write it is
supposed to gate**, and the module header asserts the opposite.

---

## 13. A peer can exhaust a node's memory, and stall its sync un-bannably

**High. Verified by reading.**

One root cause with two consequences. `on_block_body`
(`cb-net/src/p2p/engine.rs:828-840`) treats a body as solicited if *any* block
request to that peer is outstanding — it never checks the delivered body against
the requested hash. `buffer_body` then keys the buffer on the **peer-supplied
`height` field** (`engine.rs:887-904`).

**Memory.** The only bound is `pending_bodies.len() >= max_pending_blocks`
(`engine.rs:897`): 256 entries × up to `MAX_FRAME_BYTES` (64 MiB,
`codec.rs:31`) ≈ **16 GiB**. `config.rs:126-127` documents the knob purely as a
count and never mentions bytes. Sequence: handshake, then 256 times, announce a
fresh random hash at `head+1` (passes the novelty check at `engine.rs:797`) and
answer the resulting `GetBlock` with a 60 MB body labelled with a distinct
height. Zero score penalty for all 256 rounds; only the 257th is penalised, by
which point the memory is committed.

**Sync stall.** Answer `GetBlock` with a *real old block* labelled
`height: head+1`. `buffer_body` inserts it at slot `head+1`, **overwriting any
honest body already waiting there** (a `BTreeMap` insert). The verifier rejects
it as `NonMonotonic` → `RejectReason::OutOfOrder` (`chain.rs:74`) →
`Behaviour::Duplicate` (`engine.rs:117`) → **−1, and `is_benign` clamps the score
back above the ban threshold forever** (`peer_table.rs:256-258`). The attacker
occupies the single `verifying_block` slot and races honest bodies out of
`head+1` indefinitely, and can never be banned for it.

Fixing one seam — bind "solicited" to the requested hash, and bound
`pending_bodies` by bytes as well as count — closes both.

---

## 14. A revoked or expired session keeps receiving private data

**High. Verified by reading.**

`Hub::authenticate` writes `user_id` into the connection once
(`cb-gateway/src/ws.rs:263-268`) and **nothing ever clears it** — there is no
`deauthenticate`, and `user_id` is assigned in exactly one place.
`publish_to_user` matches on that stored string and never consults `AuthService`
(`ws.rs:345-361`). `AuthService::authenticate` does check `revoked` and
`expires_at` (`auth/mod.rs:476-481`), but only from `apply_op`'s `Auth` arm
(`ws.rs:484`).

Grepping `hub.` across the gateway shows the only mutating use outside `ws.rs` is
`close_all()` at shutdown (`main.rs:204`). `POST /auth/logout`, `DELETE
/user/sessions/{id}`, `POST /user/sessions/revoke-all` and admin suspend all
leave open sockets untouched.

**Reproduction:** sign in on tab A; from tab B call
`POST /user/sessions/revoke-all`. Tab A's REST calls return 401, but its socket
keeps receiving `account` fills, balances and positions indefinitely — the client
pings every 30 s (`cb-web/src/ws/mod.rs:52`), so the 90 s idle close never fires.
A session expiring mid-connection behaves identically.

**There is no re-check of the session after the initial auth.** "Revoke all
sessions" is a security feature that does not revoke the channel private data
actually flows over.

---

## 15. The gateway fabricates a fill for every open position on mirror clear

**High. Fabricated financial record. Verified by reading.**

`refresh_account` (`watcher.rs:437-458`) guards the *balance* publish behind
`if let Some(previous) = &previous_account` (`:440-444`) but does **not** guard
positions. When the mirror has no entry, `previous_positions` is
`unwrap_or_default()` → empty (`:438`), so `previous_qty` is 0 (`:449-452`). Any
existing position with `qty != 0` fails the `continue` at `:453`, and
`record_position_fill` runs with `previous_qty = 0`, computing
`delta = position.qty` (`:499`). That publishes an `account/fill` envelope
(`:563-582`) and writes a `FillRecord` into the tracker (`:554`), which is what
`GET /account/fills` reads.

The mirror is empty at gateway start-up, on `NodeEvent::Connected`
(`watcher.rs:79`) and on `RecvError::Lagged` (`watcher.rs:88`).

**Reproduction:** a user holds 1.5 BTC-PERP and has a live order. Restart the
gateway. One block later a fabricated 1.5 BTC "taker" fill appears in
`/account/fills`, with an invented fee and `realized_pnl: 0`.

It compounds: that fill feeds `volume_24h`, `trades_24h` and `fees_24h` on the
operator dashboard (`http/admin.rs:156-171`) and the leaderboard. Every restart
inflates them. The user-facing toast is suppressed today only by §16(c) — fix the
frame shape without fixing this and every restart shows every trader a "filled"
notification for a trade that did not happen.

---

## 16. Every `account` WebSocket frame fails to decode

**High. Verified by reading.**

`cb-web` builds `rust_decimal` with `serde-str` and without `serde-float`
(`cb-web/Cargo.toml:43`), so every `Decimal` field must arrive as a JSON
**string** and every field without `#[serde(default)]` must be present. Four
independent mismatches:

**(a) `balance_update` — wrong envelope shape.** Server sends
`{"balances":[…]}` (`watcher.rs:622-625`); client deserializes `data` into a bare
`Vec<Balance>` (`ws/msg.rs:140, 213-215`). Object against array.

**(b) `position_update` — 4 of 15 required fields.** Server sends `symbol`,
`side`, `qty`, `entry_price` (`watcher.rs:636-645`). Client's `Position` requires
fifteen with no defaults (`api/types.rs:752-781`).

**(c) `fill` — missing `realized_pnl`.** The server computes it into the
`FillRecord` (`watcher.rs:548`) and omits it from the envelope
(`watcher.rs:568-580`); the client's `Fill.realized_pnl: Decimal` has no default
(`types.rs:871-872`). Everything else in that payload matches field for field.

**(d) `order_update` and `liquidation` are emitted by nobody.** The contract
defines both (§8). There are exactly three `publish_to_user` call sites —
`watcher.rs:563` (fill), `:622` (balance_update), `:634` (position_update). The
client implements both handlers (`ws/msg.rs:207-208, 216-218`; UI at
`state.rs:564-575, 609-624`). Documented in one layer, implemented in neither.

Each failure lands in `ServerMessage::Unknown`, which logs and drops
(`ws/msg.rs:158-175` — a correct design, undone by what it has to drop). A client
watching the socket never learns its order went `pending → open`, filled, or was
cancelled, and never learns it was liquidated. Combined with §7 and §33, on a
live gateway the account channel delivers nothing usable and the REST fallbacks
mostly cannot decode either.

---

## 17. Accounts with no resting order are never refreshed

**High. Verified by reading.**

The documentation of `accounts_to_check` (`watcher.rs:384-390`) says: "A user
with a live order **or an open position** can be affected by anybody's
transaction, so they are checked on every block." The code (`watcher.rs:391-396`)
asks the tracker only for `addresses_with_live_orders()`. A user with an open
position and no resting order is checked **only** if they sent a transaction in
that block.

A liquidation is triggered by a keeper, so the victim is neither the sender nor in
`candidates` (`watcher.rs:400-411`). A funding tick is sent by the proposer. The
two events that move a position holder's money without their participation are
exactly the two the watcher cannot see: no `position_update`, no
`balance_update`, no fill row, no ledger entry.

Downstream, `liquidations_24h` on the operator dashboard counts ledger entries of
kind `liquidation` (`http/admin.rs:272-290`), and nothing ever writes one. It
counts zero forever, as does the user page's `liquidations` stat
(`http/user.rs:296-300`).

The check is documented in the comment directly above the code that does not
implement it.

---

## 18. Force-settle sends no price, always fails, and audits a lie

**High. Verified live.** Three defects in one handler
(`cb-gateway/src/http/admin.rs`, `settle_market`).

1. **The settlement price is never transmitted.** The handler parses
   `settlement_price`, checks it is within 10% of the mark, and submits
   `TxBody::FuturesSettle { market }` — a variant with **no price field**
   (`cb-tx/src/tx.rs:107-109`). The chain computes its own price from the oracle
   and the TWAP (`executor.rs:2058-2061`). The contract says settle "closes every
   position at the given price". It does not.
2. **It is signed by the wrong key, so it always fails.** The code comment says
   "the gateway submits it from the faucet treasury". The code uses
   `state.auth.trading_key_of(&admin.0.user)` — the operator's own derived
   trading key. `apply_futures_settle` requires `sender == ctx.proposer_addr`
   (`executor.rs:2036-2038`), which can never match, so it always fails
   `NotAuthorized` — and by §3 destroys the block it lands in.
3. **The audit record says it happened.** `audit(… "market.settle" …)` is written
   unconditionally after submission, carrying the settlement price and the
   transaction hash. The one place an investigation would look records a
   settlement that did not occur, at a price that was never used.

Reproduction: the transcript in §3.

---

## 19. Halt and risk changes never reach the chain

**High. Verified by reading.** PLAN §8 acknowledges the persistence half; the
correctness half is worse.

`halt_market`, `resume_market` and `set_risk` all call
`state.markets.amend_override(...)`, which writes to
`overrides: RwLock<BTreeMap<String, MarketOverride>>` (`markets.rs:159`).
In-process memory.

* **A halt halts nothing.** The gateway's own order validation refuses placement
  on a halted market (`orders.rs:202-212`), and that is the only enforcement.
  Bots, `cb-client`, `cb-simgame live` and anyone with a socket to the
  sequencer's RPC keep trading the "halted" market at full speed. The operator
  sees `status: "halted"` and a book that keeps moving.
* **A risk change changes no risk.** The override feeds the gateway's
  pre-submission check and the displayed liquidation price; the executor gates on
  a hardcoded tier table and reads neither number (§22). So
  `accounts_now_below_maintenance` names accounts the chain will never liquidate,
  against a threshold the chain does not have. The contract presents that list as
  "who this breaks"; it is "who this would break if the chain implemented it".
* **Both vanish on restart**, with no audit record of the resume.

The `dry_run` path is well designed and I want to say so: it computes the same
answer and changes nothing, and `affected_accounts` takes the proposed bps as a
parameter so the two cannot drift. The problem is where the write goes.

---

## 20. The gateway writes ledger entries the chain never applied

**High. Verified live.**

`POST /transfers` (`http/trading.rs:576-595`) appends a `transfer_out` ledger
entry immediately after submission, with a `balance_after` computed from the
pre-submission free balance. If the transaction never lands — and after §5 it
never will — the entry is permanent and wrong.

```
POST /transfers {"to":"0xC67C…","asset":"USD","amount":"1234.00","memo":"survives?"}
  -> 200 {"tx_hash":"0xe38d8016…"}
node:  block build failed: BadNonce { expected: 0, got: 1 }
chain: sender still 1000000000.00, recipient still 100000.00

GET /account/transfers
  -> {"kind":"transfer_out","amount":"-1234.00","balance_after":"999998766.00", …}
```

`/account/transfers` and `/user/activity` are what a person reads to answer
"where did my money go". They show a transfer that never happened and a balance
that never existed.

The faucet's entry has the mirror problem: it writes `balance_after: 0` with the
comment "Zero reads as 'not observed'" (`faucet.rs:320-322`). The contract
defines `balance_after` as the balance after the entry; nothing on the wire
distinguishes "not observed" from "you have nothing". There is also no
`transfer_in` entry for the recipient anywhere, so the contract's `kind`
vocabulary has a member the gateway never produces.

---

## 21. Rotating the server secret is silent and destructive

**High. Verified by reading.**

The trading key is `HKDF(server_secret, login_address)` and is never stored
(`auth/mod.rs:394-405`). Nothing records a user's trading address, so there is
nothing to compare a newly derived one against.

* Every user's `trading_address` changes. `GET /account` reads the *new* address,
  finds a fresh empty account, and returns `free: "0.00"` with **HTTP 200 and no
  error of any kind**. The money is still on chain under the old address,
  reachable only by restoring the secret. Completely silent.
* Any resting order placed before the rotation becomes uncancellable: the tracker
  still shows it, but `DELETE /orders/{id}` signs with the *new* key and
  `apply_cancel_spot_order` rejects a non-owner with `NotAuthorized`
  (`executor.rs:2571-2573`) — which, by §3, kills the block.
* `AuthService::new` rejects only an *empty* secret (`auth/mod.rs:189-194`), with
  a message warning that otherwise "every deployment would derive the same
  trading accounts". The shipped `rust/.env` sets
  `GATEWAY_SERVER_SECRET=local-development-secret-do-not-reuse` — a constant
  committed to the repository. The guard is nominal and the condition it warns
  about is the shipped default. Anyone who has read this repository can derive
  any user's trading key from their public login address and sign transfers as
  them directly against the node's RPC, with no session and no gateway. For a
  simulator with test money that is defensible, but nothing warns at startup and
  the `.env.example` block that carefully explains the published genesis keys
  says nothing about this line.

The runbook says not to rotate. The system should say so too, at startup — for
instance by recording each trading address at first login and refusing to start
when the derivation no longer reproduces it.

---

## 22. Every genesis risk parameter is decorative

**High. Verified by reading.** PLAN §8 states this; the extent is larger.

`apply_place_perp_order` gates on
`passes_initial_margin(&margin_account, &proposed, &marks, DEFAULT_TIERS)`
(`executor.rs:869`), and `DEFAULT_TIERS` is a hardcoded constant
(`cb-engine/src/margin.rs:94-119`). The genesis parameters
(`cb-genesis/src/build.rs:123-130`) are read by nothing on the trading path:

* `initial_margin_bps`, `maintenance_margin_bps` — zero references in
  `cb-block/src` or `cb-engine/src`.
* `lot_size` — never read; `margin.rs:139-141` says so.
* `max_leverage_x100` — never read.
* `max_position_notional` — **zero references on any execution path**.
* `tick_size` — enforced on spot (`executor.rs:2319`) and **not** on
  derivatives: `apply_place_perp_order` checks only `qty == 0 || price_q8 == 0`
  (`executor.rs:819-821`).

The genesis dated future `btc_2026q1` is listed with 25x max leverage, 4% initial
and 2% maintenance margin (`build.rs:546`). The chain gates it at tier 1 of
`DEFAULT_TIERS` — 2% initial, 1% maintenance. **50x on a market advertised as
25x, with half the maintenance buffer the market record claims.** The gateway
copies the genesis numbers into `MarketMeta` (`markets.rs:252-253`) and shows
them to users, so the displayed liquidation price and the engine's actual trigger
are computed from different parameters.

---

## 23. Consensus-critical state outside the state root

**High. Structure verified by reading; exploitability suspected.**

`compute_state_root` hashes four tries — accounts, positions, markets, margin
(`cb-state/src/lib.rs:102-114`) — matching `BlockState::commit`
(`cb-block/src/state.rs:229-237`). Outside the root:

* `CF_FUNDING_INDICES` — the cumulative index driving every funding collateral
  move.
* `CF_ORACLE_STATE` — oracle prices, marks, and the mark history that determines
  a dated future's settlement price.
* `CF_PERP_ORDERS` and `CF_PERP_BOOK_INDEX` — the entire derivative order book.
* The liquidation queue.

This is F3's shape one layer out. Two replicas can hold different resting perp
orders, different funding indices and a different mark history and report
byte-identical state roots. It is also what makes §1 invisible: the unbacked
spot order lives in `CF_ORDERS`, which no root covers.

I verified the structure. I did not construct a divergence, so whether it is
reachable in practice — as opposed to structurally possible — is **suspected**.

---

## 24. No mark-staleness check on the consensus path

**High. Verified by reading.**

`check_freshness` (`cb-engine/src/oracle.rs:27-40`) compares an update's
`submitted_height` only against the *previously stored* oracle's source height.
It never compares against the current block height, and there is no maximum age
anywhere in `cb-engine` or `cb-block`. `collect_marks_for_positions`
(`executor.rs:1292-1308`) reads whatever mark is on disk.

A staleness check does exist — in the gateway (`state.rs:136-142`, applied at
`http/trading.rs:370` and `orders.rs:338-343`). So the gateway refuses to
*submit* an order against a stale mark while the chain will *gate margin on* and
*liquidate against* one. Anyone bypassing the gateway trades against an
arbitrarily old price.

Worse in the other direction: `is_liquidatable` returns `false` when the
maintenance requirement is zero (`liquidation.rs:103-105`), and
`maintenance_margin_required` skips positions on markets with no mark
(`margin.rs:336-339`). **An account whose only position is on a market that has
lost its oracle cannot be liquidated at all**, however deep underwater.

---

## 25. No circuit breaker on a dated future when the insurance fund empties

**Verified by reading. Not previously recorded.** When the fund cannot cover bad
debt, `absorb_bad_debt` should halt the market to reduce-only
(`executor.rs:1769-1784`). It resolves the market through
`PerpMarkets::new(db).get(&market)` (`executor.rs:1775-1776`), which returns
`None` for a dated future — so the halt silently never fires. Same perp/future
split the twelfth defect fixed in `apply_liquidation_trigger`
(`executor.rs:1495`), left unfixed here. And even if it fired,
`MarketView::paused_reduce_only_only()` hardcodes `false` for `Future`
(`executor.rs:1843`). A dated future has **no insurance-exhaustion circuit
breaker at all**.

## 26. The displayed liquidation price is decoupled from real collateral

**Verified by reading. Not previously recorded.** `http/account.rs:174-179`
computes a position's "margin" as its isolated margin when `margin_mode == 1`,
and otherwise — for **every position the executor creates**, since
`apply_fill_to_position` defaults to cross (`perps_position.rs:113-132`) —
synthesises it as exactly the initial-margin requirement. Substituting into
`liquidation_price` (`account.rs:413-436`), the buffer reduces to
`mark * im_bps / 10_000`: a **fixed percentage from entry, independent of posted
collateral**. A trader with ten times the required collateral is shown a
liquidation price about 1% away. For the same reason,
`leverage: format_ratio(notional, margin)` (`account.rs:205`) is the constant
`10_000 / initial_margin_bps` — **50.00x on every cross perp position, always**.
It also ignores cross-margin PnL, `bad_debt`, and funding (§6).

## 27. F11: resting perp orders reserve nothing

**Known and open. Verified by reading.** `apply_place_perp_order`
(`executor.rs:773-969`) takes no lock and debits no margin for the resting
remainder; the gate (`executor.rs:857-872`) simulates one order against *current*
positions via `simulate_worst_case_position_set` (`executor.rs:1145-1177`), which
never reads the order book. With equity `E`, submit `N` non-crossing orders each
requiring at most `E`; each passes independently; the resulting position is
roughly `N × 50E`.

I rate this higher than the plan does because of what does **not** bound it:
`max_position_notional` and `max_leverage` are never read, and there is no cap on
resting orders per owner (§22). The binding constraint is book depth.

**A trap for whoever closes it:** settlement (`executor.rs:2107-2115`) and
liquidation (`executor.rs:1558-1564`) both cancel resting perp orders without
releasing anything, which is correct today and becomes a collateral leak the day
per-order reserves exist.

## 28. Pre-handshake connections are unbounded and never reaped

**Verified by reading.** The accept loop has no connection limit
(`transport.rs:177-190`). `on_connected` checks
`table.connected_count() >= max_peers` (`engine.rs:476`), but `connected_count`
is `by_connection.len()` (`peer_table.rs:92`), populated only in `attach`, which
runs **after a successful handshake** (`peer_table.rs:180`, called from
`engine.rs:723`). And `stale_connections` iterates `table.records`
(`peer_table.rs:322-328`), so a connection with no record can never time out.
`ConnState` (`engine.rs:255-263`) carries no handshake deadline.

**A peer that connects and never sends `Hello` is invisible to every limit and is
never reaped.** Each costs a socket, an FD, two tokio tasks and an unbounded
writer channel. It is the cheapest attack in the crate: one packet, no crypto, no
protocol knowledge.

## 29. Serving a block range blocks the p2p event loop

**Verified by reading.** Unlike `VerifyBlock`, which is correctly moved to
`spawn_blocking` (`transport.rs:314-339`), `Action::ServeBlockRange` reads up to
128 blocks from RocksDB **synchronously inside `exec`, holding the chain mutex**
(`transport.rs:368-383`). `exec` runs on the single engine task, so every other
peer stalls behind it, and the reply is queued into an **unbounded** writer
channel (`transport.rs:419`). A 20-byte `GetBlockRange` costs the victim 128 disk
reads and up to a full 64 MiB frame; the rate limit permits 2,000 per second.
The engine stalls, `peer_timeout` fires on every honest peer, and the node
self-partitions. The contrast with `VerifyBlock` is what makes this look like an
oversight rather than a policy.

## 30. A panic in the verifier stops sync forever, silently

**Verified by reading.** `verifying_block` is set at `engine.rs:919` and cleared
only on `BlockAccepted` (`:938-940`) or `BlockRejected` (`:994`). Nothing in
`on_tick` resets it. If the `spawn_blocking` verifier panics, no event is ever
sent, and both `pump_verify` (`engine.rs:914-916`) and `maybe_request_range`
(`engine.rs:1278`) return early forever. The node stops syncing, permanently,
while still answering pings and looking healthy. This is reachable: all of
`cb-state` uses `.expect("rocks")`, so any transient RocksDB error inside
`verify_and_apply` panics the task — and §1 shows the executor panicking on
ordinary trading data.

## 31. `transport.rs` documents a mutex the sequencer never takes

**Verified by reading.** `transport.rs:132-134` says the chain mutex is "shared
with the caller because the node's own block producer writes to the same
database". `run_sequencer` **never takes it** — it writes `db` directly
(`sequencer.rs:73-111`). The `Arc<Mutex<DbChain>>` is built at
`cb-node/src/p2p.rs:126` from a *clone* of the same `Database`, and the shipped
p2p cluster runs the sequencer with `--p2p-listen`
(`scripts/supervisord/supervisord_p2p.conf:68`), so both writers are live as
configured. A reviewer reading that comment concludes a race is closed that is
not. The realistic trigger chains off §4: a sequencer that crashed mid-commit
restarts at `head = N−1`, a replica legitimately at N announces, and
`verify_and_apply` re-executes block N concurrently with the sequencer's own
`build_and_apply` on the same tries. **Absent mutex verified; reachability
suspected**, since it depends on §4 firing first.

## 32. The first sign-in with an imported phrase always fails

**Verified by reading.** The contract says a first login without a handle returns
`422 HANDLE_REQUIRED` and the client "should retry with a handle". The client
does retry — with **the same challenge object** (`landing.rs:822, 829, 844`) —
and the gateway consumed that nonce before it ever looked at the handle
(`auth/mod.rs:264` precedes the check at `:339-340`; `challenge.rs:75-78` removes
unconditionally).

Attempt 2 therefore gets `401 CHALLENGE_EXPIRED` with the body *"request one from
POST /auth/challenge and sign it within five minutes"* — a message that tells a
trader to run curl. Worse, `needs_handle()` string-matches the literal
`"needs a handle"` (`landing.rs:819-821`), which the new message does not
contain, so the handle field **disappears** on the second failure while the typed
handle stays in the signal. The "paste a phrase" path has no handle field at all
(the only two `HandleField` sites are `landing.rs:660` and `:892`), so every user
arriving with an existing EVM wallet hits this on their first click.

This is not specific to `HANDLE_REQUIRED`: **any** failed click in the Sign modal
burns the nonce and makes the button permanently useless until the user backs out
and starts over.

Both readings are defensible — the client reads "retry with a handle" as *retry
the login*; the gateway reads a nonce as single-use regardless of outcome, which
is correct anti-oracle design. The contract does not adjudicate, and the mock
shares the gateway's reading (`api/mock.rs:380-383`), so the client's reading was
never tested.

## 33. `opened_at` is always null, so positions never render

**Verified by reading.** `dto.rs:549` declares `opened_at: Option<String>` and
`http/account.rs:212` sets it to **`None` unconditionally** — no code path fills
it. The client declares `pub opened_at: String` (`api/types.rs:780`), required. A
JSON `null` into `String` is a hard serde error, so **every**
`GET /account/positions` response containing a position fails to decode, and
`lib.rs:257-259` swallows it. The positions blotter is permanently blank,
`hazard_position()` (`state.rs:344-356`) never returns anything, and the
high/critical liquidation banner — the loudest element in the design system — can
never fire while a real position is being liquidated. The contract lists
`opened_at` as present, so the gateway is the deviant and the client is merely
brittle for not using `Option`.

**It interacts with §9 and the order of the fixes matters.** On the shipped
stack only genesis-seeded accounts hold positions, and none of those has a
gateway account, so this is currently unreachable through the UI — the blotter
is empty for a different reason. It stays High because it fires the moment §9 is
fixed: whoever makes the faucet fund a margin account will find the positions
blotter still blank, and will have no warning that a second, unrelated bug is
why.

## 34. "The request never reached it", and a new id on every retry

**Verified by reading.** `api/error.rs:206-209` renders a transport failure as
*"Can't reach the gateway — The request never reached it."* That is an assertion
the client cannot make: `http/trading.rs:114-150` reserves the id, subscribes and
submits, so a socket that dies after `submit` returns leaves a live order on the
chain while the browser says it never left.

Then the retry makes it worse. `client_order_id` is built **inside** `submit`,
from the wall clock, on every click:

```rust
// screens/terminal.rs:1260
client_order_id: format!("cli_{:x}", crate::clock::now_millis()),
```

The gateway's dedupe keys exactly on that value (`http/trading.rs:115-140`), so
the retry places a **second, real order**. The contract's idempotency guarantee
is defeated by the client's own id generation, and the error message is precisely
the prompt to retry. The id should be minted when the intent is formed and reused
until the order is acknowledged or abandoned.

## 35. The faucet error path re-arms the button without refreshing

**Verified by reading.** `screens/faucet.rs:85-129`: `FAUCET_COOLDOWN` gets an
in-place banner **and** a re-read of `GET /faucet` (`:119-126`) — that path is
correct. **Anything else** goes to `crate::report` (`:127`), which produces a
toast and nothing else: **no `refresh_account`, no re-read of `GET /faucet`, no
re-read of the ledger.** Those three calls exist only on the success branch
(`:105-117`).

Now put §11's failure into that screen. `available()` (`:63-69`) reads the
un-refreshed status and still says available; `claiming` went false at `:87`;
the button's `disabled` (`:179-180`) re-arms. Money arrived, the screen says it
failed, the balance is stale, and every affordance points at clicking again. The
gateway ordered its writes defensively and the client undoes that care by not
reconciling. The fix is one line's worth: reconcile on *every* outcome.

## 36. An irreversible admin settle reports as a decode failure

**Verified by reading.** `api/http.rs:81-86` treats an empty body as `null` →
`()`. `admin_settle` is declared `Fut<'_, ()>` (`api/mod.rs:293-298`) while the
gateway returns `Json<TxResponse>` (`admin.rs:733`). The contract gives an
explicit `→ 204` for resume and unsuspend and is **silent for settle** — the
ambiguity that let the two diverge. So the operator performs the one action the
contract marks 🔴 irreversible, the gateway does it, and the client shows
*"Unreadable answer — It answered, but not in a shape this build understands"*.
The natural response to that message is to try again. (The other four client-side
`()` admin methods all match `no_content()` correctly.)

## 37. The activity feed cannot load, for two independent reasons

**Verified by reading.** `api/http.rs:320-323` builds `?limit=100&cursor=` when
the cursor is `None`. The gateway parses that as `Some("")` and
`decode_cursor` (`http/user.rs:127-129`) turns `hex::decode("")` into an empty
vec whose `try_into::<[u8;8]>()` fails → `400 VALIDATION_FAILED`, swallowed at
`account.rs:58`. The feed is empty on first load, always.

Past that, `dto.rs:206` is `height: Option<u64>` fed from entries whose
production writers both set `height: None` (`faucet.rs:325`,
`http/trading.rs:590`). The client declares `pub height: u64` with
`#[serde(default)]` (`api/types.rs:1376-1377`), and a JSON `null` fails into
`u64` **even with the default**. One faucet claim in the feed kills the whole
page.

---

## 38. Medium findings

* **Fills are attributed to an arbitrary order of the right side.**
  `record_position_fill` (`watcher.rs:519-524`) picks the first live order on the
  matching side in `DashMap` iteration order, which is not deterministic. With
  two live buy orders, one reports a fill it did not get and the other stays open.
* **Fee, role and realised PnL on every fill are fabricated.** The same function
  (`watcher.rs:526-551`) computes `fee` from the taker rate, sets `role` to the
  literal `"taker"`, and `realized_pnl` to `0`. The contract presents all three as
  facts; a maker fill reports a taker fee it never paid. Those fabricated fees are
  summed into `fees_24h` on the operator dashboard.
* **Cancel and cancel-all report success without confirmation.**
  `cancel_order` (`http/trading.rs:247-251`) returns `{"status":"cancelled"}` the
  moment the transaction is submitted — in my live run, for an order that had
  already filled, while the chain rejected the cancel and the block died.
  `cancel_all` (`:296-299`) counts every live spot order in the tracker as
  cancelled by one `BatchCancel`, including `pending` ones the chain never saw;
  and it returns `Err` on the first derivative failure (`:309-318`) after earlier
  cancels were already submitted — a partial success reported as a total failure,
  the same shape as the faucet bug this project already found once.
* **An unauthenticated request can lock any address out of logging in.**
  `POST /auth/challenge` needs no authentication and `ChallengeStore` holds one
  challenge per address (`auth/challenge.rs:66`), consumed before the signature
  is verified. Anyone who knows a victim's address can poll for challenges and the
  victim's signature always arrives against a superseded nonce. The rate limiter
  is keyed on session and client address, so it costs one IP per victim.
* **The sequencer's rejection vocabulary is three codes wide.** `submit_tx` can
  return only `ok`, `InvalidAmount` or `BadSignature` (`cb-rpc/src/server.rs:107,
  115, 140`); every other `SubmitRejection` variant mapped in
  `capnp_session.rs:242-250` and `signer.rs:151-188` is unreachable. So
  `POST_ONLY_WOULD_CROSS` reaches the frontend only when the gateway's own
  pre-check catches it, and `annotate_treasury_error` (`faucet.rs:360-369`) — the
  message telling an operator their treasury key is wrong — can never fire. A
  misconfigured treasury key instead produces `200 OK`, a recorded claim, a
  started cooldown, no money, and a dead block.
* **A derivative order with no oracle returns 500, not `ORACLE_STALE`.**
  `place_order` reads the mark with `state.node.mark_price(id).await?`
  (`http/trading.rs:80`), and `close_position` the same at `:369`. Live, on a
  freshly booted stack: `500 {"code":"INTERNAL","message":"… no mark price for
  perp:0"}`. `http/market.rs:77` gets it right with `.ok()`. Every derivative
  order returns a 500 until someone starts an oracle feeder — the first thing a
  user hits on `make up`.
* **Subscribe and snapshot are not atomic.** `apply_op` inserts the channel into
  the subscription set (`ws.rs:512` → `:286`) — making the connection a publish
  target — **before** the snapshot exists (`ws.rs:609-612` →
  `http/misc.rs:141-146` → `watcher.rs:674, 716`), with nothing held in between on
  a multi-threaded runtime. The damaging interleaving queues
  `[delta(H+1), snapshot(H)]`; the client drops the delta as
  `DroppedAwaitingSnapshot` (`cb-web/src/ws/book.rs:132-134`), installs H, then
  applies delta(H+2) to a book at H. **Silently wrong book.** `apply_delta`
  returns `Gap` and it is deliberately discarded (`state.rs:538-543`). This is
  exactly the race the comments at `http/misc.rs:136-138` and
  `watcher.rs:651-653` claim cannot happen.
* **Switching markets shows the previous market's book as live.**
  `expect_snapshot()` is called from one place, `FeedEvent::Connected`
  (`state.rs:501`). Selecting ETH after BTC leaves the BTC book in place with
  `awaiting_snapshot == false`, so `is_ready()` is true and the BTC ladder renders
  under the ETH header. In that window an ETH delta passes the
  `*symbol == selected` guard (`state.rs:536`) and — because heights are
  chain-wide, so the `Stale` guard (`book.rs:135`) does not catch it — ETH levels
  are patched into the BTC map.
* **Settlement with neither an oracle nor a TWAP wedges that height.**
  `apply_futures_settle` refuses a zero price (`executor.rs:2068-2070`), commenting
  that it is "recoverable by the next block". It is not: `build_and_apply` applies
  with `?` (`block.rs:121-123`), the head does not advance (`block.rs:96`), and
  `enforce_required_futures_settle` keys on `expiry_height == height` exactly
  (`block.rs:247`). No block means no `OracleUpdate` can land. Reachability is
  narrow — it needs a market that never had an oracle price and never had a mark
  sample — so I rate it medium. Executor and block-builder behaviour **verified**;
  how a permanently failing build surfaces at the consensus layer **suspected**.
  There is no test: `futures_settle.rs` covers missing-tx, non-proposer,
  already-settled, before-expiry and happy paths, and nothing covers a zero price.
* **Unbounded mempool flood, amplified and rewarded.** `accept_tx`
  (`cb-net/src/p2p/chain.rs:158-174`) checks shape and signature, then does a raw
  `Mempool::put` (`cb-state/src/mempool_cf.rs:15-17`) — no nonce check, no balance
  check, no size cap, no eviction, no per-sender limit. A zero-balance keypair
  suffices. Each accepted transaction additionally wakes the block builder, earns
  the flooder **+1 `GoodTx`** (`engine.rs:1041`), and is **re-announced by us to
  every other peer** (`engine.rs:1049-1060`). We amplify the flood mesh-wide and
  pay the flooder for it.
* **Disconnect never closes the read half.** `Action::Disconnect`
  (`transport.rs:308-313`) drops the writer and emits `Disconnected`; nothing
  touches `read_half`, and the reader loop exits only on error or EOF
  (`transport.rs:449-480`). A "banned" peer keeps a live decode loop pushing
  fully-decoded 64 MiB messages onto an **unbounded** `Cmd` channel
  (`transport.rs:145`), which the engine discards at `engine.rs:553-555` —
  **before** `rate_limited` runs at `:556`, so there is neither a rate limit nor
  scoring on that path. Composed with the `ServeBlockRange` stall (§29), two
  mediums make a memory exhaustion.
* **"The peer is scored for it, not just dropped" is false.**
  `transport.rs:457-462` injects a synthetic `Disconnect(ProtocolViolation)` for
  an out-of-range frame length with exactly that comment. `engine.rs:582-586`
  handles it with `tracing::info!` → `note_rejection` → `on_disconnected`, and
  **`penalise` is never called**. Worse, `note_rejection`
  (`engine.rs:1168-1199`) puts *the sender's* address into *our* `dial_backoff`
  for 30 s and calls `set_backoff`, which writes `rec.banned_until_ms`
  (`peer_table.rs:279-283`) — **sending us a malformed frame makes us stop
  dialling that peer.** Since there is no transport authentication
  (`transport.rs:25-31`), an on-path attacker can inject one bad length prefix
  into any honest connection and make both ends back off from each other for
  30–300 s: a cheap partition primitive against an honest mesh.
* **Free identity and no IP dimension anywhere.** Bans are keyed on `NodeId`
  (`peer_table.rs:115-119`), and a `NodeId` is a fresh keypair — so ban evasion is
  free. `is_addr_banned` is effectively dead for inbound peers, because
  `on_connected` checks the ephemeral accept address (`engine.rs:483-491`,
  `transport.rs:182`) while the record stores the advertised `hello.listen`
  (`engine.rs:716-722`). There is no per-IP connection cap, so one host with N
  identities fills every `max_peers` slot. `on_peers` (`engine.rs:758-789`) has
  **no "did we ask?" check**, `note_address` stores arbitrary pairs unverified
  (`peer_table.rs:131-157`), and `share` re-gossips them (`peer_table.rs:306-319`)
  — so one peer poisons the whole mesh's tables and **earns +1 for every batch**.
  Inject `victim_ip:80` and every node dials it. And eclipse is sticky: seeds are
  dialled once, at `bootstrap` (`engine.rs:400-417`), after which `dial_to_target`
  uses only `table.dial_candidates` (`engine.rs:1253`). **This is a declared
  limit** — `config.rs:5-8`, `transport.rs:31` and `p2p.toml:7-10` all state the
  intended three-node loopback deployment honestly — so I record it as known
  rather than hidden. It is the item that must close before any public exposure.
* **Peer bans do not survive a restart.** `PeerTable`, `dialing` and
  `dial_backoff` all live in `P2pEngine` (`engine.rs:299-330`); nothing is written
  to disk.
* **Three admin query and shape problems.** `api/http.rs:335-339` emits
  `is_bot=` (empty) when the filter is `None`, which is the default
  (`admin.rs:109`); `serde_urlencoded` rejects that for `Option<bool>`, so axum
  400s before the handler and **the admin user list is permanently empty**.
  `api/http.rs:438-443` always emits `action=`, and the handler retains on
  `r.action == ""` (`admin.rs:1214-1216`), so **the audit trail is always empty**.
  And audit `detail` is `BTreeMap<String, String>` on the client
  (`api/types.rs:1686`) against `serde_json::Value` on the gateway
  (`dto.rs:1297`), which actually writes booleans, numbers and arrays
  (`admin.rs:799, 902-907, 1131-1135, 1280-1284`) — any one fails the whole list.
  On screen all three are indistinguishable from a 403, because every admin
  loader uses `if let Ok`.
* **`admin_grant` expects twice the fields it gets.** The client returns
  `FaucetClaim`, needing four fields (`api/types.rs:129-139`); the gateway returns
  `{claim_id, tx_hash}` (`dto.rs:1277-1283`), which is what the contract
  specifies. Here the **client** is wrong, and a successful grant reports as a
  decode failure.
* **Expiry while idle is never detected.** Every mutating call routes through
  `report()` (`lib.rs:162-174`), which signs out on 401 — that design is right and
  consistently applied. Every **read** swallows the error instead (`lib.rs:251-259`,
  `portfolio.rs:44`, `faucet.rs:34`, `account.rs:52-75`, the six admin loaders).
  A session that expires while the user reads the portfolio produces no handling
  at all: the header still shows them signed in and the numbers sit at their last
  values with nothing saying they are from a dead session.
* **Suspensions, renames and session metadata do not survive a restart.**
  `main.rs:77-78` builds the `Directory` over `InMemoryKv`, honestly commented.
  **A suspension is lifted by a gateway restart**, with no audit record of the
  unsuspension; a handle change reverts; the session list a user relies on as a
  security feature is empty even though the sessions themselves still work.
  Combined with §2, a rename *into* `admin` is lost on restart while a
  *registration* as `admin` is permanent.
* **The operator dashboard reports hardcoded zeroes as measurements.**
  `mempool_depth: 0`, `validators: 0`, `insurance_fund: "0.00"` and per-market
  `open_interest: "0.00"` (`http/admin.rs`), each with a comment explaining the
  node exposes no query. The contract presents them as readings. After §6 drains
  the insurance fund, a real zero and this permanent zero are indistinguishable on
  the one screen built to notice. Same class as the "296,000 orders per second"
  the plan already documents. Confirmed live.
* **Contract deviations.** `GET /account` (§7). `POST /auth/login` returns the
  stored row handle while `GET /auth/session` returns the effective one, so after
  a rename the two disagree. `candles:{symbol}:{interval}` is server-only — the
  server publishes (`watcher.rs:265-283`) and the client has a helper nothing
  calls and no match arm (`ws/msg.rs:198-222, 246-248`). Three undocumented server
  frames (`subscribed`/`unsubscribed`, `system/authenticated`, `system/closing`)
  are in neither the contract nor the client, so the client has no positive
  acknowledgement that auth succeeded.
* **`RiskApplied` drops `affected`, and `?dry_run` is never sent.** The contract
  devotes a paragraph to arguing that an operator needs the names, not the count
  ("a count alone does not tell them whom to warn"), and to being able to preview
  a change. The gateway implements both. The client declares only `applied` and
  `accounts_now_below_maintenance` (`api/types.rs:1644-1649`) and has no way to
  ask for a dry run (`api/http.rs:396-404`).
* **A faucet cooldown test never runs.** `cb-gateway/src/faucet.rs:512`,
  `async fn a_second_claim_inside_the_cooldown_is_refused`, has no
  `#[tokio::test]`. It is the only orphan across `cb-gateway`, `cb-block`,
  `cb-engine`, `cb-state`, `cb-net` and `cb-web`. It compiles, never runs, and is
  the test whose name claims to cover the faucet's central rule. The property is
  incidentally covered by
  `two_simultaneous_claims_from_one_user_produce_exactly_one_grant`, so the rule
  is not untested — but it is untested by the test that says it tests it.
* **PLAN §9 item 4 is stale, in the project's favour.** "Land peer-to-peer sync,
  and make gossip verify by default" describes work already done. `P2pConfig`
  (`config.rs:104-152`) has no verify/trust flag at all, `DbChain::verify_and_apply`
  (`chain.rs:143-149`) unconditionally re-executes and compares the root, and the
  engine structurally cannot adopt a block — it has no database, executor or
  decoder (`engine.rs:19-23`). `main.rs:443-445` even warns that `--strict-sync`
  and `--async-verify` have no effect in peer-to-peer mode. The *legacy*
  `--sequencer` replica still defaults to unverified fast-mirror
  (`main.rs:115`, `replica.rs:11-18`), and the shipped non-p2p cluster works
  around that by passing `--strict-sync` explicitly. Worth correcting the plan.
* **`docs/architecture.md` has no peer-to-peer section at all.** §2.6 describes
  only the star, strict-sync and fast-mirror. A reader would conclude the default
  replica is an unverified block-log backup and never learn the gossip path
  exists.

---

## 39. Low findings

* **`client_order_id` idempotency is in-memory only** (`tracker.rs:244`). The
  contract promises the same id returns the original order for 24 hours; a restart
  empties the map.
* **A second first-login for one address reports the wrong reason.**
  `users.address` has a unique index (`migrations/0001_initial.sql:39`), so the
  two-device race is safely refused — good. But `establish` maps every
  `PortError::Conflict` to `HANDLE_TAKEN` (`auth/mod.rs:357-362`), so the loser is
  told a handle is taken when the cause was the address.
* **Zero-quantity perp position rows are never collected.** The exact-close case
  returns a `qty: 0` row (`perps_position.rs:205-221`) and the only
  `Positions::delete` call sites in the executor are inside `apply_futures_settle`.
  Two comments (`cb-types/src/derivatives.rs:126-128`, `perps_position.rs:206-208`)
  describe an "end-of-block GC" that does not exist. Benign today — every reader
  skips `qty == 0` — but it is unbounded state growth and a false comment. The
  gateway already filters them out by hand (`http/account.rs:150-153`).
* **The watcher's resync comment describes book invalidation the code does not
  do.** `watcher.rs:75-77` says "every cached book is suspect. Dropping them forces
  a fresh snapshot"; the code calls only `accounts.clear()` (`:79`, `:88`).
  `state.marketdata` is never cleared, so `send_book_snapshot` may serve a cached
  book that predates a node gap (`watcher.rs:674-693`) — and `accounts.clear()` on
  those paths is precisely what triggers §15.
* **A cleared account mirror swallows the next real change** (`watcher.rs:440-444`).
* **`equity` counts spot USD, `available_margin` does not, and neither counts spot
  BTC.** `http/account.rs:80-88`. A fully funded faucet account reports $74,975 of
  equity and $0.00 of available margin in the same object, and `margin_ratio` and
  `liquidation_risk` are computed against that partial equity.
* **No mempool bound.** `Mempool::put` has no cap; `MempoolFull` is never
  produced.
* **`is_new` and `immediate_fills` are ignored by the client.** The contract says
  `is_new` drives the welcome flow; a grep finds no reader. A market or IOC order
  that filled completely on submission is announced with *"It appears in the book
  at the next block"* — a book it will never appear in. `response.status` is
  likewise parsed and never read, so the toast says "Order placed" for every
  outcome including `rejected`.
* **Client-invented fields.** `Order.tx_hash`, `Order.reject_reason`,
  `LedgerEntry.height`, `LeaderboardEntry.bot_strategy`, `AdminMarket`'s
  `oracle_age_seconds`, `resting_orders`, `max_leverage` and the two margin bps
  are all declared, defaulted and never sent. The `AdminMarket` pair is the
  notable one: the gateway *does* send oracle freshness and resting depth, under
  different names (`oracle_age_blocks`, `oracle_fresh`, `resting_levels` —
  `dto.rs:1157-1164`), so the operator's market table shows "0s" and "0 resting"
  for a market whose oracle is stale, and the risk editor opens with blanks.
* **A silent half-signed-in state.** `landing.rs:418` does
  `if let Ok(info) = backend.api.session().await`; if that fails after a
  successful login, the token is stored and installed but `stores.session` stays
  `None`, and the user lands on a terminal whose button reads "Sign in to place an
  order".
* **Latent null-into-`String` risks**, each an instance of §33's pattern with no
  producing path I could find: `LedgerEntry.memo` (a memo-less transfer from a bot
  or `cb-client`), `LedgerEntry.tx_hash`, `UserSession.user_agent`/`ip_prefix`,
  and `Block.proposer` — the last would blank the chain rail and explorer.
  **Suspected**, not verified.
* **`Block::decode` is unbounded bincode.** `cb-block/src/block.rs:40-41` uses
  `bincode::deserialize` with no `with_limit`. Serde's `size_hint::cautious`
  caps sequence pre-allocation, so I do **not** assert an allocation bomb and
  mark this **suspected**. Flagged because it is the one place attacker bytes
  reach a deserializer with no explicit bound.
* **Performance, not correctness.** `GET /admin/overview` and
  `PATCH /admin/markets/{sym}/risk` loop over every user and issue one node RPC
  per position plus one per account. At the contract's example scale of 1,284
  users that is thousands of round trips per page load.

---

## 40. Flows judged sound

Coverage is only meaningful if I say what held up. These are parts I tried to
break and could not.

**Sign-in's cryptography is the strongest code in the project.** Keccak rather
than SHA-3 (`cb-web/src/wallet.rs:54-58`); the EIP-191 prefix uses byte length,
with a test for a non-ASCII message (`:368-373`); EIP-55 is checked against four
published vectors (`:335-348`); mixed-case is rejected and flat-case accepted
exactly as the contract requires (`:99-124`, tested `:351-365`); `v` is
normalised for both 27/28 and 0/1 (`:248-251`); `Debug` is hand-written so no log
line can print a phrase (`:153-159`); errors refuse to say which word is wrong.
Derivation is pinned to the ecosystem vector `0x9858EfFD…Eda94`.

**The mnemonic never leaves the browser, and the client has no dev-login path.**
Verified by grep rather than by comment: there is no `dev_login` on the `Api`
trait, no `/auth/dev-login` string in `api/http.rs`, and the only repo-wide hit
under `cb-web/src` is the comment at `wallet.rs:13-14` explaining its absence.
`LoginRequest` carries `address`, `signature`, `handle` only. This is the single
most important rule in contract §2 and the client honours it strictly.

**The challenge lifecycle is correct.** Single-use whether or not the signature
verifies (`auth/challenge.rs:75-78`), replacing rather than accumulating,
expiring at the contract's five minutes, keyed per address, pruned. A captured
signature cannot be replayed: the test at `auth/mod.rs:828` demonstrates it and
the consume-before-verify ordering makes it structural. Session tokens are stored
hashed. `dev_login` genuinely goes through the same `establish` path, so the two
cannot drift. The defects here are §2 (the role decision), §32 (the retry) and
§21 (rotation) — none in the scheme itself.

**Two sessions racing is handled, in both senses of the question.** Two *first
logins* for one address cannot both create an account — `users.address` carries
a unique index and the second is refused (mis-attributed, §39, but refused).
Two *devices holding sessions* do not interfere: sessions are independent rows,
`authenticate` resolves each on its own, and one user's token can never resolve
to another (`auth/mod.rs:991-1007`). What they do contend for is the single
outstanding challenge per address: two devices signing in at the same moment
will see one of them lose its nonce to the other's `POST /auth/challenge`, which
is §32's mechanism arriving by accident rather than by malice, and the same
one-challenge-per-address fix covers both.

**Session expiry mid-order is correct over REST.** `Caller` is an axum extractor,
so a 401 is decided before `place_order` runs: the order genuinely was not
placed, and the client's uniform handler says "Signed out" truthfully. Nothing
re-checks mid-flight, which is right — an order already signed and submitted must
not be half-abandoned because a clock ticked.

**Spot lock and unlock telescope exactly, and this is genuinely well done.**
`spot_lock_required` (`executor.rs:2228-2233`) is a single definition of what an
order holds, evaluated as a function of its *remaining* quantity. Every movement
is the difference between two evaluations of that one function: placement takes
`required(qty)`, each fill releases `required(before) − required(after)` at the
maker's own resting price (`executor.rs:2440-2448`), the taker's surplus is
reconciled in one step against what its resting remainder needs
(`executor.rs:2544-2551`), and cancel releases `required(qty_remaining)`
(`executor.rs:2582-2588`), with `BatchCancel` using the same formula
(`:2635-2641`). The sum is exactly the placement lock however many pieces the
order fills in — precisely what F6 and F7 were the absence of. The fee reserve
rounds up at placement and the fee rounds down at fill, so the reconciliation can
never over-release. **A partial fill followed by a cancel returns exactly the
right amount.** I checked by substitution rather than by trusting the comment,
and it holds. The one thing it does not survive is §1, where the lock is rolled
back and the order is not — a failure of the surrounding transaction boundary,
not of this arithmetic.

**Dated-future settlement is the best-engineered flow in the system.** The
requirement is enforced at block level (`block.rs:235-269`) so the action lives in
the transaction root and replicas re-execute rather than trusting a leader hook,
and the handler revalidates every precondition independently
(`executor.rs:2036-2052`). It is idempotent by two composing guards, and because
it deletes every position row a second run would find nothing to pay. It removes
zero-quantity sentinels (`executor.rs:2085-2089`), registers each deletion with
the positions trie, cancels every resting order (`executor.rs:2107-2115`) and
clears the mark-history buffer. Open interest afterwards is provably zero. There
is no admin force-settle transaction at all, so the double-settle scenario is
unreachable by construction — which makes §18 a gateway defect rather than a
protocol one.

**The liquidation engine's money handling is correct.** Triggering is
permissionless and eligibility is re-checked at apply time
(`executor.rs:1513-1515`), so a spurious trigger costs the sender a failed
transaction and nothing else. The fee split rounds insurance up and the keeper
down (`liquidation.rs:199-211`), and the debit is capped at the owner's
collateral (`executor.rs:1715`). Bad debt is netted against collateral rather
than parked beside it, so a later credit repays it — a real and complete fix of
F4 — and absorption happens at the write site, covering all three writers by
construction (`executor.rs:1100-1123`). The insurance fund cannot go negative:
absorption is capped at its balance, rebates at what is available, and it cannot
absorb its own debt. The dated-future resolution fix (`load_market_view`,
`executor.rs:1495`) is real and has a positive control in its regression test.
F5 is genuinely fixed: the owner's resting orders are cancelled before any
tranche, and a failed tranche now `continue`s rather than being banked as zero.

**Rounding directions are deliberate and defensible.** Margin requirements round
up per band (`margin.rs:293`); fees round down; realised PnL and funding truncate
toward zero, which is sign-symmetric and therefore non-directional. I traced every
division on the value path and found no systematic leak and no systematic
creation. One caveat: invariant 7 ("funding transfers net to zero per tick") is
exact only to ±(number of position rows) micro-USD, because the truncation is per
row rather than once per side. Well inside the documented dust budget, but the
invariant is stated as exact.

**Order validation at the gateway is a genuine superset.** `orders.rs` checks
tick, lot, minimum notional, post-only crossing, reduce-only bounds, oracle
freshness and initial margin, and every one fails requests the chain would also
fail — with the twist that the chain's own checks are *weaker* (§22), so the
gateway is stricter rather than looser. The market-order protective bound
(`orders.rs:138-154, 262-295`) is the right design: an aggressive limit at bounded
slippage with IOC, rounded outward onto the tick grid, refusing an empty book
rather than resting. `notional_micro_usd` picks the formula from the market kind
rather than guessing. The reduce-only check (`orders.rs:399-423`) enforces the
quantity bound F1 was missing. The client's pre-validation (`ticket.rs:174-226`)
mirrors it and every hint names the number, honouring the design system's rule
that a disabled button never hides its reason.

**The admin role gate is correctly placed.** The `Admin` extractor
(`http/mod.rs:400-419`) reads the role from the server's own record and is applied
through the type system per handler, so it cannot be silently omitted. Every
mutating handler audits. The `403` message is identical to any other forbidden
request. The client gates the admin UI on `roles` for display only, says so in
comments at both sites, and fails closed on a missing field. The defect is which
fact the role is computed from (§2), not where it is checked.

**Suspension is enforced at the right layer.** In the `Caller` extractor
(`http/mod.rs:352-357`), so it covers every authenticated endpoint including
`POST /transfers` and `DELETE /orders` — not per handler, where one omission would
be a hole — and again at login. Existing positions are deliberately left open, as
the contract specifies.

**Peer-to-peer gossip verifies by default, with no bypass, and this is the
strongest part of the network design.** `chain.rs:143-149` is the single door,
`apply_remote_block` re-executes and compares the root, and the engine has no
decoder to shortcut it (`engine.rs:19-23`). Forwarding happens only after the
node's *own* verification (`engine.rs:956-982`), which makes "forward at most
once" structural and stops a lie at the first honest hop, with backlog
suppression keyed on our own pending buffer rather than on peer-claimed heights.
PLAN's core invariant — "faster propagation must never become a reason to trust a
peer" — is enforced by structure rather than by comment, and tested
(`tests/p2p_chain_sync.rs:186`).

**The p2p handshake and range bounds are correct.** Protocol version, chain id
and genesis hash are all compared (`engine.rs:638-666`), with rejection carrying a
specific reason. Catch-up is bounded on all four edges — request
(`engine.rs:1294-1300`), serve (`engine.rs:607-611`), receive count
(`engine.rs:873-876`) and receive height range (`engine.rs:877-881`) — with
saturating arithmetic throughout, so `u64::MAX` produces a bounded request rather
than a panic. A peer that lies about its head has the claim replaced by what it
demonstrated and is benched (`peer_table.rs:226-231`, tested). `MAX_FRAME_BYTES`
is enforced on the read path **before allocation** (`transport.rs:455` → `:465`),
and re-checked in both `encode` and `decode`. Duplicate-connection tie-break is a
deterministic function of the two node ids (`engine.rs:685-709`), so simultaneous
dials converge on one connection rather than zero. Dedup is FIFO rather than LRU
so an adversary cannot pin an old hash resident (`dedup.rs:8-12`). `is_benign` is
actually implemented rather than merely documented (`peer_table.rs:256-258`,
tested with 10,000 duplicates). Config uses `deny_unknown_fields`, so a typo is a
startup error. Verification runs off the event loop via `spawn_blocking` and is
serialised to one block at a time — the contrast with `ServeBlockRange` (§29) is
what makes that one look like an oversight rather than a policy.

**WebSocket transport-level behaviour is correct, and better than the contract
requires.** `/ws` is mounted at the root, sibling to `/health`, outside the
`/api/v1` nest (`http/mod.rs:139-142`), and `websocket_url()` strips the API path
and rebuilds from the authority (`cb-web/src/config.rs:86-109`) — no `/api/v1/ws`
string exists anywhere, so the past defect is genuinely gone. Fan-out is a bounded
per-connection `tokio::mpsc` (`ws.rs:210, 245`), not a broadcast, so there is no
silent `Lagged`; a full queue removes the connection (`ws.rs:422-434`), which
closes the sink and forces reconnect → `expect_snapshot()` → fresh snapshot. Lag
becomes a resync, which is the right answer, and a counter is kept. Delta
semantics agree exactly: qty 0 removes a level, a delta is a full-level replace
rather than an increment, and `format_scaled` treats `min_places` as a minimum so
no two raw prices can collide into one string. Subscribe-before-auth is refused
*and the channel is not stored* (`ws.rs:283-286`), and `publish_to_user` filters
on both the authenticated user and an `Account` subscription (`ws.rs:345-361`), so
one user's private data cannot reach another's socket. Ping/pong and the 90 s idle
close are both implemented, with the client pinging at 30 s. Book resynchronisation
ordering is subtle and right: `Connected` marks the book as awaiting a snapshot
*before* any frame from the new connection can arrive, so a delta racing the
snapshot is dropped (`state.rs:498-509`, `book.rs:132-134`).

**What the client sees between submission and the block landing is correct, and
deliberately so.** `place_order` reserves the `client_order_id` before signing,
subscribes to the node's event stream *before* submitting so it cannot miss the
block it is waiting for, then waits a bounded `order_confirm_ms`
(`http/trading.rs:114-190`, `await_inclusion` at `:607-634`). If the block
arrives in time the reply carries the real on-chain order id; if it does not,
the reply is `201` with `status: "pending"` and the transaction hash, which
`DELETE /orders/{id}` also accepts. So the honest answer to "what does the
client see in the gap" is: a pending order addressed by its transaction hash,
for at most one confirm window, resolving to `open` on the next block — normally
one 100 ms interval. The three-step ordering is reasoned out in the module
header and each step is load-bearing. The failure modes around it are §3 (a
pending order that will never resolve, because the chain never got it), §10 (an
order that resolved and was never updated) and the client's own habit of
reporting "Order placed" without reading `status` (§39) — none of them a defect
in this design.

**The client has no optimistic order or fill.** `terminal.rs:1264-1285` awaits the
response and then re-reads `/account`, `/account/orders` and `/account/positions`
from the server. The blotter renders only server truth, so there is no optimistic
row to reconcile or leave stranded. For a trading UI that is the right call.

**`ApiError` is well designed.** A decode failure becomes `ApiError::Decode`
rather than a default value (`api/http.rs:8-12, 86`), and the reasoning — "a zero
price that came from a parse failure is indistinguishable from a real zero price"
— is correct and is why several findings above are silent-blank rather than
wrong-number bugs. Unknown error codes keep the gateway's message. The uniform
401 rule for every mutating call (`lib.rs:162-174`), including on the socket, is
applied consistently across all fourteen call sites.

**Decimal discipline holds.** `cb-web` enables `rust_decimal`'s `serde-str`, so
every money field crosses the wire as a string in both directions and scale
survives. I checked every `f64` in the crate: the only conversions are pixel
geometry (`ui/canvas.rs:168-170`, `ui/chart.rs:148-166`), documented at the call
site. The two numbers a user reads that derive from `f64` — chart gridline labels
and the crosshair price readout (`ui/chart.rs:183, 322-323`) — are synthetic by
construction and never sent back; the ticket reads typed strings through
`parse_typed` and clicking the book goes `Decimal`→`Decimal`. **No precision is
lost before display of any server-supplied number, and none before sending.**

**Thirteen enum vocabularies match the gateway literal for literal** — order
status including `partially_filled` and British `cancelled`, side, position side,
margin mode, tif, liquidation risk, market kind/class/status, fill role, ledger
kind, health status, account status, scenario. Given how much else diverged, this
part of the transcription held.

**Faucet concurrency within one process is correct.** The per-user lock spans the
whole claim, and the treasury nonce is serialised per signing address by
`TxSubmitter` — exactly what one shared key needs. My live run produced strictly
sequential treasury nonces across concurrent claims.

**Order ids are derived identically on both sides of the boundary.** The gateway
mirrors the executor's private `derive_order_id` byte for byte
(`tracker.rs:191-198` against `executor.rs:1427-1431`), and
`the_order_id_layout_matches_the_engines` pins the layout rather than trusting the
comment — the right way to handle a copy that cannot be shared.

---

## 41. What I could not verify

* Whether the uncommitted column families in §23 can be made to diverge in
  practice, as opposed to being structurally able to. I read the roots and the
  commit path; I did not construct a divergence.
* How a permanently failing block build (§38, settlement wedge) surfaces at the
  consensus and syncer layer. I did not read that path far enough to say.
* Whether §31's absent mutex is reachable without §4 firing first.
* Whether `Block::decode`'s unbounded `bincode::deserialize` (§39) is
  exploitable. Serde's cautious size hint argues against it and I did not test.
* Several latent null-into-`String` risks in §39 have no producing path I could
  find; they are typed hazards, not demonstrated bugs.
* I did not run `cargo test`, `cb-simgame`, or any benchmark. Every live
  reproduction above used the release binaries already present in
  `rust/target/release` and the public REST API.

---

## 42. Overlap with other reviews in flight

`docs/security-review.md` and `docs/ui-review/` were written into this repository
by other agents while this review was running. I did not read them, so I cannot
say where they agree or differ — but the caller should expect overlap, and should
reconcile rather than assume three independent confirmations:

* §7, §8, §16, §32–§37 and most of §38's client-side bullets are frontend
  contract findings and are the likeliest to be duplicated by `ui-review/`.
* §2, §14, §21 and §38's challenge-griefing and rate-limit bullets are the
  likeliest to be duplicated by `security-review.md`.

Where two documents describe the same defect, prefer whichever names a
`file:line` and a reproduction. Where they disagree about severity, note that
this review weights "creates or destroys money, or stops the chain" above
everything else, which may not be the other documents' ordering.
