# UI/UX review — all eight screens, against the live stack

**Reviewed:** 18 September 2026, in Chrome, against a freshly initialised chain
(node RPC 7720, gateway 8121, web 3020) and against the mock backend, at 1920
and 390, in both themes and both densities.

**Standards used:** `docs/design-system.md`, `docs/design/reference.html` (served
at 8122 and compared side by side), `docs/api-contract.md`.

**A note on timing.** This review ran between 23:09 and 23:30 local time
against `cb-node` and `cb-gateway` built from the tree as it stood at 23:09.
Other engineers were working in `rust/` concurrently: `http/account.rs`,
`http/dto.rs` and several other gateway files carry modification times inside my
review window, and the gateway binary itself was rebuilt at 23:30. Every
observation below is of the build I actually ran. I re-checked my three
source-level claims against the current files at the end of the session and all
three still hold: the CORS `allow_methods` list at `http/mod.rs:156` still omits
`PATCH`, the 60/20/20 split at `screens/chain.rs:283` is unchanged, and
`.topbar .nav { display: none }` is still at `app.css:660`. The one claim most
likely to have moved under me is the `/account` response shape noted in finding
13, since that is exactly the file that changed — check it before acting on it.

**Screenshots:** `docs/ui-review/`. Every screenshot referenced below was opened
and read; nothing here is inferred from a filename. The designer's reference is
captured alongside for comparison: `ref-01-terminal-1920-dark.png`,
`ref-02-admin-1920-dark.png`, `ref-03-account-1920-dark.png`.

---

## How to read this

Findings are ordered by severity. Each says whether it is a **defect** (the
screen is wrong, or contradicts itself, or contradicts the spec or the contract)
or a **matter of taste** (a judgement I would argue for but would not block on).

Where the root cause is in the gateway rather than in `cb-web` I say so, because
the fix lands in a different file — but it stays in this report, because the
user cannot tell the difference and the screen is what they see.

**The headline.** The fixture data flatters this design badly. On mock the
terminal is genuinely excellent — dense, legible, honest
(`app-01-trade-mock-1920-dark.png`, which stands up well beside
`ref-01-terminal-1920-dark.png`). On a fresh chain the
same screens show a jade `0.000000` as the headline price, two panels of
shimmering skeletons that never resolve, a portfolio hero of em dashes above a
ledger that prints the very figures the hero says are unknown, and an order
ticket that says a 6,000-dollar order is worth 0.00. A reviewer who only ever
opened mock mode would sign this off. Sections 1–18 are mostly things you cannot
see with fixtures loaded.

---

# Critical

## 1. There is no navigation at all on a phone — *defect*

**Where:** every screen, at ≤560px container width.
**Screenshot:** `app-09-trade-mock-390-dark.png`, `app-36-admin-LIVE-390.png`

At phone width `.topbar .nav { display: none }` (`styles/app.css:660`) hides
every navigation link and **nothing replaces it**. There is no hamburger, no
drawer, no bottom bar; the only `position: fixed` element on the page is the
toast container. Portfolio, Faucet, Leaderboard, Chain, Account and Admin become
unreachable. I confirmed by enumerating every `<a>`: all six report
`offsetParent === null` and `width: 0`.

A phone user therefore cannot reach the faucet, which is the only way to get
money, which is the only way to trade. The product is a single screen with no
exits.

A second, related loss: `.pref-only` is hidden at ≤700px
(`styles/shell.css:131`), so the theme switch and the density switch also
disappear. The brief requires both themes and both densities to work; on a phone
neither can be chosen, and there is no `prefers-color-scheme` fallback.

**What I would change.** The design system says the top bar "drops the
navigation to make room for the price" — that is the right instinct, but
*drops to* has to mean somewhere. Put the six routes behind the user pill, which
is already in the bar at every width and is already a link to `/account`, or add
a five-item bottom bar above the chain rail. Keep the two preference switches
inside that menu rather than deleting them.

---

## 2. A new user is told their account was not created, when it was — *defect*

**Where:** `/`, the paste-a-phrase path, first sign-in for an address.
**Screenshots:** `app-19-handle-required.png`, `app-20-login-401-error.png`

The sequence, which every new user on the paste path hits:

1. Paste the phrase, Continue, **Sign and continue** →
   `422 HANDLE_REQUIRED`. The brass banner and the handle field appear. This
   part is exactly as designed.
2. Type the handle, **Sign and continue** → `401`. The client re-signs the
   **same, already-spent challenge nonce** instead of requesting a fresh one.
3. The handle field disappears. The error that replaces it reads:

   > **That did not go through**
   > no outstanding challenge for that address; request one from
   > POST /auth/challenge and sign it within five minutes

4. Pressing the button again fails identically, forever. The only exit is
   **Back**, which returns to the paste step.

Four separate failures in one screen:

- **The message is written for a programmer.** "request one from
  POST /auth/challenge" names an endpoint the user cannot call and a control
  that is not on screen. §11: *name things by what the user understands, not by
  how the system is built.*
- **"That did not go through" is vague**, which §11 forbids by name.
- **The remedy is unreachable.** There is no button that requests a new
  challenge.
- **The screen contradicts itself.** The panel still displays
  `Nonce expires 14:20:39` — a time in the future — directly above a message
  saying there is no outstanding challenge.

And the sting: **the account was created anyway.** After going Back, re-pasting
and signing once more, I was signed in as `admin` with
`roles: ["user","admin"]` and no handle was asked for again. So the one screen
in the product that the design system calls "the hardest screen" tells a new
user their account creation failed at the exact moment it succeeded. Most people
will try a different handle, or conclude the product is broken.

**What I would change.** Either side can fix this and both should be considered:
the client can request a fresh challenge before the handle retry, or the gateway
can stop spending the nonce on a `422 HANDLE_REQUIRED` that it issued itself.
Independently of which, the screen must keep the handle field and its value on
screen through the failure, and the message should read as a state rather than
an error: *your sign-in window expired — sign again*, with the button doing
exactly that.

---

## 3. Signing in drops you into somebody else's account — *defect*

**Where:** the redirect from `/` to `/trade` after a successful live sign-in.
**Screenshots:** `app-21-trade-live-empty-1920.png` (what you get) versus
`app-22-trade-LIVE-empty-1920.png` (what you should get)

