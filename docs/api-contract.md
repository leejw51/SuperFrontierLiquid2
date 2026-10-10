# Gateway API Contract v1

**Status: frozen.** This document is the seam between `cb-gateway` (producer)
and `cb-web` (consumer). Both sides code against it independently. Any change
must land in the same commit as the code that changes it, and must be
announced to both owners.

Base path: `/api/v1`. Content type: `application/json`.

---

## 1. Conventions

**Numbers are decimal strings.** Every price, quantity, balance and PnL value
crosses the wire as a string such as `"64250.50"`. JavaScript numbers cannot
represent the on-chain integer range, and silent precision loss in a trading
UI is unacceptable. The gateway owns conversion between these strings and the
on-chain fixed-point integers.

**Timestamps** are RFC 3339 UTC strings. **Durations** are integer seconds.

**Symbols** identify markets and are uppercase:

| Form | Example | Meaning |
|---|---|---|
| `{BASE}-{QUOTE}` | `BTC-USD`, `AAPL-USD` | spot |
| `{BASE}-PERP` | `BTC-PERP`, `TSLA-PERP` | perpetual |
| `{BASE}-{YYMMDD}` | `ETH-260626` | dated future |

**Authentication.** `Authorization: Bearer <session_token>`. Endpoints marked
🔒 require it; a missing or expired token returns `401`.

**Errors.** Non-2xx responses carry:

```json
{ "error": { "code": "INSUFFICIENT_BALANCE",
             "message": "need 1000.00 USD, have 250.00",
             "details": { "required": "1000.00", "available": "250.00" } } }
```

`code` is a stable SCREAMING_SNAKE identifier the frontend may branch on;
`message` is human-facing; `details` is optional and free-form.

| Code | HTTP | Meaning |
|---|---|---|
| `VALIDATION_FAILED` | 400 | Malformed request body or parameter |
| `UNAUTHORIZED` | 401 | Missing, invalid or expired session |
| `FORBIDDEN` | 403 | Authenticated but not permitted |
| `NOT_FOUND` | 404 | Unknown symbol, order or account |
| `HANDLE_TAKEN` | 409 | Registration handle already in use |
| `HANDLE_REQUIRED` | 422 | First login for this address needs a handle |
| `BAD_SIGNATURE` | 401 | Signature does not recover to the claimed address |
| `CHALLENGE_EXPIRED` | 401 | Login nonce expired or already used |
| `ACCOUNT_SUSPENDED` | 403 | Account suspended by an operator |
| `FAUCET_COOLDOWN` | 429 | Faucet claimed too recently |
| `RATE_LIMITED` | 429 | Too many requests |
| `INSUFFICIENT_BALANCE` | 422 | Not enough free balance |
| `INSUFFICIENT_MARGIN` | 422 | Order would breach initial margin |
| `POST_ONLY_WOULD_CROSS` | 422 | Post-only order would take liquidity |
| `REDUCE_ONLY_WOULD_INCREASE` | 422 | Reduce-only order would grow position |
| `MARKET_HALTED` | 422 | Market suspended or expired |
| `ORACLE_STALE` | 422 | No fresh mark price for this market |
| `PRICE_OUT_OF_BAND` | 422 | A perpetual or future priced more than 10% from the mark; `details` carries `min_price` and `max_price` |
| `NODE_UNAVAILABLE` | 503 | Gateway cannot reach the sequencer |
| `NOT_SUPPORTED` | 501 | This chain cannot perform the action, however you are authenticated |
| `INTERNAL` | 500 | Unexpected failure |

`NOT_SUPPORTED` is not `FORBIDDEN`. `FORBIDDEN` means *you* may not; this means
nobody can, through this gateway, because the chain underneath has no
implementation for it. The two endpoints that answer it today are
`POST /positions/margin` (§6) and `POST /admin/markets/{symbol}/settle` (§10),
and both say why in `message`. A gateway that submitted a transaction it knew
the chain would reject — and then reported success — is what this code exists
to replace.

**Idempotency.** Order placement accepts a `client_order_id`. Re-submitting
the same id within 24 hours returns the original order instead of a duplicate.

---

## 2. Identity and session

Authentication is **address plus signature**, and the whole scheme is
**Ethereum-compatible**. An account is a secp256k1 key pair; an address is
the last 20 bytes of the keccak-256 hash of the uncompressed public key,
rendered as EIP-55 checksummed hex. A key is derived from a BIP-39 mnemonic at
the BIP-44 path `m/44'/60'/0'/0/{index}`.

That means the same mnemonic produces the same address here, in MetaMask, in
`cb-client`, and in the SuperFrontier Wallet. Nothing about this login is
specific to this exchange. Use only disposable test wallets in this prototype;
never import a recovery phrase associated with real assets.

**The mnemonic is never sent to the server.** The client derives the key,
signs a server-issued nonce, and sends the signature. The gateway recovers the
signer from the signature and compares it to the claimed address.

### Two addresses, and why

Logging in proves you control an EVM address. It does **not** hand the gateway
a key, so the gateway cannot sign chain transactions as you. Every account
therefore has two addresses, and the distinction is visible in the API rather
than hidden:

