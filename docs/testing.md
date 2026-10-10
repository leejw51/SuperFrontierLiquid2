# End-to-end testing

`rust/crates/cb-e2e` starts the real `cb-node` and `cb-gateway` binaries and
drives them over HTTP and WebSocket exactly as a browser would.

```
make e2e          # build the binaries, then run the whole suite
make e2e-fast     # only the checks that need no running stack (~instant)
make e2e-clean    # clear strays left by a run that was killed
```

---

## Why this exists

Every other suite in this repository tests one subsystem against a **mock of
its neighbour**. `cb-node` is tested alone. `cb-gateway`'s integration tests
drive the real router over a scripted sequencer. `cb-web` is tested against
fixtures. All of them are green, all of them are worth having, and together they
shipped five faults that a single manual run found in ten minutes:

| | Fault | Why nothing caught it |
|---|---|---|
| 1 | The genesis treasury was a vanity address with no private key, so the faucet could never mint | Genesis applied, the node ran, the gateway started. The only symptom was clicking a button and getting nothing. |
| 2 | A multi-asset faucet grant credited the chain and *then* returned an error, because two assets in one grant collided on a primary key | The user got an error and the money. |
| 3 | A derivative market no oracle had priced made the ticker return 500 | The terminal fetches a ticker per market, so one unpriced perpetual blanked the whole screen. |
| 4 | The websocket never connected: the gateway served `/ws` at the root, the frontend asked for `/api/v1/ws` | Both sides were individually correct. A browser reports it only as a failed handshake. |
| 5 | Genesis defaulted to a file without the advertised markets, and referenced a variable the environment never set | Neither is code. No runtime test would ever look. |

Not one of those is visible from inside a subsystem. They live in the seams:
between shipped configuration and shipped code, between the gateway and a chain
that has never seen an oracle price, between the path one side serves and the
path the other asks for. The only test that sees a seam is one that starts the
actual binaries and behaves like a client.

---

## Running it

`make e2e` builds `cb-node` and `cb-gateway` in release mode and passes their
paths to the suite explicitly, so it can never silently test a stale build.

```
make e2e                       # everything
make e2e E2E_JOBS=4            # four stacks at once (needs a higher ulimit -n)
make e2e-one T=journey         # one test, by name substring, with output
make e2e-fast                  # the static checks only
```

Or directly:

```
cd rust
cargo build --release -p cb-node -p cb-gateway
cargo test -p cb-e2e -- --include-ignored --test-threads=2
```

| Variable | Meaning |
|---|---|
| `CB_E2E_NODE_BIN`, `CB_E2E_GATEWAY_BIN` | Use these binaries instead of searching `target/{release,debug}` |
| `CB_E2E_PARALLEL` | How many stacks may run at once (default 2) |
| `CB_E2E_KEEP_DATA` | Keep each stack's data directory and logs, and print where |

A missing binary is reported with the exact `cargo build` command to run, not as
a "No such file" from somewhere inside the harness.

`cb-e2e` is a member of the `rust/` workspace, so `cargo test --workspace` picks
up its static checks and skips the `#[ignore]`d ones.

### What a run costs

Each stack is two processes, a RocksDB, a SQLite and three sockets, and takes
about a second to come up. The whole suite is roughly a minute at
`E2E_JOBS=2`. The limiter exists because letting `cargo test` start one stack
per core exhausts file descriptors on a default `ulimit -n` — the same wall
`make testallslow` exists to avoid.

---

## How it is built

**Nothing has to be running first.** `Stack::start()` creates a temporary data
directory, applies the shipped genesis, boots the sequencer and the gateway, and
waits until the gateway is serving a market catalogue it could only have got
from the node.

**Nothing collides.** Every stack picks its own free ports and its own temporary
directory. Two `make e2e` runs, or a run alongside a developer's `make up`,
share a machine without noticing each other.

**Nothing leaks.** `Stack`'s `Drop` kills both children, reaps them and removes
the directory, and `Drop` runs while a panic unwinds — so a failing assertion
cleans up as thoroughly as a passing one. The one case it cannot cover is the
*test binary* being killed, which is what `make e2e-clean` is for.

**Nothing sleeps.** Synchronisation is a predicate with a deadline
(`cb_e2e::wait`), and the failure message names the condition that never held
and the last thing observed. A `sleep` long enough today is a flaky test
tomorrow, and a flaky test gets deleted by whoever hits it — which is worse than
having no test.

**Failures are loud.** A panicking test prints the tail of both process logs
before tearing down. "The gateway returned 500" is not a diagnosis; the
gateway's own log usually is.

**Assertions are on what a client sees.** There is no access to the gateway's
database or the node's state from this crate at all. A test that read a row to
confirm a faucet grant would pass while the endpoint that exposes that grant was
broken, which is the exact failure mode the suite exists to end.

---

## What it covers