The backend is selected by the `?gateway=live&api=…` query string. After
sign-in the client navigates to `/trade` as a **fresh page load** with no query
string, so the app re-reads the URL, finds nothing, and falls back to the mock
backend — while remaining genuinely signed in to the live gateway.

What the user sees one second after signing in as themselves:

- the user pill reads **satoshi**, not their handle;
- **Equity 104,250.00** in the top bar, which is not their money;
- five open orders and three positions they did not place;
- and, across the full width of the screen, the hazard-textured banner
  **"SOL-PERP is 0.67% from liquidation — Mark 141.05 against a liquidation
  price of 140.10"** with **Add margin** and **Reduce** buttons.

The design system reserves that treatment for "money about to be lost" and calls
it "the loudest the interface ever gets". Here it fires on fixture data, for a
real signed-in user, as their first impression of the product.

In-app navigation (clicking Faucet, Portfolio, …) *does* preserve the live
backend — I verified the app stays live across five route changes. It is only
the post-login full page load that loses it.

**What I would change.** Persist the backend selection where the session token
already lives, and make the post-login transition a client-side route change.
Failing that, carry the query string through the redirect. As a backstop, never
render mock fixtures while a real session token is present — refuse and say so.

---

## 4. Every PATCH action in the product silently does nothing — *defect (gateway one-liner, but the UI hides it)*

**Where:** Edit handle (`/account`), Save cooldown and the risk change
(`/admin`).
**Screenshots:** `app-31-admin-risk-dialog.png`,
`app-32-admin-risk-applied.png`

`crates/cb-gateway/src/http/mod.rs:156`:

```rust
.allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
```

`PATCH` is missing, so the browser's preflight rejects it:

> Access to fetch at '…/admin/markets/BTC-PERP/risk' has been blocked by CORS
> policy: Method PATCH is not allowed by Access-Control-Allow-Methods

That kills `PATCH /user/profile`, `PATCH /admin/markets/{symbol}/risk` and
`PATCH /admin/faucet` — the handle change, the margin-requirement change and the
faucet cooldown.

The UI half of this is the serious half. I clicked **Apply to BTC-PERP** on the
risk dialog. The dialog did not close, did not show a result, did not show an
error, did not show a spinner, and did not disable its button. It was pixel-for-
pixel identical before and after (compare `app-31` and `app-32`). The audit
trail confirms nothing was applied. **An operator has no way whatsoever to know
that a destructive change did not take effect** — and the design system's own
framing for this screen is "somebody acts under pressure, rarely, on incomplete
information, and every action is one they cannot take back."

Note the inconsistency inside one console: **Stop the faucet** on the same page,
failing the same way, *did* raise a toast. Two dialogs, same failure, opposite
feedback.

**What I would change.** Add `Method::PATCH` to the allow list. Independently of
that, no mutating control in this product should be able to fail without saying
so — the risk dialog needs the `data-loading` button state the design system
already specifies, and an error region.

---

## 5. Force-settle's deviation check inverts exactly when it is needed — *defect*

**Where:** `/admin` → Settle, on any market whose oracle has not published.
**Screenshot:** `app-35-admin-settle-priced.png`

The design document calls the deviation line the second of five devices standing
between an operator and a mistyped settlement price, and the gateway's 10% band
the third. With `Last oracle mark 0.00`, typing a settlement price of 32,000
produces:

> ⚠ **+0.00%** above the last oracle mark of **0.00**. Inside the gateway's 10%
> band.

Settling at 32,000 against a mark of zero is an unbounded deviation. The dialog
computes `0/0`, reports `+0.00%`, and **declares it safe**. Because the UI
believes it is inside the band, the `force: true` checkbox — the design's
explicit-override device — is never offered either. Two of the five safety
devices fail open, and they fail open under precisely the condition (no mark)
that makes a typo most dangerous.

This is not hypothetical on a fresh chain: `/admin/markets` returns
`mark_price: "0.00"` and `oracle_fresh: false` for every perpetual.

**What I would change.** When the mark is absent or zero, do not print a
percentage. Print *there is no oracle mark to compare against* in vermilion,
require the `force` checkbox unconditionally, and say in the checkbox label that
the gateway cannot validate this price.

---

## 6. Against a real unresponsive gateway, the connection indicator says "Live" forever — *defect*

**Where:** the top bar, on every screen.
**Screenshot:** `app-41-trade-LIVE-gateway-frozen-110s.png`

Mock mode's **Drop the socket** control simulates a clean WebSocket `close`, and
the client handles that beautifully (see "What is good"). It does not simulate
the commoner real failure: a gateway that stops answering without closing
anything. I produced that with `kill -STOP` on the gateway process and watched
the terminal, signed in, on live data.

| Elapsed | Connection indicator | Price |
|---|---|---|
| 33s | `● Live · 33s`, jade dot | `stale-rule` class applied |
| 110s | `● Live · 1m`, jade dot | brass **Price 1m old** badge in the strip |

§9 requires that no `pong` within 90 seconds turns the indicator **vermilion**
and makes it read **Offline** with the time it dropped. At 110 seconds the
product had never left the `live` state. Of the four states §6 specifies —
`live`, `reconnecting`, `stale`, `down` — only the first is ever reached against
a stalled server.

The one thing that did fire is good and worth keeping: the market strip grew a
brass **Price 1m old** badge and a brass rule under the price, which is exactly
§9's *stale price* treatment. But that badge lives on the trading screen only.
On Portfolio, Account, Faucet, Leaderboard or Admin, the sole indicator is the
top bar, and the top bar says the feed is live while it has been dead for two
minutes. The design system puts the connection status in the top bar "always"
precisely so this cannot happen.

Recovery is clean: on `kill -CONT` the indicator returned to `Live · 0.9s`
within a second.

This is the honest answer to the brief's question about whether it is ever
ambiguous that a price is stale. In mock mode, no. Against a real server that
stops talking, yes — and the ambiguity is on every screen but one.

**What I would change.** Drive the indicator from the age of the last message,
which the client is already computing and already printing: brass
*Reconnecting* past a few seconds, vermilion *Offline* past 90, both with the
age. Add a stalled-socket case to the mock control so this state is reachable
in development.

---

# High

## 7. The order ticket says every order costs nothing — *defect*

**Where:** `/trade`, both backends, signed in or out, limit or market.
**Screenshots:** `app-05-ticket-market-qty.png`,
`app-06-ticket-limit-priced.png`, `app-25-ticket-LIVE-filled.png`