| | What it is | Who holds the key |
|---|---|---|
| `login_address` | the EVM address you signed in with | you, in your wallet |
| `trading_address` | the on-chain account holding your test money | the gateway |

The gateway derives the trading key deterministically, as
`HKDF-SHA256(server_secret, login_address)`. It is never stored, because it can
always be recomputed, and one login address always maps to the same trading
account. The server secret is configuration; rotating it orphans every trading
account, which is why the runbook says not to.

This is a **deliberate simulator trade-off**. The money is test money the
backend mints through the faucet, so custody of it is not a real risk, and
custodial signing is what lets a trade be one click instead of a wallet prompt.
A production system would instead have the chain accept orders signed by the
user's own key, and that is written up as a follow-on rather than pretended
away.

**Message signing is EIP-191 `personal_sign`.** The bytes actually hashed are:

```
keccak256("\x19Ethereum Signed Message:\n" + len(message) + message)
```

where `len(message)` is the decimal byte length as ASCII. Signatures are
65 bytes, hex with a `0x` prefix, laid out as `r || s || v` with `v` being 27
or 28. A `v` of 0 or 1 is also accepted, because some signers produce it.

**Addresses on the wire.** The gateway always *emits* EIP-55 checksummed hex.
It *accepts* any case, and rejects a mixed-case address whose checksum does not
validate — a wrong checksum means a typo or a corrupted copy-paste, and
accepting it would credit a different account.

### `POST /auth/challenge`

```json
// request
{ "address": "0x9858EfFD232B4033E47d90003D41EC34EcaEda94" }

// 200
{ "nonce": "cbl-login-7f2a91c4e8...",
  "message": "SuperFrontierLiquid wants you to sign in.\n\nAddress: 0x9858EfFD232B4033E47d90003D41EC34EcaEda94\nNonce: 7f2a91c4e8\nIssued: 2026-09-18T12:00:00Z",
  "expires_at": "2026-09-18T12:05:00Z" }
```

`message` is the exact string to sign. The client must sign it verbatim rather
than rebuilding it, so that the server can change the wording without breaking
every client. A nonce is single-use and expires in five minutes.

### `POST /auth/login`

```json
// request
{ "address": "0x9858EfFD232B4033E47d90003D41EC34EcaEda94",
  "signature": "0xf445005436439a4398409aee0e0b13702bdee4e3774b6aa67184f0732d3a270a1ef3802a2455afba1374fb2ad23345e89eb7366c9d567fe0e5338df934434e3b1c",
  "handle": "satoshi" }

// 200
{ "user_id": "usr_7c9a1f",
  "handle": "satoshi",
  "address": "0x9858EfFD232B4033E47d90003D41EC34EcaEda94",
  "session_token": "sess_...",
  "expires_at": "2026-09-19T12:00:00Z",
  "is_new": true }
```

There is no separate registration step. A first successful login creates the
account, which is why `handle` is accepted here; it is required when `is_new`
would be true and ignored otherwise. `is_new` tells the client whether to show
the welcome flow.

Handles are 3–20 characters of `[a-z0-9_]`, unique case-insensitively.
`409 HANDLE_TAKEN` if someone has it. A first login that omits `handle`
returns `422 HANDLE_REQUIRED`, because the client cannot know in advance
whether the address is new; it should retry with a handle rather than guess. A signature that recovers to a different
address than claimed is `401 UNAUTHORIZED`, never a hint about which address it
did recover to.

### `POST /auth/dev-login` 🧪

```json
// request
{ "mnemonic": "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
  "index": 0,
  "handle": "tester" }
```

Returns the same shape as `/auth/login`. The gateway derives the key, signs its
own challenge, and discards the key immediately.

**This exists only to make testing easy** — it lets the bot simulator, the
integration suite and a developer poking at curl log in without implementing
client-side signing. It is mounted only when the gateway is started with
`--dev-login`, it is off by default, and it refuses to run when the gateway is
bound to anything other than a loopback address. Sending a mnemonic to a server
is exactly what the real flow avoids, so the affordance is deliberately hard to
turn on by accident.

### `POST /auth/logout` 🔒 → `204`

### `GET /auth/session` 🔒

```json
{ "user_id": "usr_7c9a1f", "handle": "satoshi",
  "login_address": "0x9858EfFD232B4033E47d90003D41EC34EcaEda94",
  "trading_address": "0x8f3b...c21d",
  "roles": ["user"],
  "issued_at": "...", "expires_at": "..." }
```

`roles` is a list containing `user`, and `admin` for an operator account. The
frontend uses it to decide whether to show the admin navigation. It is not a
security boundary — the gateway enforces the role on every admin endpoint
regardless of what the client believes.

### Test vectors

Signature verification MUST be tested against the published SuperFrontier Wallet
vectors, which come from `eth-account` and are therefore the same values any
EVM tooling produces:

| File | What it pins |
|---|---|
| `derivation.json` | mnemonic + index to address and private key, at `m/44'/60'/0'/0/i` |
| `eip191.json` | message to prefixed hash, signature and recovered signer |
| `eip55.json` | lowercase hex to checksummed rendering |
| `bip39.json` | mnemonic to seed |
| `bip39-invalid.json` | phrases that must be rejected, and why |

The canonical all-zero-entropy phrase
`abandon abandon ... abandon about` derives `0x9858EfFD232B4033E47d90003D41EC34EcaEda94`
at index 0. A test that does not reproduce that address is testing a different
derivation than the rest of the ecosystem.


## 3. Faucet

### `GET /faucet` 🔒

```json
{ "available": true,
  "next_available_at": null,
  "cooldown_seconds": 3600,
  "grant": [ { "asset": "USD", "amount": "100000.00" },
             { "asset": "BTC", "amount": "1.00000000" } ],
  "claims_made": 3 }
```

### `POST /faucet/claim` 🔒

```json
// 200
{ "claim_id": "clm_1f2e",
  "tx_hash": "0x...",
  "granted": [ { "asset": "USD", "amount": "100000.00" } ],
  "next_available_at": "2026-09-18T13:00:00Z" }
```
`429 FAUCET_COOLDOWN` when claimed too recently.

---

## 4. Market data (public)

### `GET /markets`

```json
{ "markets": [
  { "symbol": "BTC-PERP",
    "kind": "perp",
    "asset_class": "crypto",
    "base": "BTC", "quote": "USD",
    "display_name": "Bitcoin Perpetual",
    "status": "trading",
    "tick_size": "0.10",
    "lot_size": "0.0001",
    "min_notional": "10.00",
    "price_decimals": 2,
    "size_decimals": 4,
    "max_leverage": "50",
    "initial_margin_bps": 200,
    "maintenance_margin_bps": 100,
    "funding_interval_seconds": 3600,
    "expiry": null,
    "maker_fee_bps": -1,
    "taker_fee_bps": 4 } ] }
```

`kind` ∈ `spot | perp | future`. `asset_class` ∈ `crypto | equity`.
`status` ∈ `trading | halted | expired | settling`.
Fields `max_leverage`, `initial_margin_bps`, `maintenance_margin_bps`,
`funding_interval_seconds` are null for spot. `expiry` is non-null only for
futures.

### `GET /markets/{symbol}/ticker`

```json
{ "symbol": "BTC-PERP",
  "last_price": "64250.50",
  "mark_price": "64248.10",
  "index_price": "64251.00",
  "open_interest": "1284.5000",
  "volume_24h": "128450000.00",
  "change_24h_pct": "2.41",
  "high_24h": "64980.00", "low_24h": "62100.00",
  "funding_rate": "0.000125",
  "next_funding_at": "2026-09-18T13:00:00Z" }
```

### `GET /markets/{symbol}/orderbook?depth=50`

```json
{ "symbol": "BTC-PERP",
  "height": 184203,
  "bids": [ ["64250.00", "1.2500"], ["64249.50", "3.0000"] ],
  "asks": [ ["64251.00", "0.8000"] ] }
```
Levels are `[price, quantity]`, aggregated, best first, at most `depth`
entries per side (default 50, max 200).

### `GET /markets/{symbol}/trades?limit=100`

```json
{ "trades": [ { "id": "trd_...", "price": "64250.50", "qty": "0.2500",
                "side": "buy", "at": "...", "height": 184203 } ] }
```
`side` is the aggressor side.

### `GET /markets/{symbol}/candles?interval=1m&limit=500`

`interval` ∈ `1m | 5m | 15m | 1h | 4h | 1d`.

```json
{ "symbol": "BTC-PERP", "interval": "1m",
  "candles": [ { "t": "2026-09-18T12:00:00Z", "o": "64200.00",
                 "h": "64280.00", "l": "64180.00", "c": "64250.50",
                 "v": "84.2500" } ] }
```

### `GET /markets/{symbol}/funding?limit=100`

```json
{ "history": [ { "at": "...", "rate": "0.000125",
                 "mark_price": "64248.10", "index_price": "64251.00" } ] }
```

---

## 5. Account 🔒

### `GET /account`

```json
{ "login_address": "0x9858EfFD232B4033E47d90003D41EC34EcaEda94",
  "trading_address": "0x8f3b...c21d",
  "nonce": 42,
  "balances": [ { "asset": "USD", "free": "94000.00",
                  "locked": "6000.00", "total": "100000.00" } ],
  "portfolio": {
    "equity": "104250.00",
    "available_margin": "88000.00",
    "used_margin": "12000.00",
    "unrealized_pnl": "4250.00",
    "realized_pnl_24h": "820.00",
    "margin_ratio": "0.115",
    "liquidation_risk": "low" } }
```
`liquidation_risk` ∈ `low | medium | high | critical`.

**`free + locked == total`, always.** `total` is everything the account holds of
that asset and `locked` is the part of it reserved against resting orders, so
`free` is what can actually be spent. The chain models it the same way — locking
an order's collateral does not debit the balance, it records how much of the
balance is spoken for — and a client may rely on the identity holding through a
rest, a partial fill and a cancel.