| File | What it guarantees |
|---|---|
| `tests/journey.rs` | The whole product promise in one test: sign in with a mnemonic, claim the faucet, see the money arrive **on chain**, place a limit order, see it rest on the public book, cancel it, see the funds unlock, then cross with another account and see a fill. |
| `tests/regressions.rs` | One test per fault in the table above. Three need no stack. |
| `tests/contract.rs` | The endpoints of `docs/api-contract.md`: the documented success shape, each documented error code for its condition, money as a decimal string everywhere, idempotent `client_order_id`, `cancel-all`, transfers landing in both ledgers, per-session revocation and the rate limit. |
| `tests/auth.rs` | The real challenge-and-signature login, plus every way of failing: no token, a malformed token, a token never issued, another user's token, a signature recovering to a different address, a replayed challenge, a first login with no handle, a handle already taken, logout and revocation. |
| `tests/websocket.rs` | Connect, subscribe, snapshot before any delta, a delta applying, a zero quantity removing a level, ping/pong, and a private channel that needs auth and never carries another account's fills. |
| `tests/admin.rs` | An operator reaches the admin surface and an ordinary account gets 403; the operator role cannot be claimed by registering a handle; the gateway refuses a weak server secret, and for the right reason; halting refuses new orders but still allows cancels; a risk change names the accounts it affects and a dry run changes nothing; suspension, operator grants, faucet settings, the user record and the audit trail. |
| `tests/resilience.rs` | The node is SIGKILLed mid-run: the gateway answers `503 NODE_UNAVAILABLE` rather than hanging, and recovers by itself when the sequencer restarts, on the same chain. |

### Money is a decimal string

`src/money.rs` walks every response and asserts the contract's number
conventions: money as a decimal string, never a JSON number, and `height`,
`tx_count` and the `*_bps` risk parameters as JSON numbers. This is the check
that catches a serialiser change nothing else would — emitting `64250.5`
instead of `"64250.50"` breaks no Rust client, breaks no test that parses the
value back, and silently rounds a balance in every browser.

### The websocket path is derived, not typed in

`cb_e2e::ws::frontend_websocket_url` is a port of `cb-web`'s own
`websocket_url`, and the test dials what *that* produces. A test that connected
to a literal `"/ws"` would have been green throughout fault 4, because the
gateway served `/ws` the whole time; what was broken was the question the other
side asked. The naive derivation is dialled too, so the test demonstrates it can
tell them apart. `cb-web` cannot simply be called — it is excluded from the
workspace and compiles only for `wasm32` — so a second test reads the shipped
frontend source and fails if that derivation changes shape, which is the signal
to re-port it.

---

## Adding a test

1. Boot a stack: `let stack = Stack::start().await;`. Vary anything through
   `StackConfig`.
2. Get an account: `cb_e2e::flows::funded_user(&stack.api, 0, "my_handle")`.
   It signs in, claims the faucet, and waits until the money is visible on
   chain — never a shortcut that credits an account another way.
3. Assert on responses. `Reply::ok(200)`, `Reply::error(422, "MARKET_HALTED")`.
4. Wait on conditions, never on time: `await_order_status`, `await_account`,
   `await_book_level`, or `wait::expect_within` for anything else.
5. Name the test as a sentence saying what it guarantees, and write a doc
   comment saying why it exists. A regression test whose reason has been
   forgotten gets "simplified" away at the next refactor.
6. Mark it `#[ignore]` if it boots a stack. Static checks stay un-ignored so a
   plain `cargo test` runs them.

### Two rules

**Do not weaken an assertion to get green.** If the product is wrong, leave the
test failing and report it. Several tests in this suite fail today, each for a
real defect; they are written to the contract rather than to the server, because
a test relaxed to match a deviation stops being a contract test and becomes a
description of the bug. That is how the five faults above survived as long as
they did.

**Do not read the database.** A test that reaches into a table to check a row
has not tested the API.

---

## What is not covered, and why

* **An expired session.** `cb-gateway` floors the configured lifetime at
  `session_ttl_hours.max(1)`, so the shortest session any invocation can issue
  lasts an hour — `--session-ttl-hours 0` yields a one-hour session, not an
  expired one. Enforcement exists (a session is rejected once `expires_at <=
  now`), but provoking it needs a clock the test can move or a way to age a
  stored session, and neither is exposed. Covered instead: a session the user
  ended, and a session an operator revoked. Closing this properly needs an
  injectable clock behind a test-only flag.
* **An expired login challenge.** Same reason: the nonce lives five minutes and
  nothing can shorten it. A *replayed* challenge is covered, and it exercises
  the same rejection path.
* **Derivative trading, and the endpoints that need a position.** Every
  perpetual and future is listed and every ticker is checked, but no position is
  opened: a fresh chain has no oracle price, and a derivative order without a
  fresh mark is refused. So `POST /positions/close`, `POST /positions/margin`,
  `POST /positions/leverage` and `POST /admin/markets/{symbol}/settle` have no
  coverage at all, and `GET /account/positions` and `GET
  /markets/{symbol}/funding` are only checked for their empty shape. Closing
  this needs the harness to post an `OracleUpdate` signed by
  `ORACLE_AUTHORITY_KEY` — `make oraclehit` shows the shape — and it is the
  largest gap here.
* **Liquidation, funding and settlement.** Reachable only after the oracle gap
  above is closed. They are exercised in-process by `cb-sim`; the seam between
  them and the gateway is not.
* **`GET`/`POST /admin/sim`.** The bot population and scenario driver. Starting
  a bot population inside a test would make every other number on that stack
  non-deterministic, so it is left to `make simgame`.
* **Market orders, `ioc`/`fok`, `post_only` and `reduce_only`.** Only `limit`
  with `gtc` is exercised. `POST_ONLY_WOULD_CROSS` and
  `REDUCE_ONLY_WOULD_INCREASE` therefore have no test for the condition that
  produces them.
* **The browser.** This suite drives the gateway, not `cb-web`. A page that
  fetches the wrong URL, renders a decimal string as a float, or never
  subscribes would pass everything here. The websocket-path test closes the one
  case of that which has actually bitten.
* **A killed test binary.** `Drop` cannot run, so a stack survives. `make
  e2e-clean` clears it.