With a limit price of 60,000.00 and a quantity of 0.1 BTC — an order worth
6,000.00 USD — the ticket reads:

| Order value | `0.00 USD` |
| Initial margin | `0.00` |
| Taker fee +0.03% | `−0.00` |

I reproduced this with values typed, with values set by the lot stepper, in
Limit and in Market mode, on mock and on live, on a perpetual and on a spot
market. The three cost rows never leave zero.

The ticket **can** do the arithmetic — the hint under the disabled button reads
*"Needs 600.00 USD of margin, 0.00 free"*, which is 6,000 ÷ 10×. So one screen
tells the user this order costs 0.00 and, four lines lower, that it needs 600.00
of margin. The blotter two panels away computes it correctly too: the resting
BTC-USD order shows `Value 6,000.00`
(`app-41-trade-LIVE-gateway-frozen-110s.png`). The number exists in the client;
it just never reaches the three rows the user reads before committing.

The brief asks whether the ticket is unambiguous about what you are about to do
and what it will cost. It is unambiguous about the first — the button carries
*Buy 0.10000 BTC* — and it is actively wrong about the second. The designer's
reference (`ref-01-terminal-1920-dark.png`) shows this block populated, with a
**Liquidation price** row the implementation does not have at all.

Two smaller things in the same block:
- **`−0.00`** is a negative zero, on a row whose own label says `+0.03%`.
- **Liquidation price is missing.** On a 50×-capable perpetual it is the most
  consequential number on the ticket, and the reference includes it.

---

## 8. The risk dialog's "Now" column is wrong, and the "New" fields inherit it — *defect*

**Where:** `/admin` → Risk.
**Screenshot:** `app-31-admin-risk-dialog.png`

For BTC-PERP the dialog shows:

| Parameter | Now | New |
|---|---|---|
| Maximum leverage | `1×` | `1` |
| Initial margin | `0 bps` | `0` |
| Maintenance margin | `0 bps` | `0` |

`GET /markets` for BTC-PERP returns `max_leverage: "50"`,
`initial_margin_bps: 200`, `maintenance_margin_bps: 100`. Every figure in the
Now column is wrong; `/admin/markets` does not carry these fields, and the
client defaults to 1/0/0 rather than reading them from the public endpoint it
already calls.

The design document is explicit about why this matters: *"A form showing only
the new value asks the operator to remember the old one, and the old one is the
whole basis of the judgement."* Here the old one is displayed and false.

Worse, the New fields are pre-filled **from the false Now**. An operator who
opens this dialog to raise leverage, edits only that field, and presses Apply
would send `initial_margin_bps: 0` and `maintenance_margin_bps: 0` — abolishing
the margin requirement on the market — believing they had left those rows
untouched. That is the brief's "can you do something irreversible by accident",
and the answer is yes. It is currently masked only by finding 4.

---

## 9. The risk dialog tells the operator a safety feature does not exist — *defect (documentation drift)*

**Where:** `/admin` → Risk, the brass note.
**Screenshot:** `app-31-admin-risk-dialog.png`

> **The count comes back after the change, not before**
> … **There is no dry run in the contract**, so this dialog shows you the answer
> in place rather than throwing it into a toast you can miss.

`docs/api-contract.md:769` now says *"Send `?dry_run=true` to compute the
consequence without applying it"*, and the response now carries `affected` with
handles and before/after margin ratios. I called it against the running gateway:

```
PATCH …/risk?dry_run=true → {"applied":false,"accounts_now_below_maintenance":0,"affected":[]}
```

Both of the two asks the design system raised in §10.8 have been granted, and
neither the dialog nor the design document has caught up. The screen is denying
the operator a preview that the gateway will hand over, and telling them the
preview does not exist. §12 rule 6 asks the engineer to check the contract
before inventing a treatment; the contract moved.

**What I would change.** Call the dry run when the fields change, show
`accounts_now_below_maintenance` and the `affected` handles *before* the Apply
button, and delete the note.

---

## 10. The chain explorer fabricates a transaction breakdown, under a note swearing it does not — *defect*

**Where:** `/chain`, block detail, "What this block carried".
**Screenshots:** `app-40-chain-LIVE.png`, `app-14-chain-mock.png`
**Source:** `crates/cb-web/src/screens/chain.rs:281`

```rust
let orders  = block.tx_count.saturating_mul(6) / 10;
let cancels = block.tx_count.saturating_mul(2) / 10;
let other   = block.tx_count.saturating_sub(orders + cancels);
```

The breakdown is a hardcoded 60/20/20 split of a single number. There is no data
behind it — `/chain/blocks` returns headers only, exactly as the note beneath
says. Live block 2 contains one transaction, and that transaction is the limit
order I placed. The screen reports:

- **0** order transactions
- **0** cancellations
- **1** funding, transfer, faucet and liquidation transactions

Every line is false, and the panel closes with:

> The contract's `/chain/blocks` carries block headers, not the transactions
> inside them, so this is a summary of the header's own count **rather than a
> list this client invented**.

The disclaimer makes this worse than a plain invention, because it tells a
reader who is being careful that they can trust the numbers. Each of the three
rows also prints the **block's** hash in the transaction-hash column, so three
different "transactions" appear to share one hash.

In fairness: the designer's own `reference.html` invents a full per-transaction
list here, which `/chain/blocks` cannot support either. The implementation was
right to refuse the list. It should have refused the split too.

**What I would change.** Print `tx_count` and stop. Keep the note, which is
excellent once it is true.

---

## 11. Two requests are malformed, return 400, and fail silently — *defect*

**Where:** `/account` activity feed, `/admin` user table.
**Screenshots:** `app-28-account-LIVE-1920.png`, `app-29-admin-LIVE-1920.png`

The client sends empty parameters and a trailing ampersand:

```
GET /user/activity?limit=60&cursor=&        → 400 VALIDATION_FAILED  "cursor is not valid"
GET /admin/users?query=&is_bot=&limit=60&   → 400
```

Dropping the empty `cursor` returns the real data — the faucet claim is there.
So on the account page, the activity feed (the largest region on the page, and
the one the design calls "what a person actually wants to read") is **empty for
every user, always**, and on the admin console the user table is empty while its
own panel header counts "Users 1".

Neither failure is shown. Both regions render a column header row over blank
space: no error state, no empty state, no retry. The gateway even supplied a
usable message — "cursor is not valid" — which the client discards. §8 requires
an error state that says what happened and what to do.