### `GET /account/orders?status=open&symbol=BTC-PERP`

`status` ∈ `open | filled | cancelled | all`, default `open`.

```json
{ "orders": [
  { "order_id": "0x...", "client_order_id": "cli_...",
    "symbol": "BTC-PERP", "side": "buy", "type": "limit",
    "price": "64000.00", "qty": "1.0000", "filled_qty": "0.2500",
    "remaining_qty": "0.7500", "avg_fill_price": "64000.00",
    "status": "open", "tif": "gtc",
    "post_only": false, "reduce_only": false,
    "created_at": "...", "updated_at": "..." } ] }
```
`status` ∈ `pending | open | partially_filled | filled | cancelled | rejected`.

### `GET /account/positions`

```json
{ "positions": [
  { "symbol": "BTC-PERP", "side": "long", "qty": "1.5000",
    "entry_price": "63000.00", "mark_price": "64248.10",
    "liquidation_price": "58200.00",
    "notional": "96372.15",
    "margin_mode": "cross", "leverage": "10",
    "margin": "9637.21",
    "unrealized_pnl": "1872.15", "unrealized_pnl_pct": "19.42",
    "funding_paid": "-12.40",
    "opened_at": "..." } ] }
```

### `GET /account/fills?limit=100&symbol=`

```json
{ "fills": [ { "fill_id": "...", "order_id": "0x...",
               "symbol": "BTC-PERP", "side": "buy",
               "price": "64250.50", "qty": "0.2500",
               "fee": "6.42", "fee_asset": "USD",
               "role": "taker", "realized_pnl": "0.00",
               "at": "...", "height": 184203 } ] }
```

### `GET /account/transfers?limit=100`

Faucet grants, transfers in and out, settlements and liquidation charges as a
unified ledger:

```json
{ "entries": [ { "id": "...", "kind": "faucet", "asset": "USD",
                 "amount": "100000.00", "balance_after": "100000.00",
                 "memo": "faucet claim clm_14932017d33a",
                 "at": "...", "tx_hash": "0x..." } ] }
```
`kind` ∈ `faucet | transfer_in | transfer_out | fee | funding | realized_pnl |
liquidation | settlement`.

**An entry is written when the chain applies the movement, never when the
gateway submits one.** Submission returns on mempool acceptance, which is not
the chain having moved anything: a transaction can still fail at block build. So
a transfer does not appear here the instant `POST /transfers` answers — it
appears when the block carrying it arrives. The entry is then dated by the
**block's** timestamp and carries the height it landed at, and `balance_after`
is the balance the chain reported **when the gateway processed that block**,
rather than one the gateway computed for itself. The distinction matters on a
replay: the node has no historical balance query, so a gateway catching up on
old blocks dates them correctly and can only report the balance it can see now.
This is the answer to "where did my money go", and it must not contain movements
that did not happen.

The kinds the gateway produces today are `faucet` (one per asset of a grant,
memo `faucet claim <claim_id>`), `transfer_out` and `transfer_in` (one each, for
the sender and for a recipient who has an account here), `liquidation`, and
`funding` — one entry per block that charged it, against the cross-margin
collateral, because the chain charges funding into a single pool and splitting
one movement between two markets that ticked together would mean inventing the
split. A transfer memo is held by the gateway between submission and the
block, because `Transfer` has no memo field on chain; it is lost if the gateway
restarts in that window.

---

## 6. Trading 🔒

### `POST /orders`

```json
// request
{ "symbol": "BTC-PERP",
  "side": "buy",
  "type": "limit",
  "price": "64000.00",
  "qty": "1.0000",
  "tif": "gtc",
  "post_only": false,
  "reduce_only": false,
  "leverage": "10",
  "margin_mode": "cross",
  "client_order_id": "cli_a1b2c3" }
```

`type` ∈ `limit | market`. `price` omitted for market orders.
`tif` ∈ `gtc | ioc | fok`, default `gtc`.
`leverage` and `margin_mode` apply to derivative markets only.

```json
// 201
{ "order_id": "0x...", "client_order_id": "cli_a1b2c3",
  "status": "open", "tx_hash": "0x...",
  "accepted_at": "...",
  "immediate_fills": [ { "price": "64000.00", "qty": "0.2500",
                         "fee": "6.40" } ] }
```

### `DELETE /orders/{order_id}` → `200 { "order_id": "0x...", "status": "cancelled", "tx_hash": "0x..." }`

### `POST /orders/cancel-all`

```json
{ "symbol": "BTC-PERP" }   // omit symbol to cancel across all markets
// 200
{ "cancelled": 7, "tx_hash": "0x..." }
```

### `POST /positions/close`

```json
{ "symbol": "BTC-PERP", "qty": "0.5000" }  // omit qty to close fully
// 200
{ "tx_hash": "0x...", "closed_qty": "0.5000", "avg_price": "64248.10",
  "realized_pnl": "624.05" }
```

### `POST /positions/margin` → `501 NOT_SUPPORTED`