---

## 12. Empty and loading are the same picture — *defect*

**Where:** `/trade` order book and Recent trades, on any market with no data.
**Screenshots:** `app-22-trade-LIVE-empty-1920.png`,
`app-27-book-LIVE-skeleton-asks.png`

On a fresh chain the order book and the tape render skeleton rows — 40 `.skel`
elements in the tape alone — and keep shimmering forever, because there is
nothing to arrive. A skeleton means *this is loading*. These are not loading;
they are empty, which on day one is the overwhelmingly likely state.

The wrapper carries `aria-hidden="true"`, so to a screen reader these panels are
simply blank.

The pattern is already in the codebase and applied well one panel to the left:
the blotter shows *"No orders yet — orders you place that do not fill
immediately will rest here until they do. Place your first order from the ticket
on the right."* The book and the tape need the same three lines.

The chart is a third variant: a bare monospace string, **"Waiting for candles"**,
in the corner of a 570px-tall empty panel — no glyph, no heading, no action.

---

## 13. The portfolio hero is blank above a ledger that prints the same figures — *defect*

**Where:** `/portfolio`, live, signed in.
**Screenshot:** `app-38-portfolio-LIVE-1920.png`

The account holds 100,000.00 USD free, 6,006.00 locked, equity 106,006.00. The
equity hero shows:

| Equity | Unrealised | Realised 24h | Free USD | Locked USD |
|---|---|---|---|---|
| ▬ | — | — | — | — |

Six hundred pixels below, the ledger on the same page prints **+100,000.00 USD**
and **+1.00 BTC** correctly. So the screen says "unknown" for numbers it is
displaying.

Meanwhile the panel immediately to the right is confidently specific: a jade
**Low risk** badge, **0.0%** at display size, a four-zone gauge with the needle
at rest, **Position notional 0.00**, **Effective leverage 0.00×**. Three
conventions for absence coexist in one panel — em dash, bare zero, and a
positive assertion of low risk.

A likely cause, which is worth checking first: `GET /account` on the running
gateway returns a single `address` field, where the contract (§5) specifies
`login_address` **and** `trading_address`. A response-shape mismatch on that
endpoint would explain why the hero — fed by `/account` — is blank while the
ledger, fed by `/account/transfers`, prints its figures correctly.

There is also **no equity curve**, which §10.3 makes the centrepiece of this
screen. Its space is about 200px of empty black with nothing said about it.

---

## 14. The admin faucet panel hides a hundred million dollars behind a one-thousand — *defect*

**Where:** `/admin` → Faucet.
**Screenshot:** `app-30-admin-LIVE-lower.png`

`GET /admin/faucet` returns `authority_balance` and `granted_total` as **arrays
across eight assets**. The panel renders the first element of each, unlabelled:

| Shown | Actual |
|---|---|
| Granted total `1.00` | 1.00 BTC **and** 100,000.00 USD |
| Authority left `1,000.00` | 1,000 BTC, 100,000,000.00 USD, 10,000 ETH, … |

Read beside the line directly under it — *Grant: 100,000.00 USD · 1.00 BTC* — an
operator would reasonably conclude the faucet has one hundredth of a single
grant left and is about to fail. It has a thousand grants left.

The design system asks for a derived figure here for exactly this reason:
*"The useful figure is not what the authority account holds but how long it
lasts… shown as about 998 days of grants left. A faucet that has run dry looks
exactly like a broken faucet to a user."* That figure is absent, and what
replaced it is misleading in the dangerous direction.

Also missing from this panel: the hand grant (`POST /admin/faucet/grant`), which
§10.8 specifies with its own confirmation and a button carrying the amount.

---

## 15. "Stop the faucet" fires on one click, with no confirmation — *defect*

**Where:** `/admin` → Faucet.
**Screenshot:** `app-30-admin-LIVE-lower.png`

A bare vermilion **Stop the faucet** button sits beside **Save cooldown**. I
clicked it; no dialog appeared and the request went straight out. (It failed
only because of finding 4.)

Turning the faucet off stops every new player from getting money — it is the
single most consequential switch on the admin console, and it has less
protection than halting one market, which correctly opens a dialog and requires
a typed reason. §10.8: *every destructive action behind a dialog that states
what it is about to do before it offers a button that does it.*

**What I would change.** The same dialog pattern as Halt: what stops, what keeps
running, a required reason, and a button reading *Stop the faucet*.

---

## 16. The landing hero greets a first visitor with a green zero — *defect*

**Where:** `/`, live.

`BTC-PERP` · **0.000000** in jade · **+0.00%**, over an order book of grey
shimmer bars, captioned:

> This is the order book from the terminal itself, at the density you will trade
> in. **Nothing on this page is a picture of the product.**

The caption is true and that is the problem: the real component, fed real data,
renders a price of zero in the colour that means *up*, above a loading animation
that will never finish. The design system makes the hero "the thing they are
about to use"; on a fresh chain what a stranger is about to use appears to be
broken.

(The six decimals are correct — this genesis gives BTC-PERP `price_decimals: 6`
and a tick of `0.000001`. See finding 24 for the consequences of that.)

Below the fold, the third explainer card reads **Height 0**, *"0.0 tx/s from 0
validators, state root 0x0000…0000"*, under the heading *"Look underneath any
time you like"*.

**What I would change.** A market with no trades has no price. Show an em dash
and the word *no trades yet*, in `--text-muted`, with no percentage and no
direction glyph. Give the empty book the invitation the blotter already has.

---

## 17. "Every movement here is traceable to a block", and the block column says 0 — *defect*

**Where:** `/portfolio` ledger, `/faucet` grant history.
**Screenshots:** `app-38-portfolio-LIVE-1920.png`,
`app-24-faucet-LIVE-claimed-cooldown.png`

Both tables render a **Block** column, and both print `0` for the faucet grant
that is in block 1. `GET /account/transfers` does not carry a height field at
all — the contract's entry shape is
`{id, kind, asset, amount, balance_after, memo, at, tx_hash}` — so the zero is
invented. The portfolio panel captions itself *"Every movement here is
traceable to a block"*, which the column immediately falsifies.

The same rows show **Balance after 0.00** for a 100,000.00 USD grant. That value
does come from the gateway, but it is printed as fact with nothing marking it as
suspect.

**What I would change.** Drop the column until the contract carries a height, or
show an em dash. The transaction hash beside it is real and does the traceability
job on its own.

---

## 18. A first visitor cannot place a single order — *defect (gateway, surfaced badly)*

**Where:** `/trade`, live, after claiming the faucet.
**Screenshots:** `app-25-ticket-LIVE-filled.png`,
`app-26-trade-LIVE-btcusd-oneorder.png`

After claiming 100,000.00 USD and 1.00 BTC, the ticket's Buy button stays
disabled on every market, with the hint:

> Needs 6,000.00 USD of margin, **0.00 free**

The gateway reports `available_margin: "0.00"` while the same response reports
`free: "100000.00"` and `equity: "100000.00"`. The client trusts
`available_margin` alone, so the user is told they have nothing, immediately
after being given a hundred thousand.

Separately, `POST /orders` on any perpetual returns
`500 INTERNAL — "no mark price for perp:0"`, a raw Rust error string, rather than
the `422 ORACLE_STALE` the contract defines and the design system has a designed
state for (§9, *Stale price*). Spot markets do accept orders.

So the loop a new player is invited into — claim, trade, appear on the
leaderboard — is closed at step two, and the explanation given contradicts the
balance shown on the next screen over. This is the brief's "does any error
message blame the user for something the system did": the user is told they have
no money.

---

---

## 19. The sessions panel shows no sessions and offers to sign out zero of them — *defect*

**Where:** `/account` → Active sessions.
**Screenshot:** `app-28-account-LIVE-1920.png`

`GET /user/sessions` returns one session, `current: true`, with a real user
agent. The panel renders **zero rows**, a count pill of **0**, and a vermilion
**Sign out 0 other sessions** button.

Three problems: the current session — the one thing this panel exists to
show — is dropped; the empty table has no empty state; and a destructive-looking
button is offered for an action with no subject. §10.7 specifies the opposite:
the current session takes a neutral wash and a *This session* badge, and the bulk
button *counts and names the others*.

**What I would change.** Render the current session with its badge and no revoke
control, and hide the bulk button when the count of others is zero.

---

# Medium

## 20. Zeros that mean "unknown", signed and coloured — *defect*

Across the product, an absent value is rendered as a zero that carries a sign
and a market colour, which makes it read as a real, directional figure:

| Where | Shown | Problem |
|---|---|---|
| `/portfolio` risk panel | `Funding paid 24h  +0.00` in **vermilion** | positive sign, loss colour, on a zero |
| `/account` statistics | `Realised · 30 days  +0.00` in **jade** | a green profit of nothing |
| `/trade` market list, live | every market `+0.00` in **jade** | 16 markets that have never traded, all "up" |
| `/trade` ticket, signed out | `Available margin 0.00` | unknown, while `Account equity —` two rows below is honest |
| `/trade` ticket | `Taker fee  −0.00` | negative zero |
| `/trade` blotter, market order | `Value 0.00` | while `Price` in the same row correctly shows `—` |

The design system's redundant encoding for direction is the sign (§3: *"the `+`
is rendered explicitly, never implied"*). A zero has no direction, so it should
carry neither a sign nor a hue. The product already knows how to say
"unknown" — the em dash is used correctly in a dozen places. It just is not used
consistently, and two conventions appear inside single panels.

**Screenshots:** `app-38-portfolio-LIVE-1920.png`,
`app-28-account-LIVE-1920.png`, `app-22-trade-LIVE-empty-1920.png`

---

## 21. A missing oracle reads as the freshest possible oracle — *defect*

**Where:** `/admin` markets table, Oracle age column.
**Screenshot:** `app-29-admin-LIVE-1920.png`

Every one of sixteen markets shows **"just now"**. `GET /admin/markets` returns
`oracle_age_blocks: null` and `oracle_fresh: false` for the perpetuals. A null
age is rendered as zero, and zero renders as "just now" — the most reassuring
value in the column, on the one screen where an operator checks whether prices
can be trusted, and feeding the deviation check in finding 5.

§10.8 specifies a `badge warn` for a stale oracle age. Null should be *no oracle
price* in that badge, never a duration.

---

## 22. The one tile allowed to shout, shouts at zero — *defect/taste*

**Where:** `/admin` overview.
**Screenshot:** `app-29-admin-LIVE-1920.png`

`Accounts below maintenance` = **0**, and the tile still carries the brass wash
and the warning triangle. §10.8 designs this tile to be the only one that
escalates, precisely so that its escalation means something; a permanently
alarmed tile trains the operator to ignore it. The specified **Open the user
list** route out of the tile is also missing entirely.

Flat at zero, brass above zero.

---

## 23. Component health is transposed and hard to scan — *defect*

**Where:** `/admin` → Component health.
**Screenshot:** `app-29-admin-LIVE-1920.png`

The row renders as `✓ OK    node height 2`, `✓ OK    database schema 1`,
`✓ OK    bus —`, `✓ OK    feed —`: the status precedes the component name, so it
reads "OK node", and the gaps between the four groups are large and uneven. The
reference renders `Node ✓ OK | height 184,203`, which scans as a list of
components. §10.8's stated requirement for this panel is that it answer "is
anything wrong" *from across a room*; transposed labels and ragged spacing are
exactly what stops that working.

---

## 24. Uptime is rendered as a clock — *defect*

**Where:** `/admin` overview header, `/chain` hero.
**Screenshots:** `app-29-admin-LIVE-1920.png`, `app-40-chain-LIVE.png`

`node up 09:38` and `Up 08:49 on a single sequencer`. The node had been running
nine minutes. `09:38` reads as either a time of day or nine and a half hours;
the reference says `node up 23h 20m`. On an admin overview, "how long has this
been up" is a question the operator is actually asking.

---

## 25. Five tables have no empty state — *defect*

The blotter **Ledger** tab, the **admin user table**, the **admin audit trail**,
the **account activity feed** and the **account sessions** table all render a
column header row above blank space with no glyph, no heading, no explanation
and no action. §8 specifies the anatomy and the product implements it well
elsewhere (`No orders yet`, `No open positions`, `No grants yet` are all good).
These five were missed.

**Screenshots:** `app-07-blotter-ledger-empty.png`,
`app-29-admin-LIVE-1920.png`, `app-30-admin-LIVE-lower.png`,
`app-28-account-LIVE-1920.png`

---

## 26. Signed out, the faucet insists you are on a cooldown of zero — *defect*

**Where:** `/faucet`, signed out.
**Screenshot:** `app-12-faucet-mock-signedout.png`