```json
{ "symbol": "BTC-PERP", "action": "add", "amount": "500.00" }
```
`action` ∈ `add | remove`. Isolated-margin positions only.

**Not available on this chain.** `AdjustMargin` is the one transaction that
would move value between a spot balance and margin collateral, and the executor
answers it `Unsupported`. The request is validated — an unknown symbol is still
a `404`, a bad `action` still a `400` — and then refused, because submitting a
transaction the chain will reject is worse than saying no.

The consequence is worth stating because it shapes the product: a faucet grant
credits the **spot** ledger, margin collateral is a different one, and nothing
bridges them. Every account created through the gateway therefore has zero
collateral, and only genesis-funded accounts can trade a perpetual or a future.
Implementing `AdjustMargin` in the executor is what changes this.

### `POST /positions/leverage`

```json
{ "symbol": "BTC-PERP", "leverage": "20", "margin_mode": "isolated" }
```

### `POST /transfers`

```json
{ "to": "0x...", "asset": "USD", "amount": "100.00", "memo": "gg" }
// 200 { "tx_hash": "0x...", "at": "..." }
```

---

## 7. Chain and leaderboard (public)

### `GET /chain/status`

```json
{ "chain_id": 42,
  "height": 184203,
  "state_root": "0x...",
  "block_time_ms": 100,
  "tps_1m": "1240.5",
  "tx_count": 9128401,
  "mempool_depth": 12,
  "validators": 3,
  "synced": true,
  "uptime_seconds": 84021,
  "node_rpc": "127.0.0.1:7700",
  "gateway_listen": "127.0.0.1:8081",
  "ws_path": "/ws",
  "oracle_control": "http://127.0.0.1:8171",
  "sim_mode": "on" }
```

`synced` is true only once the gateway's block stream has reached the head
the node reported when the link opened. A gateway holding a socket to a node
it has received no block from is connected, not synced, and says so.

The last five fields are the **RPC facts** a user or operator page shows so a
person can point another client at the same stack: the sequencer's Cap'n
Proto address, where this gateway listens, where its WebSocket is, the
oracle's scenario control (null when none is configured) and the position of
the simulation switch (§10). None of them is a secret; all of them are what
somebody asks first when they want to drive the chain from the command line.

### `GET /chain/txs?limit=50`

The most recent transactions the chain applied, newest first, from the
gateway's own block stream. Bounded to the last few hundred and empty on a
fresh gateway: this is a dashboard feed, not an archive.

```json
{ "txs": [ { "hash": "0x...", "height": 184203, "index": 3,
             "sender": "0x...", "kind": "PlaceOrder",
             "market": "BTC-USD", "side": "buy", "at": "..." } ] }
```

`kind` is the chain's own name for the transaction (`Transfer`, `Deposit`,
`PlaceOrder`, `CancelOrder`, `BatchCancel`, `PlacePerpOrder`,
`CancelPerpOrder`, `OpenPosition`, `ClosePosition`, `AdjustMargin`,
`LiquidationTrigger`, `OracleUpdate`, `FundingTick`, `FuturesSettle`).
`market` and `side` are null when the kind has neither.

### `GET /chain/blocks?limit=20`

```json
{ "blocks": [ { "height": 184203, "hash": "0x...", "state_root": "0x...",
                "tx_count": 41, "at": "...", "proposer": "0x..." } ] }
```

### `GET /leaderboard?window=24h&limit=100`

`window` ∈ `24h | 7d | all`.

```json
{ "window": "24h",
  "entries": [ { "rank": 1, "handle": "satoshi", "address": "0x...",
                 "equity": "184250.00", "pnl": "84250.00",
                 "pnl_pct": "84.25", "volume": "2840000.00",
                 "trades": 412, "is_bot": false } ] }
```

---

## 8. WebSocket

Endpoint: `GET /ws` (upgrade), at the server **root** — `ws://<host>/ws`, not
under the `/api/v1` base the REST calls use. This was ambiguous in an earlier
revision and the two sides implemented it differently, so it is spelled out
here: strip the API path before attaching the socket path.

Send a session token to receive private channels.

### Client → server

```json
{ "op": "auth", "token": "sess_..." }
{ "op": "subscribe", "channels": ["orderbook:BTC-PERP", "trades:BTC-PERP", "ticker", "chain", "account"] }
{ "op": "unsubscribe", "channels": ["trades:BTC-PERP"] }
{ "op": "ping" }
```

### Channels

| Channel | Auth | Payload |
|---|---|---|
| `orderbook:{symbol}` | no | snapshot then deltas |
| `trades:{symbol}` | no | individual trades |
| `ticker` | no | all-market ticker updates |
| `candles:{symbol}:{interval}` | no | the in-progress candle |
| `chain` | no | new block headers |
| `account` | 🔒 | balance, order, position, fill updates |

### Server → client

Every message carries `channel` and `type`.