With nobody signed in, the page shows an **On cooldown** brass badge, a
**00:00 left** chip, a full brass ring reading **00:00 / no wait**, and a
disabled button hinted *"The wait is the same for everybody, bots included.
00:00 left."*

Nothing has been asked of the gateway, so nothing is known — and the panel
asserts a cooldown, then contradicts itself twice in one glance ("On cooldown"
beside "no wait"; "00:00 left" beside a disabled button). §10.4 states the rule
this breaks by name: **"The button never lies about being available."** The
caption under the ring even promises the ring "is drawn from
`next_available_at` and from nothing else, so it cannot disagree with the
button" — and it does.

The grant list is also absent, so a visitor cannot see what they would get,
which is the one thing this screen is for.

Worth saying plainly: **signed in, this screen is one of the best in the
product.** See "What is good" below.

---

## 27. The confirm step does not name the word you actually typed — *defect*

**Where:** `/`, Create a wallet → Confirm three of the words.

I typed the correct words into 03 and 11, and into **Word 07** entered word
**08** of the same phrase, the exact mistake a person makes when they skip a
line copying to paper.

Word 03 and Word 11 took jade ticks. Word 07 showed **nothing at all**: no
error, no glyph, no border change, no message. The submit button simply stayed
disabled.

§10.1 specifies the opposite, and explains why: *"A wrong answer names the word
actually typed (that is word 08 — check for a skipped word) so the person knows
what to look for."* Catching an ordering mistake in the minute the words are
still on the desk is the entire reason words 03, 07 and 11 were chosen — three
ordinals spread across three rows. The check runs; it just will not say what it
found, so the person is left staring at a field that looks the same as an empty
one.

The related escalation is also absent: §10.1 says *"after three attempts the
words are shown again"*. There is no attempt counter, because there is no
submit to fail — the button never enables. A stuck user does have **Show the
phrase again**, so the recovery exists; only the guidance toward it is missing.

**What I would change.** On a word that is in the phrase at a different
position, say so and say which: *that is word 08 — check for a skipped line.* On
a word that is not in the phrase at all, say that instead. Both are one lookup
against a list the client is already holding.

---

## 28. The top-bar equity readout is clipped and unaligned — *defect*

**Where:** every screen, every theme, every density.
**Screenshot:** `app-02-topbar-mock-1920-dark.png`

The `Equity` label sits high in the bar and its value is cut off by the bar's
top edge, so what renders is the word "Equity" with a sliver of a dash under it.
It is not baseline-aligned with its neighbours (Live, Compact, Light, the user
pill), and it gets worse in compact density. It appears in every full-screen
screenshot in this review.

Signed out it also shows `Equity —`, which is noise; the reference omits the
readout entirely when there is no account. The signed-out ticket
(`app-04-ticket-mock-dark.png`) and the signed-out portfolio
(`app-11-portfolio-mock-signedout.png`) show the same readout in context.

---

## 29. The simulation panel cannot choose the bots — *defect*

**Where:** `/admin` → Simulation.
**Screenshot:** `app-30-admin-LIVE-lower.png`

The six-value scenario segmented control is present and correct. The **bot
counts** §10.8 specifies as numeric steppers are not — there is a prose line
reading "0 bots." and a **Start the bots** button, and no way to say how many of
what.

This matters more than it looks. Bots are the only counterparty this chain has,
and without them no perpetual has a mark price (finding 28), which means nothing
can be traded. The one control that could unblock the product is missing.

`Invariant violations 0` is correctly rendered flat, per §10.8.

---

## 30. The blotter's last column is clipped at 1920 — *defect*

**Where:** `/trade` blotter, with rows present.
**Screenshot:** `app-21-trade-live-empty-1920.png`

At the widest supported width, the rejection reason on the TSLA-PERP row reads
`Market h` and the Cancel control on the ETH-PERP row reads `Cance`, both cut by
the panel edge, with a horizontal scrollbar underneath. The rejection reason is
the most important cell in a rejected row and it is the one being truncated.

---

# Low, and matters of taste

**31. Brass as text on light, in the settle dialog — *defect, small*.** §3 says
*"Brass on light is a fill, never text"*, and §10.8 names this element: *"The
same rule governs the settle dialog's deviation figure."* In the light theme the
deviation reads `+0.00%` in `rgb(138, 98, 0)` = `--brass`, measured.
`app-37-admin-settle-light.png`

**32. Jade used as chrome for things that are not direction — *defect in the
design system, faithfully copied*.** `badge live` (jade) carries *Order* (a
transaction kind), *Synced*, *Checksum valid*, *Available*, *Paying out* and
*Trading*. §2 rule 1 says *"nothing else in the product is ever green"* and
allows only *a live market*. I checked `reference.html`: it does the same for
*Order* and *Synced*. So this is the specification disagreeing with its own
normative implementation, not an engineering slip — but somebody should decide
which one wins. `app-14-chain-mock.png`, `app-17-paste-phrase-valid.png`

**33. The partial-fill bar is jade on a sell — *same category*.**
`.fillbar::after { background: var(--bid) }` in both `app.css:709` and
`reference.html:1133`, while §9 says *"a jade or vermilion bar"*. A 100%-filled
**Sell** row shows a green bar. `app-21-trade-live-empty-1920.png`

**34. The HANDLE_REQUIRED banner restates its own title.** *"This address has
not signed in here before"* / *"This address has not signed in here before, so
it needs a handle."* The body should say what a handle is for.
`app-19-handle-required.png`

**35. Disabled buttons that hide their reason.** *Cancel all* in the blotter and
*Halt the market* in the halt dialog are disabled with no hint and no title
attribute. §8: *"A disabled trading button never hides its reason."* The order
ticket gets this right everywhere, which makes the omissions conspicuous.

**36. The settle button does not look disabled.** It keeps a full-contrast
vermilion border and label while `disabled === true`. On the most dangerous
control in the product, "looks pressable, isn't" is the wrong way round.
`app-34-admin-settle-dialog.png`

**37. The order book does not fill its panel — *taste*.** With nine levels a
side, the book occupies the top 45% of the tallest panel on the screen and the
rest is black, with the spread — the row your eye goes to — at 30% height rather
than centred. `app-03-orderbook-mock-dark.png`

**38. The mock leaderboard fixture is not sorted — *fixture defect*.** Ranks 8,
9, 10 run −8,598 / −27,959 / −81,780, then rank 11 is −8,073. The page header
says "Ranked by profit and loss". The client is right not to re-rank (§10.5),
but the fixture makes the screen look broken in every demo.
`app-13-leaderboard-mock.png`

**39. The faucet contradicts its own claim count.** *Claims made: 1* in the left
panel, *2 claims* in the Grant history badge — the badge counts ledger entries
(one per asset), not claims. `/user/profile` agrees with the 1.
`app-24-faucet-LIVE-claimed-cooldown.png`

**40. The leaderboard credits 6,006.00 of profit to an account with zero
trades.** `Trades 0`, `Volume 0.00`, `Profit and loss +6,006.00`. The figure
comes from the gateway double-counting a locked balance, but P&L and trade count
contradicting each other on one row is visible to any reader.
`app-39-leaderboard-LIVE.png`

**41. `Mark 0.000000` beside `Spread —`, `Best bid —`, `Best ask —`.** Three of
five strip figures say unknown, two say zero, for the same absent data.
`app-22-trade-LIVE-empty-1920.png`

**42. Market-strip content differs from the reference — *taste*.** The
implementation shows Mark / Spread / Best bid / Best ask / Funding; the reference
shows Mark / Index / 24h high / 24h low / 24h volume / Open interest / Funding.
Best bid and best ask duplicate the top of the book two panels away, and at 1920
there is room for the figures that are not shown anywhere else. I would take the
reference's set.

**43. Post only stays available on a market order — *defect, small*.** A
post-only market order is a contradiction that the gateway would reject; the
toggle stays live when Market is selected.
`app-05-ticket-market-qty.png`

---

# What is good, and why

Several of these are good enough to be worth protecting during the fixes above.

**The account page's two-address panel is the best screen in the product.**
`app-28-account-LIVE-1920.png`, against `ref-03-account-1920-dark.png`. It does
land in one glance, which is what the brief asked. Two cards with a labelled
arrow reading *computed from the one on the left*; each card leading with **You
hold the key** / **The gateway holds the key** at body size *above* the hex, so
the difference between them is read before either address is; a bone spine on
yours and a brass spine on the system's; both addresses in full, unlike
everywhere else in the product, so they can be compared against a wallet. And
the reassurance is the first clause of the note — *"Your balance sits at the
trading address, and nothing has been moved"* — with the mechanism second, so a
frightened person can stop reading after nine words. It admits the custodial
trade-off and points at §2 of the contract rather than hiding it. I would change
nothing here.

**The faucet, signed in, is exact.** `app-23-faucet-LIVE-available.png` →
`app-24-faucet-LIVE-claimed-cooldown.png`. Available: the grant list at display
size, one primary button, a *Ready / no wait* ring. Claimed: the panel retitles
itself *Next grant*, the badge flips to brass *On cooldown*, the ring drains
against a real unlock time, the grant list dims behind it, and the button
disables with its reason on the control. The verb survives the flow —
**Claim grant** → toast **Grant claimed** — exactly as §11 asks.

**The twelve-word screen is the other screen I would not change.**
§10.1 calls this the hardest screen in the product and reasons about it in more
detail than anything else in the design document; the implementation carries
every one of those decisions. The words are covered when the modal opens,
behind a panel that explains what is behind it and asks the person to check
nobody is watching. The grid is three across and four
down, read **row-major**, so the numbering and the natural reading order agree.
The ordinals are zero-padded, tabular and in their own gutter, so each word is
clearly paired with its position. Ledger hairlines run under every row and
between every column. **The first four letters of each word carry full contrast
and the tail is dimmed** — the subtlest instruction in the whole document, and it is there,
telling the reader exactly which characters must be right. The re-cover timer is
visible and counting (*Hiding again in 01:28*) with **Hide now** beside it, so
the re-cover is never a surprise. **Copy for a password manager** is secondary
and names its destination, the clipboard caveat is stated, and there is no
download. There is no other field on the screen — no handle, no checkbox. And at
390 the grid becomes two columns and six rows, still row-major, still one line
per ordinal-and-word, exactly as §10.1 asks. Finding 27 is the only thing
between this flow and being exemplary end to end.