```json
{ "channel": "orderbook:BTC-PERP", "type": "snapshot",
  "data": { "height": 184203, "bids": [["64250.00","1.25"]], "asks": [] } }

{ "channel": "orderbook:BTC-PERP", "type": "delta",
  "data": { "height": 184204,
            "bids": [["64250.00","0.00"], ["64249.00","2.00"]],
            "asks": [] } }
```
A delta level with quantity `"0"` means remove that price level. Clients that
miss a sequence must resubscribe; the gateway sends a fresh snapshot on every
subscribe and every reconnect.

```json
{ "channel": "trades:BTC-PERP", "type": "trade",
  "data": { "price": "64250.50", "qty": "0.25", "side": "buy", "at": "..." } }

{ "channel": "ticker", "type": "update",
  "data": { "symbol": "BTC-PERP", "last_price": "64250.50", "mark_price": "64248.10",
            "change_24h_pct": "2.41", "funding_rate": "0.000125" } }

{ "channel": "chain", "type": "block",
  "data": { "height": 184204, "hash": "0x...", "tx_count": 41, "at": "..." } }

{ "channel": "account", "type": "order_update", "data": { /* one order, §5's order object */ } }
{ "channel": "account", "type": "fill",         "data": { /* one fill, §5's fill object */ } }
{ "channel": "account", "type": "position_update", "data": { /* one position, §5's position object */ } }
{ "channel": "account", "type": "balance_update",
  "data": [ { "asset": "USD", "free": "94000.00",
              "locked": "6000.00", "total": "100000.00" } ] }
{ "channel": "account", "type": "liquidation",
  "data": { "symbol": "BTC-PERP", "qty": "1.50", "price": "58200.00",
            "loss": "-7200.00", "at": "..." } }
```

Each account payload is **exactly** the object the matching REST endpoint in §5
returns, field for field, except `balance_update`, whose payload is the array of
balances itself rather than an object wrapping it. An earlier revision wrote
`{ /* balances */ }` here and the two sides read it differently; this spells it
out. A frame missing a field the client requires is dropped, so a partial
payload delivers nothing rather than something.

`order_update` is sent whenever an order changes state: `pending → open`, each
fill, and cancellation. `liquidation` is sent when the engine closes a position
the user did not close themselves; the matching fill is **not** also sent,
because the user placed no order.

Errors arrive as `{ "channel": "system", "type": "error",
"data": { "code": "UNAUTHORIZED", "message": "..." } }`. A session that is
revoked, logged out or expired has its sockets closed: the connection receives
that frame with `UNAUTHORIZED` and is then closed by the server. Revoking a
session that left its socket delivering private data would not be revoking
anything that matters.

The server sends `{"channel":"system","type":"pong"}` in reply to `ping`, and
closes a connection that has not pinged for 90 seconds.

---

## 9. User page 🔒

The user page is a person's own account: who they are, what they hold, what
they have done, and how to get more test money. It is distinct from the
portfolio, which is about open risk right now.

### `GET /user/profile`

```json
{ "user_id": "usr_7c9a1f",
  "handle": "satoshi",
  "address": "0x9858EfFD232B4033E47d90003D41EC34EcaEda94",
  "joined_at": "2026-09-01T09:14:00Z",
  "last_seen_at": "2026-09-18T11:58:00Z",
  "is_bot": false,
  "roles": ["user"],
  "stats": {
    "trades": 412,
    "volume_30d": "2840000.00",
    "realized_pnl_30d": "8420.00",
    "markets_traded": 7,
    "liquidations": 2,
    "faucet_claims": 3,
    "best_rank": 4 } }
```

### `PATCH /user/profile`

```json
{ "handle": "satoshi_v2" }
// 200 -> the updated profile
```
Only the handle is mutable. The address is the identity and cannot change.
`409 HANDLE_TAKEN` if it is in use.

### `GET /user/sessions`

```json
{ "sessions": [
  { "session_id": "ses_3f1a", "issued_at": "...", "expires_at": "...",
    "last_used_at": "...", "user_agent": "Mozilla/5.0 ...",
    "ip_prefix": "203.0.113.0/24", "current": true } ] }
```
`ip_prefix` is truncated rather than exact, because a session list is a
security feature and does not need to be a location log.

### `DELETE /user/sessions/{session_id}` → `204`
### `POST /user/sessions/revoke-all` → `{ "revoked": 3 }`
Revokes every session except the one making the request.

### `GET /user/activity?limit=100&cursor=`

A unified, newest-first feed across trades, faucet grants, funding, transfers,
liquidations and settlements. This is what a person actually wants to read; the
per-type endpoints in sections 5 and 6 are what a table wants to render.

```json
{ "entries": [
    { "id": "act_...", "kind": "fill", "at": "...",
      "summary": "Bought 0.25 BTC-PERP at 64,250.50",
      "symbol": "BTC-PERP", "amount": "-16062.63", "asset": "USD",
      "tx_hash": "0x...", "height": 184203 } ],
  "next_cursor": "eyJ..." }
```
`kind` is one of `fill`, `order`, `faucet`, `funding`, `transfer_in`,
`transfer_out`, `liquidation`, `settlement`. `next_cursor` is null at the end.

---

## 10. Admin 🔒 `admin`