**The socket-*close* handling on the terminal is correct, and I checked it
properly** — the gap is the silent stall in finding 6, not this.
`app-10-trade-socket-dropped.png`. On drop, the indicator turns
brass and reads *Reconnecting, attempt 3* with a timer — a dot *and* a word
*and* a figure. The book stays on screen, visibly frozen; nothing is hidden. I
measured the computed colours during the outage: the market strip's headline
price gains a `frozen` class and drops to `--text-muted` (#9C9286), as do the
book and the tape. The toast explains what is happening without apologising:
*"The feed will reconnect on its own. Prices hold their last value and stop
flashing until it does."* This is the answer to "is it ever ambiguous whether a
price is live or stale", and here it is not.

**The sign-in flow, up to the handle step, is excellent.**
`app-17-paste-phrase-valid.png`,
`app-18-signature-step.png`. One field rather than twelve boxes, masked by
default with a Show control, a live *n of 12 words* count, a *Checksum valid*
badge, and the derived address shown before anything is signed — with the
account-index control beside it and the hint to leave it alone. It derived
`0x9858EfFD232B4033E47d90003D41EC34EcaEda94` from the canonical phrase, matching
the published vector. The signature step shows the gateway's `message` verbatim
in the sunken surface and states what the signature does not do. Finding 2 is
one step away from an otherwise exemplary flow.

**The force-settle dialog's structure is right**, once finding 5 is fixed.
`app-35-admin-settle-priced.png`. Consequences before controls; status, open
interest, resting orders and the mark; the price **in words** (*32 thousand*);
double entry into two fields that never show each other, with the confirmation
explaining *why* ("a slipped digit does not survive being typed twice"); and the
button carrying the number — **Settle BTC-261006 at 32,000.00**. The halt dialog
is built the same way and requires a typed reason
(`app-33-admin-halt-dialog.png`).

**The explanatory notes under the admin tables are the best writing in the
product.** *"Halting stops new orders and leaves the resting book alone;
cancelling stays allowed, because trapping somebody's order with no way out
would be worse than whatever caused the halt. Settle stays visible and disabled
on a trading market… an operator hunting for a missing button under pressure is
how the wrong button gets pressed."* That is the design document's reasoning
rendered as interface copy, and it is why the disabled-Settle-everywhere pattern
reads as deliberate rather than broken.

**The landing page.** *"The order book is real.
The money is not."* is a real headline. Two paths and no form; no handle field
asked of returning users; the BIP-39 interoperability claim made once, in one
paragraph, early; the third figure corrected to *0 personal data*; a footer that
says plainly that nothing settles into a real asset. The tape-rule divider under
the paragraph is the identity doing work rather than decorating.

**The leaderboard.** `app-13-leaderboard-mock.png`. Rank-one brass chip, bots
badged in signal blue with their strategy named, the viewer's own row in a brass
wash with a *You* badge, and the share-of-window bar anchored left for gains and
right for losses so the sign is carried by position as well as by colour. The
note under the table explains that hiding bots filters without renumbering.

**Accessibility of the terminal is better than it looks.** The order-ticket
segmented controls are real `group`s with names ("Side", "Order type", "Margin
mode", "Size as a share of available margin"); the steppers are labelled
"Increase size by one lot"; the quantity and limit-price fields have accessible
names. Order-book levels are `<button>` elements, so the book is tabbable and
the focus ring lands on the level, exactly as §8 requires.

**Depth bars, numerals and density are faithful to the system.** I measured
rather than eyeballed: `.ob-row::before` uses `inset: 0 0 0 auto` with
`border-radius: 2px 0 0 2px`, matching `reference.html` — anchored to the outer
edge, growing inward. Row height is 22px at comfortable and the `mine` brass
notch renders on a real resting order (`app-27-book-LIVE-skeleton-asks.png`).
Every price I sampled carries `font-variant-numeric: tabular-nums` and the
monospace stack. Light theme and compact density both hold up at 1920
(`app-08-trade-mock-1920-light-compact.png`).

**No horizontal page overflow.** At 390 I measured
`document.documentElement.scrollWidth === innerWidth` on the terminal and on the
admin console; the wide tables scroll inside their own wrappers. The phone
terminal keeps the persistent Buy / Sell bar above the rail and the market
picker in the strip, both as §10.2 specifies.

**Numbers do not jitter, as far as I checked.** I did not film the screen; what
I did was sample computed styles and compare column positions across screenshots
of the same panels taken minutes apart while the mock feed ticked. Every price
cell I sampled carries `font-variant-numeric: tabular-nums`, decimal points stay
in column between samples, and the tab labels are insulated from their counts by
the count pill. The price tick is a decaying tint rather than a strobe. I saw no
reflow, but this was not a frame-by-frame test.

---

# Coverage, and what I did not do

Stated exactly, because a review is only worth what it actually looked at.

**Widths, themes and densities.** Trading terminal: 1920 dark comfortable, 1920
light compact, 390 dark comfortable. Admin console: 1920 dark, 1920 light (the
settle dialog specifically), 390 dark. The other six screens: 1920 dark
comfortable plus 390, with one light or compact variant each. I did **not** run
all eight theme × density × width combinations on any screen.

**Backends.** Both, on every screen. Mock signed out; live signed in as `admin`
with a real session, a real faucet claim and a real resting order.

**Disconnection.** Both kinds. The mock **Drop the socket** control (a clean
close, handled well) and a real stalled server via `kill -STOP` on the gateway
for 110 seconds, which produced finding 6.

**Not covered:**

- A real fill, a real position, and therefore the whole liquidation escalation
  ladder — the proximity meter, the `high` banner, the `critical` hazard
  treatment in the market strip, and the liquidation toast. Finding 18 blocks
  them; nothing can fill on a fresh chain. This is the largest untested area of
  the design system (§9) and it needs a seeded chain with bots running.
- 1440, 1280 and 834. The container queries for those bands exist at
  `app.css:622–651` and their breakpoints match §10.2, but I only exercised 1920
  and 390.
- The settle action itself, and suspend/unsuspend. I opened and validated the
  dialogs but did not execute an irreversible settle.
- `prefers-reduced-motion`, and keyboard-only traversal of a full screen.

**Console errors.** Zero across the eight screens on the mock backend, with one
caveat: `/admin` is not reachable on mock (no admin role), so the mock figure
covers seven screens, not eight. On live the only errors are the four documented
above — two `400`s from malformed query strings, the CORS rejection and its
`net::ERR_FAILED` partner — plus the expected `422` during sign-in.

---

# Verdict by screen

| Screen | State | Blocking work |
|---|---|---|
| **Account** | **Ready.** The two-address panel answers the brief's question in one glance and I would change nothing in it | 11, 19 — the activity feed and the sessions list are both silently empty |
| **Faucet** | **Ready signed in.** The claim → cooldown cycle is exact | 26 (the signed-out state asserts a cooldown that does not exist), 17 |
| **Leaderboard** | **Ready** | 38 is a fixture fix, 40 is upstream of the client |
| **Sign-in** | **Close.** The wallet-creation flow, including the twelve words, is the best-executed thing in the product | 2 is a hard blocker at the last step; 27 is one lookup |
| **Trading terminal** | Not ready | 7, 12, 18 — the ticket must cost the order, empty must not look like loading, and a first visitor must be able to trade |
| **Portfolio** | Not ready | 13, 17 — the hero is blank above data the same page is displaying |
| **Chain explorer** | Not ready | 10 — it prints fabricated numbers under a note promising it does not |
| **Admin console** | Not ready | 4, 5, 8, 9, 14, 15 — destructive actions fail silently, the settle safety check inverts, and the risk dialog's baseline figures are wrong |
| **Every screen at 390** | Not ready | 1 — there is no navigation, and neither preference can be reached |
| **Every screen, live feed** | Not ready | 6 — the top bar reports Live indefinitely against a stalled gateway |

## If only five things are fixed

1. **A way to navigate on a phone** (1) — without it, five of eight screens do not exist on the device most people will open the link on.
2. **The handle retry at sign-in** (2) — it is the last step of an otherwise excellent flow, and it currently tells a new user their account failed at the moment it succeeded.
3. **Cost in the order ticket** (7) — a trading interface that says a 6,000-dollar order is worth 0.00 cannot be shipped, and the number already exists in the client.
4. **The settle deviation check and the risk dialog's silent failure** (5 and 4) — these are the two places where the admin console is less safe than it looks, which is worse than being visibly unsafe.
5. **One empty-state pattern applied everywhere** (12, 25, and the dashes/zeros of 20) — the product already writes excellent empty states; the fresh-chain experience is mostly a matter of using them in the five places they were missed.