Every endpoint here requires a session whose `roles` include `admin`, enforced
server-side on each request. A non-admin session gets `403 FORBIDDEN`, and the
response never reveals that the endpoint exists for anyone else.

Every mutating admin action writes an audit record with the acting address, so
"who halted this market" always has an answer.

### `GET /admin/overview`

The dashboard's single fetch. One call so the page paints at once rather than
waterfalling.

```json
{ "chain": { "height": 184203, "block_time_ms": 100, "tps_1m": "1240.5",
             "mempool_depth": 12, "validators": 3, "synced": true,
             "state_root": "0x...", "uptime_seconds": 84021 },
  "users": { "total": 1284, "humans": 412, "bots": 872,
             "active_24h": 318, "new_24h": 41 },
  "markets": { "total": 17, "trading": 16, "halted": 1, "expired": 0 },
  "volume": { "volume_24h": "128450000.00", "trades_24h": 84021,
              "fees_24h": "51380.00" },
  "faucet": { "claims_24h": 96, "granted_24h": [ { "asset": "USD", "amount": "9600000.00" } ],
              "total_granted": [ { "asset": "USD", "amount": "412000000.00" } ] },
  "risk": { "open_interest": "48200000.00", "insurance_fund": "1000000.00",
            "accounts_below_maintenance": 3, "liquidations_24h": 27 },
  "health": [ { "component": "node", "status": "ok", "detail": "height 184203" },
              { "component": "database", "status": "ok", "detail": "schema 1" },
              { "component": "bus", "status": "ok", "detail": null },
              { "component": "feed", "status": "degraded", "detail": "SPY stale 42s" } ] }
```
`status` is `ok`, `degraded` or `down`.

### `GET /admin/users?query=&is_bot=&limit=100&cursor=`

```json
{ "users": [ { "user_id": "usr_...", "handle": "satoshi",
               "address": "0x...", "is_bot": false,
               "joined_at": "...", "last_seen_at": "...",
               "equity": "104250.00", "open_positions": 3,
               "trades_24h": 41, "faucet_claims": 3,
               "status": "active" } ],
  "next_cursor": null }
```
`query` matches a handle or an address. `status` is `active` or `suspended`.

### `GET /admin/users/{user_id}`
The full record: profile, balances, positions, recent activity and audit trail.

### `POST /admin/users/{user_id}/suspend`

A suspension stops the account **acting**: every mutating request from it —
placing, cancelling, transferring, claiming — answers `403 ACCOUNT_SUSPENDED`.
Its reads are not stopped, and neither is its `account` channel. The suspension
leaves existing positions open, so the account is still carrying risk it can
still be liquidated on, and refusing it sight of that would leave somebody
holding exposure they are not allowed to look at.

```json
{ "reason": "abusive order flow" }
```
A suspended account cannot place orders or claim the faucet. Its existing
positions stay open, because force-closing someone's position is a different
and much larger decision than stopping them trading.

### `POST /admin/users/{user_id}/unsuspend` → `204`

### `GET /admin/markets`
Every market with live operational state: status, open interest, last price,
oracle freshness, resting order count.

### `POST /admin/markets/{symbol}/halt`
```json
{ "reason": "oracle feed stale" }
```
Halting stops new orders. Resting orders stay on the book, and cancellation
stays allowed — trapping someone's order with no way out would be worse than
the condition that caused the halt.

**Scope: `gateway_local`.** The chain has no halt transaction — a market's
`paused` flag is set in genesis — so a halt stops *this gateway* accepting
orders and changes what it publishes, and binds nothing else. Anything trading
the sequencer's RPC directly, including the bot simulator, keeps trading a
halted market at full speed, and the halt does not survive a gateway restart.
That is a real limit of the product and not a wording choice, so it is labelled
wherever it appears: in the audit record's `scope` and `enforcement`, and in the
`override_scope` field of each `GET /admin/markets` row that carries an
override. An operator deciding what to do next during an incident needs to know
which of those things their halt did.

### `POST /admin/markets/{symbol}/resume` → `204`

### `POST /admin/markets/{symbol}/settle` 🔴 → `501 NOT_SUPPORTED`
```json
{ "settlement_price": "3200.00" }
```
**This chain settles a dated future itself and the gateway cannot do it for
you.** An earlier revision of this document described force-settling as closing
every position at the given price. The chain has no such transaction:
`FuturesSettle` names a market and carries **no price** — the engine computes
settlement from its own oracle and mark TWAP at the future's expiry height — and
it is accepted only from the block proposer, which the gateway is not.

The request is still validated in full, because the validation is the part an
operator can act on, and then refused:

* the market must be a dated future, and `expired`, `halted` or `settling`;
* `settlement_price` must parse and be greater than zero;
* the market must have a **fresh mark price**, or the answer is
  `422 ORACLE_STALE`. A market whose oracle has never published has no mark to
  check a settlement price against, and a deviation computed from a zero mark is
  not a deviation of nought percent — it is nothing at all, which would let both
  the band check below and any client-side confirmation built on it pass
  vacuously at exactly the moment the market is least understood;
* a price more than 10% from that mark is refused unless `"force": true`.

Every attempt is written to the audit trail with `"outcome": "refused"` and the
reason, whether it passed validation or not. Nothing is submitted to the chain.

### `PATCH /admin/markets/{symbol}/risk`
```json
{ "max_leverage": "20", "initial_margin_bps": 500,
  "maintenance_margin_bps": 250 }
```
Raising margin requirements can put open accounts below maintenance
immediately. The response says how many, and who, so the operator finds out
before the liquidation engine does:
```json
{ "applied": true,
  "accounts_now_below_maintenance": 3,
  "affected": [ { "user_id": "usr_...", "handle": "dip_hunter",
                  "margin_ratio_before": "0.061", "margin_ratio_after": "0.041" } ],
  "scope": "gateway_local",
  "enforcement": "this gateway validates new orders and renders liquidation prices against these numbers. The executor gates margin on its own table and does not read them, so `affected` is who the change would break if the chain enforced it. It does not survive a gateway restart." }
```

Send `?dry_run=true` to compute the consequence without applying it. The
response is identical except that `applied` is false and nothing changed. An
operator should be able to find out what a change would do without doing it,
and a count alone does not tell them whom to warn.

**Scope: `gateway_local`, as `scope` and `enforcement` say in every response.**
A risk change feeds this gateway's pre-submission checks and the liquidation
price it displays. The executor margins against a table of its own and reads
neither number, so `affected` names accounts this chain will not in fact
liquidate, measured against a threshold it does not hold. The list is still
worth having — it is the exposure the change *would* create — but an operator
must not read it as "who is about to be liquidated".

### `GET /admin/faucet`
```json
{ "enabled": true,
  "cooldown_seconds": 3600,
  "grant": [ { "asset": "USD", "amount": "100000.00" } ],
  "max_claims_per_user": null,
  "claims_24h": 96,
  "granted_total": [ { "asset": "USD", "amount": "412000000.00" } ],
  "authority_balance": [ { "asset": "USD", "amount": "9588000000.00" } ] }
```
`authority_balance` is what the faucet account has left. A faucet that has run
dry looks exactly like a broken faucet to a user, so the dashboard shows it.

### `PATCH /admin/faucet`
```json
{ "enabled": true, "cooldown_seconds": 1800,
  "grant": [ { "asset": "USD", "amount": "250000.00" } ] }
```

### `POST /admin/faucet/grant`
```json
{ "user_id": "usr_7c9a1f",
  "grants": [ { "asset": "USD", "amount": "1000000.00" } ],
  "reason": "load test" }
// 200 -> { "claim_id": "clm_...", "tx_hash": "0x..." }
```
Bypasses the cooldown. Audited like everything else.

### `GET /admin/audit?actor=&action=&subject=&limit=100&cursor=`
```json
{ "records": [ { "record_id": "aud_...", "actor": "0x...",
                 "action": "market.halt", "subject": "BTC-PERP",
                 "detail": { "reason": "oracle feed stale" },
                 "at": "..." } ],
  "next_cursor": null }
```

### `GET /admin/sim` and `POST /admin/sim`

Drive the bot population and the market scenario. Because this is a
simulator, the operator may change the rules of the game while it runs, and
the whole of that power is a three-position **switch** plus a scenario:

| `mode` | What trades |
|---|---|
| `off` | Nobody. There is no default buyer or seller; a lone human finds an empty book. |
| `on` | A default buyer and seller: one simulator worker, eight funded traders, about twenty signed orders a second, so the books move. |
| `max` | The stress test: six workers, sixty funded traders in disjoint ranges, tens of thousands of signed orders a second, every signature recovered by the node. |

```json
// GET
{ "running": true, "mode": "on", "scenario": "calm", "seed": 12648430,
  "bots": { "market_maker": 4, "noise": 4 },
  "elapsed_seconds": 1840, "orders_submitted": 184020,
  "invariant_violations": 0 }

// POST
{ "action": "mode", "mode": "max" }
{ "action": "scenario", "scenario": "crash" }
{ "action": "start", "seed": 42 }
{ "action": "stop" }
```

`action` is `start`, `stop`, `scenario` or `mode`. `start` without a `mode`
means `on`; `stop` is `mode: off`. Switching to the mode already running is a
no-op rather than a restart. Scenarios are `calm`, `trend_up`, `trend_down`,
`crash`, `squeeze` and `chop`; a scenario change is forwarded to the oracle's
control API when the gateway was given one, so it reprices every derivative
market rather than being remembered in one process.

Two honesty notes about the status. The population behind the switch is the
measured `cb-sim` load generator: its even-numbered traders rest orders and
its odd-numbered traders cross them, and those two halves are reported under
`market_maker` and `noise`; the other four bot kinds are accepted in a request
and not run. And `cb-sim` reports nothing back over the wire, so
`orders_submitted` is the number of transactions the **chain applied** since
the switch was turned on, as counted from the gateway's own block stream.

These endpoints exist because this is a simulator. A production deployment does
not mount them, and the gateway refuses to mount them together with a
non-loopback bind unless explicitly forced.
