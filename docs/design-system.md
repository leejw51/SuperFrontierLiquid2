# SuperFrontierLiquid Design System — "Lacquer & Neon"

**Status: v1.** The normative artefacts are `docs/design/tokens.css` (the tokens,
importable by `cb-web`) and `docs/design/reference.html` (a working
implementation of every token, component and screen, with both themes and both
densities live). This document explains the decisions behind them. Where this
document and `tokens.css` disagree, `tokens.css` wins and this document is the
bug.

Every screen in this system is designed against `docs/api-contract.md` and
nothing else. No state described here is inferred from a field the gateway does
not send.

---

## 1. The tension, and how it is resolved

SuperFrontierLiquid is two products wearing one skin.

It is a **professional instrument**: dense, fast, precise, meant to be read at a
glance by someone who already knows what a book is. It is also a **game a
stranger lands on** from a link, with no idea what a perpetual is, who must
understand the thing within one screen or leave.

Those pull in opposite directions. Resolving them by averaging — a
medium-density terminal with friendly tooltips — produces something that is bad
at both. So they are resolved by **separation of labour**:

1. **The landing screen does all of the teaching.** It explains the product,
   shows the real components at real density, and gets the user through a
   BIP-39 mnemonic and an EIP-191 signature into the terminal. Everything a
   newcomer needs to understand is spent there.
2. **The terminal never apologises for being dense.** It has no onboarding
   overlays, no coach marks, no beginner mode. It is the instrument.
3. **Density carries the ramp.** `comfortable` is the default, because a
   beginner arrives before a professional does: 32px rows, 13px text, generous
   hit targets. `compact` is one switch in the top bar, remembered per user:
   22px rows, 12px text, roughly 50% more rows on screen. Same components, same
   markup, different token values.
4. **Where a number needs explaining, the explanation is a tooltip on the
   derivation, not a definition of the word.** The tooltip on *margin ratio*
   shows used margin, equity, the ratio and the liquidation threshold. It does
   not say "margin ratio is a measure of risk".

The one place the instrument is allowed to shout is money about to be lost. See
§9.

---

## 2. Identity

The name is the brief. Causeway Bay is dense, vertical, commercial and lit at
night — so the palette is named after **materials from a shop front at night**,
not after moods:

| Material | Token | Role |
|---|---|---|
| Black lacquer | `--surface-*` | Every surface. Warm graphite, hue ≈ 25°, not blue-black. |
| Bone | `--text-primary`, `--brand-ink` | Type, the wordmark, the primary call to action. |
| Jade | `--bid` | Bid, buy, long, profit, up. |
| Vermilion | `--ask` | Ask, sell, short, loss, down. |
| Brass | `--brass` | Warning, cooldown, stale, halted, rank one. |

A sixth, **signal blue** (`--signal`), carries "pending on chain" and the chain
rail. It is deliberately the coldest thing in the palette because it always
means *not yet settled*.

### The four rules the identity is actually made of

1. **Market colour never touches chrome, and chrome colour never encodes
   direction.** Jade and vermilion only ever mean up and down. Buttons, links,
   focus rings, selected tabs, the wordmark and the logo are bone and graphite.
   Consequence: a green button in this product always means *buy*, and nothing
   else in the product is ever green.
2. **Radius encodes interactivity.** Panels, tables and the chart are `0`.
   Anything clickable is `4`. Anything floating above the page is `6`. Anything
   round is a state, not a container. You can tell what is clickable with the
   colour switched off.
3. **Structure over decoration.** The recognisable devices are the **spine**
   (a 2px bar on the leading edge of every panel header, coloured by the system
   that owns the panel), the **tape rule** (a hairline with tick marks, used as
   a section divider, which reads as a price axis), the **one-pixel canvas seam**
   between flush panels, and the **chain rail**.
4. **The chain has a rail, not a page.** Block height, throughput and the last
   twelve blocks occupy 40px along the bottom of every screen. The blockchain
   underneath is a continuous fact, not a tab you have to find.

### Deliberately rejected

| Rejected | Why |
|---|---|
| Blue-black canvas | The default surface of every exchange front end. Warm graphite gives jade and vermilion somewhere of their own to sit and reads as lacquer, not as a screen. |
| A saturated brand accent | In a trading UI the loudest colours must belong to the market. The brand speaks through form. |
| A purple or gradient anything | The templated look we are explicitly not shipping. There is no gradient in this system except the depth-bar fill and one 22%-opacity area wash under the equity curve. |
| A fifth hue for liquidation danger | A second red separates from the first by only ΔE 5.0 under protanopia. Liquidation danger is vermilion **plus** a 135° hazard texture **plus** the word. |
| Cards with shadows | A terminal is a window manager, not a feed. Panels are flush, square and separated by a one-pixel seam. Shadow is reserved for things that genuinely float. |

---

## 3. Colour

### Surface hierarchy — six steps

| Token | Dark | Light | Use |
|---|---|---|---|
| `--surface-canvas` | `#0E0C0B` | `#EFEAE2` | The page, and the 1px seam between panels |
| `--surface-panel` | `#171412` | `#FBF9F5` | Every terminal panel, the top bar, the rail |
| `--surface-raised` | `#1F1B18` | `#FFFFFF` | Sticky table headers, inputs, inactive tabs |
| `--surface-overlay` | `#272320` | `#FFFFFF` | Modals, dropdowns, tooltips, toasts |
| `--surface-hover` | `#302B27` | `#EBE5DB` | Row hover, pressed controls, active segment |
| `--surface-sunken` | `#0A0908` | `#F5F1EA` | Chart plot area, hashes, the code box |

Borders are three weights and no more: `--border-subtle` (row rules inside a
panel), `--border-default` (panel edge, input edge), `--border-strong` (hover,
focused container). The fourth line in the system is the 2px spine.

### Semantic roles

| Role | Token | Dark | Light | Never used for |
|---|---|---|---|---|
| Bid · buy · long · profit · up | `--bid` | `#34E0B4` | `#00705B` | Anything that is not direction |
| Ask · sell · short · loss · down | `--ask` | `#FF7053` | `#BC3318` | Anything that is not direction |
| Liquidation danger | `--ask` + `--hazard` | — | — | Ordinary losses |
| Warning · stale · halted · cooldown · rank 1 | `--brass` | `#FFD166` | `#8A6200` | Direction, or body text on light |
| Pending on chain · information | `--signal` | `#78B4E0` | `#15679E` | Settled or confirmed things |
| Neutral · cancelled · expired | `--text-muted` | `#9C9286` | `#665D53` | — |
| Brand · primary action · focus | `--brand-ink` | `#F4EFE7` | `#17130F` | Direction |

`--profit` and `--loss` are aliases of `--bid` and `--ask`. Use them in P&L code
so intent reads clearly at the call site.

**Brass on light is a fill, never text.** At AA contrast on paper, every ochre
collapses toward vermilion (ΔE 12.5 normal vision, 2.1 protan). So in the light
theme a warning is a `--brass-wash` panel with a `--brass` left spine and
**ink-coloured words**. The dark theme may use brass as text; it separates from
vermilion by ΔE 16.3 under deuteranopia there.

### Contrast

Every text token was measured against canvas, panel, raised **and hover** in
both themes; the lowest ratio in that set is what is quoted. All clear WCAG AA
for normal text (4.5:1), which is the bar even at 11px.

| Token | Dark (worst of four surfaces) | Light (worst of four surfaces) |
|---|---|---|
| `--text-primary` | 12.2:1 | 14.0:1 |
| `--text-secondary` | 6.5:1 | 6.9:1 |
| `--text-muted` | 4.6:1 | 5.2:1 |
| `--bid` | 8.3:1 | 4.8:1 |
| `--ask` | 5.1:1 | 4.6:1 |
| `--brass` | 9.7:1 | fill only |
| `--signal` | 6.3:1 | 4.8:1 |

`--text-faint` is **non-text only** — rules, glyph strokes, disabled
iconography. It reaches only 2.9:1 dark and 3.1:1 light on a hovered surface,
which is fine for a 1px rule and disqualifying for a word. It must never carry
one.

### Colour vision

Jade and vermilion separate by **ΔE 32.2** for normal vision, **12.7** under
deuteranopia and **11.9** under protanopia, measured in OKLab ×100 with the
validator in the project's dataviz tooling. That clears the threshold on its
own. Colour is still never the sole carrier of meaning:

| Where direction appears | Required redundant encoding |
|---|---|
| Order book | **Position.** Asks above the spread, bids below, at every breakpoint including the phone. |
| Profit and loss | **Sign.** The `+` is rendered explicitly, never implied. |
| Positions | **Word.** A `Long` / `Short` badge, read before the colour is. |
| Price change, candles | **Shape.** A ▲ / ▼ glyph; down candles hollow, up candles filled. |
| Liquidation danger | **Texture and word.** 135° hazard stripes plus the literal word *Liquidation*. |
| Depth bar | **Direction of growth.** The fill always grows from the outer edge inward toward the spread. |
| Leaderboard bars | **Anchor.** Gains grow from the left, losses from the right. |

### Price convention

Jade rises and vermilion falls. Markets across Greater China read the opposite
way, and the switch is a two-variable swap rather than a redesign: a
`[data-convention="cjk"]` block that reassigns `--bid` to the vermilion ramp and
`--ask` to the jade ramp flips the entire product, because **no component is
allowed to hard-code either hex value**. That constraint is the reason the swap
is cheap, and it is worth keeping even if the mode is never shipped.

---

## 4. Type

Two roles. The v1 system shipped them as system fonts with no web font at all;
the 16-bit skin (§13) replaces both stacks with two bundled pixel faces and
keeps the two-role rule. Everything below about *which words wear which role*
still holds.

| Token | Stack | Rule |
|---|---|---|
| `--font-ui` | `ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, …` | All prose, labels, buttons, headings |
| `--font-num` | `ui-monospace, "SF Mono", Menlo, Consolas, "Cascadia Mono", …` | **Digits, hashes, symbols and identifiers only.** Never prose, never labels. |

Reserving the monospace face for numerals converts what is usually a
generated-page tell — mono for small captions — into the one rule that makes
prices legible.

### Scale

| Token | px | Line height | Use |
|---|---|---|---|
| `--fs-micro` | 10 | 1.28 | Block chips, cumulative depth, unit suffixes |
| `--fs-xs` | 11 | 1.28 | Column headers, badges, timestamps, hints |
| `--fs-sm` | 12 | 1.5 | Dense table body, compact rows, panel titles |
| `--fs-base` | 13 | 1.5 | Interface default |
| `--fs-md` | 15 | 1.28 | Panel titles, emphasised body |
| `--fs-lg` | 18 | 1.28 | Market symbol in the strip |
| `--fs-xl` | 22 | 1.28 | Screen headings, secondary stats |
| `--fs-2xl` | 28 | 1.0 | The live price in the market strip |
| `--fs-3xl` | 38 | 1.12 | Portfolio equity, landing price |
| `--fs-4xl` | 52 | 1.12 | Landing headline |
| `--fs-5xl` | 76 | 1.12 | Reserved; display specimens only |

Weights: 400 / 500 / 600 / 700. Tracking: `-0.022em` above 38px,
`-0.012em` from 18–28px, `0` for body.

Prose is capped at `--measure` (66ch). Serif is not used anywhere in this
system.

### Numerals

Everything that can change wears `--font-num` with
`font-variant-numeric: tabular-nums` and a slashed zero. Prices are
right-aligned in every table so the decimal point forms a column. **Trailing
zeros are never trimmed** — the gateway sends `"64250.50"` and the UI shows
`64,250.50`, because a price that drops a digit reads as a different price.

| Quantity | Rule | Example |
|---|---|---|
| Price | `price_decimals` from `/markets`, thousands separated | `64,250.50` |
| Size | `size_decimals` from `/markets` | `1.2500` |
| Profit and loss | Explicit sign, two decimals, coloured | `+1,872.15` |
| Percentage | Explicit sign, two decimals | `−3.42%` |
| Funding rate | Four decimals, per interval | `0.0125%` |
| Address, hash | First six and last four, copy on click | `0x8f3b…c21d` |
| Block height | Thousands separated, never abbreviated | `184,203` |
| Large notional | Abbreviated above a million, one decimal | `128.4M` |

### Capitals

Letterspaced capitals are banned as decoration. They occur in exactly one place:
instrument tags of four characters or fewer — `PERP`, `SPOT`, `GTC`, `IOC` —
where they are the market's own notation rather than a label we invented.
Everything else is sentence case, including buttons and column headers.

---

## 5. Space, radius, border, elevation

**Spacing** is a 2px scale below 16px and an 8px scale above it, because a
terminal spends most of its budget between 2 and 12 pixels:
`2 · 4 · 6 · 8 · 10 · 12 · 16 · 20 · 24 · 32 · 40 · 48 · 64 · 80 · 96 · 128`.
`--gutter` is `1px`: the seam of canvas between flush panels.

**Radius** encodes interactivity, as above: `--r-0` panels, `--r-2` depth bars
and inline chips, `--r-4` controls, `--r-6` overlays, `--r-full` pills and dots.

**Elevation.** Dark interfaces lift by catching light along a top edge, not by
casting shadow into black. Panels do not lift at all.

| Token | What it is | Used by |
|---|---|---|
| `--elev-0` | nothing | Panels, tables, the chart |
| `--elev-1` | inset top highlight | Inputs, sticky table headers |
| `--elev-2` | 4px blur + hairline ring | Dropdowns, tooltips, toasts |
| `--elev-3` | 24px blur + hairline ring | Modals only, over `--scrim` |
| `--elev-live` | an 18px glow in `currentColor` | **Live data only.** A connected socket, a ticking price. Never decoration. In the light theme it resolves to nothing, because glow is a dark-theme device. |

---

## 6. Motion

Motion answers one question: what just changed, and which way. Nothing eases in
on scroll, nothing floats, nothing breathes for atmosphere.

| Token | Value | Used for |
|---|---|---|
| `--dur-fast` | 120ms | Hover, focus, press, row highlight |
| `--dur-base` | 180ms | Tab change, dropdown, depth-bar width |
| `--dur-slow` | 280ms | Modal, drawer, toast entry, new block chip |
| `--dur-flash` | 400ms | The price tick — a tint that decays to nothing, so a stream of ticks reads as a rhythm rather than a strobe |
| `--dur-pulse` | 1600ms | Connection heartbeat; 700ms while reconnecting |
| `--ease-out` | `cubic-bezier(0.16, 1, 0.30, 1)` | Anything entering or responding |
| `--ease-in-out` | `cubic-bezier(0.65, 0, 0.35, 1)` | Anything moving between two held positions |
| `--ease-linear` | `linear` | Progress, cooldown countdowns, skeleton shimmer |

### Reduced motion

All five durations collapse to 1ms under `prefers-reduced-motion`. **The
information the motion carried does not disappear with it.** Every animated
component has a static substitute:

| Animated | Static substitute |
|---|---|
| Price tick flash | The tint is *held* rather than decayed, and the ▲/▼ glyph beside the price persists until the next tick |
| Connection heartbeat | Solid dot plus the word and the latency figure |
| Skeleton shimmer | A flat raised block; surrounding copy still names what is loading |
| New block sliding onto the rail | Chip appears in place; the height figure holds its tint |
| Button spinner | Static ring at 50% opacity; the label becomes *Sending…* |
| Toast slide-in | Appears in place; the queue keeps its full dwell time |

---

## 7. Density

Two modes, one switch in the top bar, remembered per user. Comfortable is the
default. Compact fits roughly half again as many rows into the same panel.

| Token | Comfortable | Compact | Applies to |
|---|---|---|---|
| `--row-h` | 32px | 22px | Orders, positions, fills, ledger, leaderboard, markets |
| `--row-h-tight` | 22px | 17px | Order-book levels, trade tape, block list |
| `--row-h-head` | 30px | 24px | Sticky table header |
| `--control-h` | 36px | 28px | Button, input, select, numeric stepper |
| `--control-h-sm` | 28px | 22px | In-table and in-toolbar controls |
| `--panel-head-h` | 40px | 30px | Panel header |
| `--tabbar-h` | 38px | 30px | Tab bar |
| `--topbar-h` | 52px | 44px | Application top bar |
| `--strip-h` | 64px | 52px | Market strip |
| `--rail-h` | 40px | 32px | Chain rail |
| `--cell-pad-x` | 12px | 8px | Table cell horizontal padding |
| `--density-fs` | 13px | 12px | Table body and control text |

**One scope may override the user's choice.** `[data-density-lock="comfortable"]`
re-declares the comfortable values locally. It is used on the destructive admin
confirmations — force-settle and the risk parameters — and on all four join
modals, where a mnemonic set at 12px would be the wrong thing to ask somebody to
copy onto paper. Compact exists to fit more rows on a screen; a confirmation and
a recovery phrase both want fewer things on a screen instead. It is the only
override of the user's density preference in the system, and it may not be
applied to a table.

**Row heights are fixed, never content-driven.** This is not an aesthetic
preference: the orders, fills and ledger tables are virtualised, and
virtualisation needs `top = index × var(--row-h)` to be true. A row that grows
to fit its content breaks the scroller. Anything that will not fit is truncated
with an ellipsis and available in the row's tooltip.

---

## 8. Components

Each of these is implemented in `docs/design/reference.html` in every variant
and state. The CSS there is production CSS; lift it.

### Button
Variants: `primary` (bone fill, ink text), `secondary` (raised fill, hairline),
`ghost`, `buy` (jade fill), `sell` (vermilion fill), `danger` (vermilion outline).
Sizes: `sm` 28px, default `--control-h`, `lg` 46px. States: rest, hover, pressed
(`translateY(1px)`), focus-visible (2px `--focus-ring` outline, 2px offset),
loading (`data-loading`: label hidden, 14px spinner, pointer-events off),
disabled. One primary action per region. Labels are sentence case, name what
will happen, and keep the same verb through the flow: *Place order* → *Order
placed*. No arrows appended to labels. **A disabled trading button never hides
its reason**; it sits beside a hint drawn from the same `details` object the
gateway returns, so the client-side guard and the server-side rejection say the
identical thing.

### Input
`--control-h` tall, `--r-4`, raised fill, hairline border that strengthens on
hover and becomes the focus ring plus a 3px halo on focus. `aria-invalid="true"`
turns the border vermilion and swaps the halo. A field is a label, the control,
and either a hint or an error — never both.

### Numeric input with stepper
Right-aligned, `--font-num`, tabular. The unit rides inside the field, pinned
right, in `--text-muted`. A 22px stepper column sits on the trailing edge with
up and down buttons split vertically. **Steps are the market's own
increments** — `tick_size` for price, `lot_size` for size, from `/markets` —
never `1`. Holding an arrow accelerates after 500ms. The hint line carries the
tick and the minimum notional.

### Select
Native `<select>`, appearance stripped, chevron drawn with a rotated border.
Same height, radius, fill and focus treatment as the input, so a row of mixed
controls aligns exactly.

### Segmented control
Used for side, order type, margin mode, chart interval and percentage presets.
2px padding, `--r-2` children inside a `--r-4` shell. The selected segment takes
`--surface-hover`; the **side** segmented control is the one exception where the
selected segment takes `--bid` or `--ask`, because side is the single most
consequential choice on the screen.

### Tab bar
The active tab is marked by a 2px bone underline **and** a weight change, never
colour alone. Live counts ride in a pill so the tab label itself never reflows
as numbers arrive. Horizontally scrollable with the scrollbar hidden.

### Table
Sticky header on `--surface-raised`. Numeric columns right-aligned and tabular;
text columns left. Row hover is `--surface-hover`. Row actions are hidden until
hover or keyboard focus, and **always visible under reduced motion**.

> **Virtualisation contract.** The scroll body is `height: rows × var(--row-h)`.
> Each rendered row is absolutely positioned at `top: index × var(--row-h)`,
> with a nine-row overscan either side of the viewport. Sorting and filtering
> operate on the whole set, not the rendered window. Tables over 200 rows
> virtualise; below that they do not need to.

### Order book row
Three equal columns: price, size, cumulative total. The depth bar is a
**cumulative** background fill — at each level it shows the total size resting
between that price and the top of the book, which is the number that actually
tells you what a market order would cost. It is a `linear-gradient`-free solid
fill sized by `width: calc(var(--depth) * 1%)`, anchored to the outer edge and
growing inward toward the spread, so the two sides converge and the shape of the
book reads as a silhouette. Scale is relative to the largest cumulative total
shown on either side, so the sides are directly comparable. Your own resting
order is a 2px brass notch in the left gutter — a mark, not a recolour, so it
works on both sides. Clicking a price fills the ticket's price field; clicking
the size fills price and size together. The aggregation pill in the header
offers 0.10 / 1 / 10 / 100 and aggregated levels keep exact cumulative totals.
A level that changes tints for `--dur-flash`. **Each level is a `<button>`,
not a div**, so the book is tabbable and the focus ring lands on the level.

### Position row
Fixed `--row-h`. Side is a `Long` / `Short` badge with a direction glyph, read
before the colour is. P&L always carries its sign and its percentage. When
`liquidation_risk` reaches `medium`, a 3px proximity meter appears beneath the
row showing how far `mark_price` has travelled from `entry_price` toward
`liquidation_price`; at `critical` the meter's track becomes hazard texture.

### Toast
340px, `--r-6`, `--elev-2`, with a 2px accent spine on the leading edge in the
semantic colour. Bottom-right, newest at the bottom, maximum four. Good and
informational toasts dwell 5s; anything the user must read stays until
dismissed. **Every trade toast names the block** — on this product that is the
receipt. A rejection toast carries a single-tap remedy where the error code
allows one. The `account` channel's `liquidation` message gets a toast of its
own that stays until dismissed: the symbol, the quantity, the price it closed
at, the loss, and the block — the game's most consequential event is never
allowed to scroll past.

### Modal
`--r-6`, `--elev-3`, over `--scrim`. Reserved for decisions with consequences:
closing a position at market, cancelling every resting order, raising leverage,
revealing a recovery phrase, force-settling a market and changing margin
requirements. The body shows the arithmetic — position, mark,
estimated fill, realised P&L, estimated fee — and the confirming button repeats
the verb and carries the number: *Close 1.5000 BTC*.

### Tooltip
`--r-6`, `--elev-2`, max 240px. Appears after 350ms of hover, immediately on
keyboard focus. It is for the **derivation** of a number, laid out as a
definition list with right-aligned tabular values — not a restatement of the
label, and never content that could have been shown in the row.

### Skeleton loader
Takes the exact shape of the content it replaces, at the current density's row
height, so nothing jumps when data lands. 1400ms linear shimmer; flat under
reduced motion.

### Connection-status indicator
Lives in the top bar, always. Four states — `live`, `reconnecting`, `stale`,
`down` — each a dot **and** a word **and** a figure. The bare dot is never the
whole message. Live pulses at `--dur-pulse` with `--elev-live`; reconnecting
pulses at 700ms in brass.

### Empty state
A faint glyph, a heading, one line of explanation, and an action. It is an
invitation: *No open orders — orders you place that do not fill immediately will
rest here until they do.* → **Place your first order**.

### Error state
Same anatomy, vermilion glyph. It says what happened and what to do, in the
interface's voice. It does not apologise and it is never vague: *Can't reach the
sequencer — the gateway answered but the node did not. Market data is frozen at
12:41:08.* → **Try again**.

---

## 9. The nine states

Each names the field it is derived from. Nothing here is inferred from a field
the frozen contract does not send.

| State | Derived from | Treatment |
|---|---|---|
| **Socket disconnected** | WebSocket `close`, or no `pong` within 90s | Indicator turns vermilion, reads *Offline* with the time it dropped. All prices dim to `--text-muted` and stop flashing. Submit disables with the reason inline. **Nothing is hidden** — the last known book stays on screen, visibly frozen. |
| **Reconnecting** | Backoff attempt counter | Indicator turns brass, heartbeat speeds to 700ms, label reads *Reconnecting · attempt 3*. On success the gateway sends a fresh snapshot on every channel, so the book is replaced wholesale rather than patched, and the panel briefly shows a *Resynced* badge. |
| **Stale price** | Time since the last `ticker` message > 5s, or a `422 ORACLE_STALE` on submit | The price keeps its last value — never blanked — and gains a brass rule beneath it plus an age counter. Order entry stays enabled, because the node may still accept, but the ticket shows a brass banner naming the age. |
| **Pending on chain** | `status: "pending"`, before `accepted_at` | The row appears immediately at 60% opacity with a signal-blue *Pending* badge and the truncated `tx_hash`. Cancel is disabled until acceptance, because there is nothing yet to cancel. |
| **Accepted** | `status: "open"` with `tx_hash` and `accepted_at` | Opacity returns to full, badge becomes *Open*, the row flashes its side tint once. The `height` from the fill appears in the toast as the receipt. |
| **Partial fill** | `filled_qty` against `qty` | The filled fraction is drawn as a jade or vermilion bar across the left of the size cell with the percentage in text beside it. A partially filled order is **never** rendered in the same style as an unfilled one. |
| **Rejected** | The non-2xx `error.code` | A toast whose title is the plain-language consequence and whose body is the gateway's own `message`, plus a single-tap remedy where the code allows one: reprice for `POST_ONLY_WOULD_CROSS`, open the faucet for `INSUFFICIENT_BALANCE`, reduce size for `INSUFFICIENT_MARGIN`. |
| **Approaching liquidation** | `liquidation_risk`, with `margin_ratio` and `mark_price` against `liquidation_price` | Four steps, below. |
| **Settling** | `status: "settling"` | The window between expiry and the settlement transaction landing. The book dims and stops being clickable, the market carries a signal-blue *Settling* badge, and open positions read *Settlement pending* in place of an unrealised figure — there is no longer a mark price to compute one from. |
| **Halted / expired / settled** | `markets[].status` | The book dims and stops being clickable, the ticket is replaced by an explanation, and the market keeps its place in the list marked *Halted* or *Settled*. A position in a settled market shows the settlement price and the realised amount, and the row is no longer live. |

### Liquidation escalation

| `liquidation_risk` | What changes |
|---|---|
| `low` | Nothing. The meter is not drawn at all. |
| `medium` | The proximity meter appears under the position row in brass. The portfolio's risk gauge moves off its rest state. |
| `high` | Meter turns vermilion. The liquidation price is shown in vermilion in the position row. A persistent banner appears at the top of the portfolio with the exact margin needed. |
| `critical` | The meter's track becomes hazard texture. The banner moves into the terminal's market strip so it is visible from any screen, with *Add margin* and *Reduce* one tap away. **This is the loudest the interface ever gets, and it is reserved for money about to be lost.** |

---

## 10. Screens

### 10.1 Landing and join

The hero is not an illustration of a market — it **is** a market: the production
order-book component, at production density, with the production data shape,
ticking. What a stranger sees first is the thing they are about to use.

- Headline, one paragraph of plain explanation, three figures (`100ms`, `17`,
  `0`), then **two paths and no form**: *Create a wallet* and *Paste a phrase*.
- There is no handle field on the landing page. A handle is still required on a
  first login, but the client only knows an account is being created on step
  three of the new-wallet flow, or after a `422 HANDLE_REQUIRED` on the paste
  path. Asking every returning user for something they already have is the
  wrong default.
- The third figure changed. It used to read *0 — cost, no wallet, no key
  management*, which is now false: there is a wallet and the key is the user's
  problem, exactly as it is in every other EVM product. It reads *0 personal
  data — no email and no password, a signature from your own key is the login*.
- The identity explanation is made once, early, under the paths: BIP-39 at
  `m/44'/60'/0'/0/0`, Ethereum-compatible addresses, and a phrase that is never
  sent. The copy asks for a disposable test wallet and warns against importing
  a recovery phrase associated with real assets.
- Three explainers below the fold are **real components** — a faucet grant row,
  a position row with its liquidation meter, and the chain rail.
- Footer states plainly that it is open source, independent, and that nothing
  settles into a real asset.
- No countdown, no scarcity, no waitlist.

#### The join flow

| Step | What the client does | What the gateway sees |
|---|---|---|
| Create a wallet | 128 bits of entropy → twelve BIP-39 words → key at `m/44'/60'/0'/0/0` | nothing |
| Write it down | Reveals the words, hides them again on a timer or on blur | nothing |
| Confirm | Checks words 03, 07 and 11; takes a handle | `POST /auth/challenge`, then `POST /auth/login` with the handle; `is_new: true` |
| Paste a phrase | Word count, then wordlist, then checksum, then derives and shows the address | nothing until the address is accepted |
| Sign in | Signs the gateway's `message` verbatim under EIP-191 | `POST /auth/challenge` → `POST /auth/login`; a handle only after `422 HANDLE_REQUIRED` |

#### The twelve words

This is the hardest screen in the product. Two failure modes run at once: lose
the phrase and the account is gone; let someone read it and the account is
theirs. A wall of red warnings addresses neither, because people learn to click
past those within a week. Every decision below is aimed at one failure or the
other.

**Why the words are laid out the way they are.** The grid is **three across and
four down, read row-major** — left to right, then down, the way a line of text
is read. The reason is that the dominant transcription error on paper is
**order**, not spelling. BIP-39 words are unique in their first four letters, so
a misspelling is caught by the wordlist on the way back in; a swapped pair is
caught by nothing until the checksum fails, possibly months later. A
column-major grid invites the eye to read across it and pick up 01 then 07 —
which produces exactly the error nothing else can catch. Row-major three wide is
the only arrangement whose natural reading order and whose numbering agree, and
three columns keeps the line short enough that the eye finds the start of the
next row without a ruler.

**What reduces transcription error, specifically:**

| Device | What it does |
|---|---|
| **The ordinal is the primary key** | Zero padded, tabular, `--font-num`, in its own 20px gutter to the left of the word. You transcribe *07 marble*, not *marble*. Zero padding makes 01 and 12 the same width so the numbers form a true column. |
| **Ledger ruling** | A hairline under each row of three and between each column, so the return sweep lands on the right line. It is the same device a table uses, doing the same job. |
| **The first four letters carry full contrast** | Every BIP-39 English word is unique in its first four letters. Those four are set in `--text-primary`, the tail in `--text-secondary`. The reader is told exactly which part must be right. It is the only typographic emphasis on the screen. |
| **Monospace, the same monospace as a price** | `--font-num` is for digits, hashes, symbols and identifiers; a mnemonic is an identifier. Fixed advance width means a missing letter reads as a short word rather than as nothing. |
| **Confirmation by re-entry, not by checkbox** | Words 03, 07 and 11, typed back after the phrase is hidden. Three ordinals spread across three rows and three columns, so a phrase written in the wrong order fails here — in the minute when the words are still on the desk — rather than on the day the account is needed back. A wrong answer names the word actually typed (*that is word 08 — check for a skipped word*) so the person knows what to look for; after three attempts the words are shown again. |

**Why not a checkbox.** The old design ended with *I have saved my recovery code
somewhere safe* and a disabled button. A checkbox proves intent and nothing
else, and it is precisely the control people learn to click through. Re-entering
three words proves possession and costs about twelve seconds.

**Not on screen longer than it needs to be.**

- The words are **covered when the modal opens**. A cover panel explains what is
  behind it and asks the person to check nobody can see the screen and to find
  something to write on. The phrase is therefore never on screen during page
  load, during a screenshot of something else, or while the person walks away.
- It **re-covers itself** after 90 seconds or on window blur. The timer is
  visible and counts down, so the re-cover is never a surprise, and *Show them
  again* is one press.
- The reveal screen carries **no other field**. No handle, no checkbox, nothing
  to read but the words. The handle is asked on the next step.

**Copy exists; download does not.** A password manager is the most reliable
place a phrase can live, and a product that refuses to offer **Copy** pushes
people to take a screenshot instead — which lands in a cloud photo library, a
sync backup and an OCR index in one move. So the button exists, it is secondary
rather than primary, it names its destination (*Copy for a password manager*),
and the line beneath states that the clipboard is readable by other
applications and that the phrase should be pasted straight into a manager.
**Download as a file is removed from the design**: a seed phrase sitting in a
downloads folder is the worst of the three outcomes.

**The danger is one paragraph.** One brass icon in the modal head and one
sentence naming both failures in the user's own terms — *anyone who reads them
can trade as you, and if you lose them nobody, us included, can give the account
back* — and then nothing else raises its voice. Brass is the only attention
material available here: vermilion means direction and errors, and the hazard
texture is reserved for money about to be lost.

**On a phone.** Transcription to paper happens on a phone more often than on a
desktop, so at 390 the grid drops to two columns and six rows. Reading order and
numbering still agree, and the ordinal and the word still share one line.

#### The paste path

Designed for someone whose phrase lives in a password manager.

| Decision | Reasoning |
|---|---|
| **One field, not twelve boxes** | Twelve boxes look careful and are hostile to the only tool that reliably stores a phrase. The field normalises on paste: numbered-list prefixes stripped, line breaks and whitespace runs collapsed, case folded down. Pasting `1. ripple⏎2. cactus` from a note yields a valid phrase, not an error. |
| **Masked by default** | Same reason a password field is masked: the common case for pasting is a laptop in a room with other people. Word count and validity are computed while masked. |
| **Three checks, escalating** | **Count** as you type (*9 of 12 words*). **Wordlist** per word, naming the offender and its nearest neighbour (*word 4, "recieve", is not a BIP-39 word — did you mean receive?*). **Checksum** once twelve valid words are present, which is the only check that catches an ordering mistake: *all twelve words are real, but the phrase does not check out — two of them are in the wrong order, or one is the wrong word.* |
| **The derived address is shown before anything is signed** | This is the paste path's transcription check, and the strongest one available: the address is a function of the whole phrase, so a person who recognises their own address has verified all twelve words at once. Drawn in signal blue, because nothing has been settled with the server yet. |
| **An account index control, defaulted to zero** | *Why is this not my address* is the commonest EVM login question and the answer is almost always another index of the same phrase. The control sits beside the address it changes and the hint says to leave it alone. |

#### The signature step

The gateway's `message` is shown **verbatim**, in the sunken surface used for
hashes, because the contract requires the client to sign the string it was given
rather than rebuild one — and a person is entitled to read what they are
signing. Beside it, what the signature does not do: it costs nothing, moves
nothing, and authorises nothing but this login. There is no wallet prompt to
explain away, because the key is already in the tab.

`422 HANDLE_REQUIRED` is a designed state, not an error toast. A new user
reaching the signature step through the paste path is the one case where the
client cannot know in advance that an account is being created, so the handle
field appears only after the gateway says so, with a brass banner explaining why
it has suddenly appeared. **There is no separate registration step in this
product and the design must not imply one.**

The derived key is held in memory for the tab and is discarded when the tab
closes; the session token is the credential from then on.

### 10.2 Trading terminal

Reading order runs left to right from *what to trade*, through *what it has
done*, to *what it costs now*, to *what you are about to do*. The ticket sits on
the right because it is the end of that sentence and because it is where a right
hand rests.

| Width | Columns | What changes |
|---|---|---|
| **1920** | `208 \| 1fr \| 244 \| 224 \| 300` | Everything visible at once. Chart `1fr` over a 288px blotter. |
| **1440** | `200 \| 1fr \| 250 \| 288` | The trade tape folds into the order-book panel as a second tab. It is the panel you glance at least and the one that duplicates information already in the chart. |
| **1280** | `1fr \| 232 \| 268` | The market list collapses into a picker in the market strip. The blotter drops to 256px, which still shows six comfortable rows. |
| **834** (tablet) | `1fr \| 300`, two rows | Chart and blotter span the full width on top; the order book and the ticket sit side by side beneath. The market strip wraps to two lines and drops the 24h high and low. The chart toolbar drops its OHLC readout. The rail drops block time, mempool, validators and state root. |
| **390** (phone) | single column | Chart 208px, blotter 236px, then the full two-sided book. The ticket leaves the flow entirely and becomes a sheet behind a persistent Buy / Sell bar pinned above the rail. The top-bar navigation and equity readout collapse. The rail keeps height and throughput and drops the block chips. |

All of this is driven by **container queries on the terminal shell**, not media
queries, so the terminal behaves identically whether it fills a monitor or sits
inside a split pane. The widths in the table above are the reference sizes; the
queries themselves fire on the band boundaries, which sit above each reference
width so that a window a little narrower than a nominal size does not lose a
column:

| Query | Layout it produces |
|---|---|
| default | five columns |
| `@container (max-width: 1700px)` | tape folds into the book |
| `@container (max-width: 1360px)` | market list becomes a picker |
| `@container (max-width: 1080px)` | tablet: two rows, book beside ticket |
| `@container (max-width: 560px)` | phone: single column, ticket becomes a sheet |

> **The element wrapping `.term` must declare `container-type: inline-size`.**
> In `reference.html` that lives on the mock frame, which is reference-document
> chrome. Lifting `.term` without giving its wrapper a container type produces a
> terminal that never collapses.

**What never collapses**, at any width: the live price with its direction, the
two-sided order book, and the connection status. If a viewport cannot hold all
three, it is too small to trade on and the screen says so rather than degrading
quietly.

### 10.3 Portfolio

Equity is the headline; everything else explains it.

- Equity hero with unrealised, realised 24h, free and locked balances, over a
  24-hour equity curve with a dashed baseline at the starting figure.
- **Liquidation risk is a four-zone gauge, not a number.** `margin_ratio: 0.462`
  means nothing on day one; *Medium — 46.2% of 80%* does. The zones map exactly
  onto the four values `liquidation_risk` can take, so the gauge and the API can
  never disagree. A needle marks the current ratio; the critical zone is drawn
  in hazard texture.
- Open positions, with proximity meters where risk warrants them.
- The ledger, tabbed to fills, funding and transfers, with block height and
  transaction hash on every row — every movement in this product is traceable to
  a block.
- When any position is at `high` or `critical`, the hazard banner sits above
  everything with the exact remedy: *Add 1,240.00 USD of margin, or reduce the
  position by 62%, to move liquidation to 136.40.*

### 10.4 Faucet

Two states from one endpoint, shown side by side so the wait never hides the
reward.

- `available: true` → the grant list at display size and one button.
- `available: false` → a draining ring counting down to `next_available_at`, the
  same grant list dimmed behind it, and a disabled button. **The button never
  lies about being available.**
- Facts beneath: cooldown, claims made, the ledger kind it is recorded as, and
  the settlement time.
- Grant history is the append-only ledger filtered to `kind: "faucet"`, with
  block and transaction hash.
- `429 FAUCET_COOLDOWN` renders as a brass banner naming the exact unlock time.

### 10.5 Leaderboard

- Window control (`24h` / `7d` / `all`) and a *Hide bots* switch.
- **Bots are marked, always** — a signal-blue badge naming the strategy
  (market maker, momentum, noise, liquidation hunter). They are here because a
  lone human needs a counterparty; pretending they are human would make the
  ranking dishonest.
- Rank one takes a brass chip. The viewer's own row takes a brass wash and a
  *You* badge.
- A *Share of window* bar closes each row: gains grow from the left, losses from
  the right, so the sign is carried by anchor as well as colour.
- **Rank comes from the API and is never recomputed on the client.** Hiding bots
  filters rows; it does not renumber them, so rank 3 stays rank 3 and the gap
  tells you how much of the board is machine.

### 10.6 Chain explorer

The rail is the permanent version of this screen. Giving the chain a rail rather
than a page is the single decision that makes the blockchain feel like a fact
about the product rather than a claim on a marketing page.

- Hero: height, `tps_1m`, block time, total transactions, mempool depth, state
  root, and a 24-block histogram where fill height is transaction count against
  the busiest block shown.
- Recent blocks, streaming, with height, transaction count, hash, state root,
  proposer and age.
- A selected block's detail with its transaction list, each tagged by kind —
  order, cancel, liquidation, funding, transfer, faucet.

**The rail's behaviour**

| Element | Behaviour |
|---|---|
| Height | From `chain/status` and the `chain` channel. Flashes for `--dur-flash` on every block; the tint is held under reduced motion. |
| Throughput | `tps_1m`, one decimal, tabular, never abbreviated. |
| Block chips | The last twelve. Fill height is the block's transaction count against the busiest chip on the rail, making the rail a twelve-block histogram of load. A new chip slides in from the right over `--dur-slow`. |
| Mempool | Shown only when `mempool_depth > 0`; a zero is noise on a rail that is always on screen. |
| Not synced | `synced: false` turns the whole rail brass and replaces the chips with the sync distance. This is the one time the rail is allowed to be loud. |

### 10.7 Account

Route `/account`, reached from the user pill in the top bar. This page is who
you are, what you hold and what you have done; the portfolio next door is about
open risk right now. It is fed by `/user/profile`, `/user/sessions` and
`/user/activity`, with the two addresses coming from `/auth/session` — the only
endpoint that carries both.

**The two addresses.** A person who opens this page and sees an unfamiliar
address holding their balance will conclude they have been robbed, and will
conclude it in about a second. The panel is built so that the reassurance
arrives in the same glance as the problem.

| Decision | Reasoning |
|---|---|
| Two cards side by side with an arrow between them | The question is not *what are my addresses*, it is *why are there two*. Two objects with a labelled arrow is a picture of a relationship; two rows of a definition list is not. The arrow reads *computed from the one on the left*, so the direction is stated rather than implied. |
| Each card leads with who holds the key | **You hold the key** and **the gateway holds the key** are set at body size *above* the hex string, because that is the actual difference between them. The spines carry it too: bone for the address that is yours to sign with, brass for the one the system operates. |
| The reassurance is the first clause | *Your balance sits at the trading address, and nothing has been moved.* The derivation comes second. Someone who is frightened reads the first clause and stops; someone curious keeps going. |
| Determinism in the user's own terms | Not *HKDF-SHA256 of the server secret and your login address* but *one login address always maps to this same trading account, and signing in with the same twelve words on another machine brings you back here.* The mechanism belongs in the contract; the guarantee belongs on the page. |
| The trade-off is admitted, with a pointer | The note says plainly that a production exchange would take orders signed by the user's own key, and names §2 of the contract where that is written up. A simulator that hides its custodial shortcut is a simulator nobody should trust about anything else. |
| Neither address is truncated here | Everywhere else an address is `0x8f3b…c21d`. On this page both are shown in full, because this is the one screen whose job is to let a person compare an address against what their wallet shows. |

**The rest of the page.**

| Region | Behaviour |
|---|---|
| Profile | Handle, joined date, last seen, `user_id`, roles. Only the handle is mutable and the panel says so where it is edited; `PATCH /user/profile` validates against the same 3–20 character rule the join flow uses and answers `409 HANDLE_TAKEN`. |
| Statistics | Exactly the seven figures in `stats`. Volume and realised profit are **labelled as 30-day figures**, because that is what the contract sends; inventing a lifetime volume from a 30-day one is the kind of small lie that makes every other number on the page suspect. Realised profit takes a sign and a colour; a count of liquidations does not, because a count is not a direction. |
| Sessions | One row per live session. The current one takes a neutral wash and a *This session* badge instead of a revoke control. `revoke-all` excludes the requesting session, so the bulk button counts and names the others: *Sign out 3 other sessions*, never *Sign out everywhere*. Networks are the `ip_prefix` /24 and the note says why it is truncated. |
| Activity | One feed across all eight `kind` values, newest first, with the gateway's own `summary` sentence as the second column — the client never composes prose the server already wrote. Kind is a glyph **and** a word. Amount carries its sign and colour; an `order` entry has no amount and shows an em dash rather than a zero. Block height and transaction hash close every row. |
| Paging | `next_cursor`, never a page number, and the footer says so. A cursor is the only paging that survives new entries arriving while somebody is reading. |

### 10.8 Admin

Route `/admin`, in the navigation only when `roles` contains `admin` — which is
a display decision, not a security boundary; the gateway enforces the role on
every request regardless of what the client believes.

**This screen is the opposite of the terminal, on purpose.** The terminal is
dense because its user is fluent and repeats the same reading a thousand times a
day. An admin screen is where somebody acts under pressure, rarely, on
incomplete information, and every action is one they cannot take back. So here
**legibility beats density**: fewer figures, larger, further apart, every
destructive action behind a dialog that states what it is about to do before it
offers a button that does it. Where the terminal hides an explanation in a
tooltip, this screen prints it under the table.

| Decision | Reasoning |
|---|---|
| Six tiles, one number each | The overview answers one question — *is anything wrong* — and must answer it from across a room. Each tile leads with a single figure at `--fs-xl` and demotes its supporting numbers to `--fs-xs` rows, flush and one gutter apart on a canvas seam like every other panel. Large notionals abbreviate (`128.4M`), per §4. |
| One tile is allowed to shout | `accounts_below_maintenance` is the only figure on the overview that is a decision rather than a fact, so it takes a `--brass-wash` fill and a route into the user list. (It is a route, not a filter: `/admin/users` takes `query` and `is_bot` and nothing else, so the button says *Open the user list* rather than promising a filter the gateway cannot serve.) The others stay flat however large they get: a number that is large is not the same as a number that is wrong. |
| Component health is a shape and a word | A row of green ticks would break rule one. `ok` is a muted tick, `degraded` a brass triangle, `down` a vermilion cross, each beside the word, with the gateway's `detail` string in monospace after it. The list reads identically in greyscale. **The brass is on the glyph, never on the word** — §3 forbids brass as text in the light theme, so *Degraded* is set in ink and takes its weight from `--fw-semi`. The same rule governs the settle dialog's deviation figure and the markets table's stale oracle age, which is a `badge warn` rather than a brass number. |
| Jade stays reserved | A market that is `trading` keeps the jade *live* badge it has in the terminal. A user account that is merely not suspended takes a **neutral** badge, and a suspended one takes vermilion. Green is for a live market, not for "this row is fine". |
| A degraded component raises a banner | A status list is where you go to check; a banner is what finds you. `degraded` or `down` raises a brass banner above the overview naming the component, the age and the remedy, with the actions attached. Everything `ok` and the banner is not drawn. |
| Destructive controls stay visible and disabled | *Settle* is present and disabled wherever it does not apply, carrying its reason on the control: on a market that is still trading, and on every perpetual, because `POST /admin/markets/{symbol}/settle` force-settles **a dated future** and a perpetual has no expiry to settle at. A control that disappears sends the operator hunting, and hunting under pressure is how the wrong button gets pressed. Same rule as the order ticket: **a disabled button never hides its reason.** |
| Confirmations ignore the density switch | Compact exists to fit more rows on a screen; a confirmation wants fewer things on a screen. Every destructive dialog carries `data-density-lock="comfortable"`. It is the only scope in the system permitted to override the user's density preference, and it may not be used on a table. |
| The audit actor is an address | A handle changes with one `PATCH`; an address does not change at all. *Who halted this market* must still have an answer after somebody renames themselves. |
| The user table's address column is the login address | `/admin/users[].address` is the address the person signed in with, the same field `/user/profile` returns. Labelling it *Login address* rather than *Address* keeps the distinction the account page spends a whole panel establishing. |
| Bots are badged here too | Same badge, same signal blue, same strategy name as the leaderboard. An operator filtering for abusive order flow needs to see at a glance that the account submitting eight thousand orders an hour is one of ours. |
| Suspension is explained where it is offered | *Stops orders and faucet claims; does not close anything.* Existing positions stay open and keep accruing funding, because force-closing somebody's position is a different and much larger decision. The dialog requires a reason and the reason lands in the audit trail. |

#### Force-settle — catching a mistyped price before it is applied

`POST /admin/markets/{symbol}/settle` closes every position at a price the
operator types and cannot be undone. The price is the whole risk: every other
field in the dialog is already decided. Five devices stack against a typo, and
none of them is a wall of text.

| Device | What it catches |
|---|---|
| **The price in words** | *three thousand two hundred* against *thirty-two thousand*. A dropped or doubled digit is invisible in a column of tabular figures and unmissable in English. This is the single most effective control in the dialog and it costs one line. |
| **Live deviation from the oracle mark** | Computed as you type against the last oracle mark, which the market row already carries. Muted inside 2%, brass beyond 2%, vermilion beyond 10%. It names both numbers, so the comparison is on screen rather than in the operator's head. |
| **The gateway's own 10% band** | The contract rejects a price more than 10% from the mark unless `force: true` is sent. The dialog renders that as a second, explicit act — a checkbox whose label repeats both prices — instead of quietly attaching the flag. An override that takes no extra effort is not an override. |
| **Double entry** | The price is typed twice, into fields that never show each other. The oldest control in accounting, here for the same reason: a slip of the hand does not reproduce itself. A mismatch blocks the button and names the field that is empty or different. |
| **The button carries the number** | *Settle ETH-261225 at 3,200.00.* The last thing under the cursor repeats the market and the price, so the final glance is at the value rather than at the word *Confirm*. |

Consequences come before controls: the dialog opens with what will happen —
every position closed, the difference paid out, zero open interest, no undo —
and only then shows status, open interest, resting orders and the mark.
`/admin/markets` does not return a position count, so the dialog does not claim
one; it shows open interest, which it does return.

#### Margin requirements — putting the consequence in the flow

`PATCH /admin/markets/{symbol}/risk` returns
`accounts_now_below_maintenance` so the operator finds out before the
liquidation engine does. The design task is to make that number land where the
decision is being made.

| Decision | Reasoning |
|---|---|
| Every parameter shows now and new on one line | `25× → 20×`. A form showing only the new value asks the operator to remember the old one, and the old one is the whole basis of the judgement. |
| The count that exists *before* is shown before | `/admin/overview` already carries `accounts_below_maintenance` for the whole system. It is not a preview of this change and the banner does not pretend it is — it is the baseline the new figure is read against, which makes the *after* number a delta rather than a bare shock. |
| The dialog does not close on save | It becomes its own result. The consequence is the reason the endpoint returns a number at all; putting it in a toast — a thing that dwells five seconds and leaves — would throw away the only warning the contract gives. |
| The count is the headline of the result | `accounts_now_below_maintenance` is rendered at display size in vermilion beside the figure it was before, so the change reads as a delta. It is **not** accompanied by a list of accounts, because the response does not contain one and `/admin/users` carries no margin ratio to derive one from. A brass note says exactly that, so nobody reads the missing list as an omission. |
| There is a way back, and it is honest about its limits | **Restore 25× · 400 · 200** is a second `PATCH` with the values the dialog still holds. The copy says plainly that restoring does not un-liquidate anything that has already happened, because between the two requests is at least one block. |

> **A gap in the contract, raised rather than designed around.** There is no dry
> run: `accounts_now_below_maintenance` arrives *with* `applied: true`, never
> before it. A true preview would need a new endpoint, and inventing one in the
> design would put a promise on screen the gateway cannot keep. Per §12 rule 6
> this is raised here, as two asks in order of value:
>
> 1. **`?dry_run=true` on the risk PATCH**, returning the count without applying
>    anything. This is the smallest change that would make the flow genuinely
>    safe, because it turns an after-the-fact report into a preview.
> 2. **The affected `user_id`s in the response**, not just a count. *3 accounts*
>    is a statistic; three handles at 84.2%, 81.9% and 80.6% is a decision, and
>    the operator cannot get from one to the other with the endpoints that
>    exist.

#### Faucet and simulation

- Faucet configuration is a form; faucet **state** is the panel beside it. The
  useful figure is not what the authority account holds but how long it lasts,
  so `authority_balance` against `granted_24h` is shown as *about 998 days of
  grants left*. A faucet that has run dry looks exactly like a broken faucet to
  a user.
- A hand grant bypasses the cooldown, requires a reason, and its button carries
  the number — *Grant 1,000,000.00 USD to @satoshi* — so the amount is read
  twice.
- Simulation drives `bots` and `scenario` from `/admin/sim`. The bot counts are
  numeric steppers and the scenario is a segmented control over the six values
  the contract defines.
- **`invariant_violations` is the one number on this screen allowed to be
  loud.** Any value other than zero means the matching engine broke a rule it is
  supposed to be unable to break, so it escalates on its own, and it escalates
  the way liquidation does — vermilion plus hazard texture plus the word —
  because that treatment is reserved for a state nobody should be able to
  ignore. The banner names the seed and the scenario, because the value of a
  violation is that the run reproduces.

---

## 11. Voice

- Sentence case everywhere, including buttons and column headers.
- Name things by what the user understands, not by how the system is built.
- An action keeps its verb through the whole flow: *Place order* → *Order
  placed*; *Claim grant* → *Grant claimed*.
- Errors do not apologise and are never vague about what happened. They name the
  cause and the remedy: *Post-only order rejected — your buy at 64,251.00 would
  have taken liquidity. Best ask is 64,251.00.* → **Reprice to 64,250.90**
- Empty screens are invitations, not dead ends.
- Numbers in prose are written the way the interface writes them, tabular and
  unabbreviated, so a figure in a sentence and the same figure in a table are
  visibly the same figure.
- **Separators are a data-row device, not a prose device.** A middle dot may join
  two facts that share one cell of a table or one badge — `GTC · post only`,
  `10× cross` — where the alternative is a wider column. It never appears in a
  heading, a sentence, a button or a toast, where a preposition does the job
  better: *Funding in 18:42*, not *Funding · 18:42*.

---

## 12. For the frontend engineer

1. `@import url("./tokens.css")` once at the root. Set `data-theme` and
   `data-density` on `<html>`; both are persisted per user.
2. Never write a hex value in a component. If you need a colour that does not
   exist, that is a design change, not a local decision.
3. Never write a pixel value for a row height, a control height or a cell
   padding. Use the density tokens; they are the contract virtualisation
   depends on.
4. `--font-num` on everything that changes. `tabular-nums` is already in the
   `.num` utility.
5. Copy the CSS out of `docs/design/reference.html`. It is written as production
   CSS, not as demo CSS, and the component blocks are contiguous and commented
   to be lifted whole.
6. If a state you need to render is not in §9, check `docs/api-contract.md`
   first. If the field genuinely exists and the state is genuinely missing here,
   that is a bug in this document — raise it rather than inventing a treatment.

---

## 13. The 16-bit skin — "Causeway Bay, 1994"

**Status: shipped, 2026-09-22.** `rust/crates/cb-web/styles/retro.css` is the
normative artefact. It is loaded **after** `tokens.css`, `app.css` and
`shell.css`, and it redefines tokens and repaints components without touching
their markup. Where this section and `retro.css` disagree, `retro.css` wins.

The brief: an exchange simulator that is fun to be in, in the visual register
of the Super Nintendo and the Mega Drive. The design system's structure was
right and is kept — the spine, the tape rule, the rail, the nine states, the
density switch. What changes is the paint.

### What the skin keeps, on purpose

1. **Jade is up and vermilion is down, and nothing else in the product is
   green.** The neon versions are brighter but the rule is intact, and both
   still travel with a sign, a glyph or a position.
2. **Colour is never the only carrier of meaning.**
3. **Numerals are tabular.** VT323 is monospaced by construction.
4. **Comfortable and compact both work**, at slightly taller row heights so
   the taller type has room.
5. **Both themes work.** Night is SNES navy; day is the Mega Drive's sky blue
   and cream cabinet plastic.

### What the skin overrides, and says so here

| v1 rule | 16-bit rule | Why |
|---|---|---|
| No web fonts | Two bundled OFL faces, `Press Start 2P` and `VT323`, `font-display: swap` | Pixel type is the whole idea; a fallback system face paints first, so no price waits |
| Radius encodes interactivity | Everything is square | Pixels have no curves; interactivity is now carried by the **bevel** (raised = pressable, sunken = a field or a well) |
| Elevation is a top highlight, no shadow | Bevels and a hard offset shadow | A raised control has a light top-left edge and a dark bottom-right edge, and drops a 4px solid shadow; pressing it inverts the bevel and moves it 3px |
| No gradient anywhere | Two: a 3-stop wash over the landing city and a two-stop band on bars | Both are backdrops behind chrome, never behind data |
| Labels are sentence case | Labels in the pixel face are upper case | `Press Start 2P` has no lower-case worth reading at 8px; prose stays sentence case in VT323 |

### Type

| Token | Face | Sizes | Wears it |
|---|---|---|---|
| `--font-px` | Press Start 2P | 8 · 10 · 12 · 16 · 24 px, always a multiple of 8 | Headings, panel titles, buttons, badges, tabs, nav, column heads, stat labels |
| `--font-ui` | VT323 | 13–88 px, the v1 scale lifted about a third | Prose, hints, table body, anything longer than a phrase |
| `--font-num` | VT323 | as above | Digits, hashes, identifiers |

### Colour

Night: canvas `#0f1440`, panel `#1a2260`, ink `#fbf1cf`, gold `#ffd23f` for
chrome that must be noticed (the current nav tab, the primary button, rank
one, the switch's selected position), jade `#3cf39f`, vermilion `#ff5d5d`,
signal cyan `#5ad7ff`. Day inverts to a `#cfd8f7` sky with `#141c48` ink and
darkens jade and vermilion to `#0b7a4a` and `#c22a2a` so both clear AA.

### The CRT

`body::after` lays 1px scanlines every 3px and a soft vignette over the whole
viewport, `pointer-events: none`, `z-index` above everything. It is removed
under `prefers-reduced-motion`. It costs one compositor layer and no
JavaScript.

### Art

`rust/crates/cb-web/assets/` holds the sprites and backdrops, generated with
Grok (`tools/gen_assets.sh`, prompts inline) and post-processed by
`tools/knock_out.py`: the magenta screen behind each sprite is made
transparent, the sprite is trimmed, requantised to 48×48 and scaled to 96×96
with nearest-neighbour so it stays pixel art at any size. `image-rendering:
pixelated` is set on every `.sprite`. Sources sit in `art-src/`, which is not
committed. The RPG dialogue frame (`panel_frame.png`) is a `border-image` on
toasts, modals and the phone menu.

| Sprite | Where |
|---|---|
| `hero_city` | the landing hero backdrop |
| `title_logo` | the landing wordmark |
| `coin_btc` · `coin_eth` · `coin_sol` · `coin_usd` · `stock_cert` | the menu, the landing paths, the favicon |
| `mascot_trader` · `mascot_admin` | the menu's Account and Admin entries |
| `faucet` · `trophy` · `block` · `bull` · `bear` · `rocket` · `skull` · `robot` | the menu and the simulation switch |

### New components

**Menu panel** (`.menu-grid`, `.menu-item`): every screen in the product as a
grid of bevelled RPG menu entries with a sprite each; the hovered entry shows
a blinking `▶` cursor. Lives on the account page. *Buy* and *Sell* deep-link
to `/trade?side=buy` and `/trade?side=sell`, which the terminal honours by
opening the ticket on that side.

**RPC panel** (`.rpc-panel`, `.rpc-line`): where everything listens, each
line copyable — the gateway API, its WebSocket, where it listens, the node's
Cap'n Proto address, the oracle control URL, the chain id, the block time and
the simulation switch position. Lives on the account page and the dashboard.
The facts come from `GET /chain/status`.

**Simulation switch** (`.mode-switch`, `.mode-option`): three cabinet buttons
on the admin page, `role="radiogroup"`. Off, On and Max, each with a sprite,
a name and one sentence saying what it does. Max is edged in vermilion when
selected, because it is the one position that can make the rest of the page
crawl.

**Transactions table** on the dashboard: kind, market, side, sender, hash and
age for the last sixty transactions the chain applied, from `GET /chain/txs`,
refreshed every three seconds.

### Two languages

The client speaks English and Korean. English is the source language: every
string in the views is an English literal passed through `i18n::t`, and the
Korean table in `rust/crates/cb-web/src/i18n.rs` maps English to Korean, so a
string nobody has translated falls back to English rather than to nothing.
The switch sits beside the theme and density switches, stores its choice in
`localStorage`, sets `lang` on `<html>` and reloads. Under `lang="ko"` the
stylesheet leads every font stack with **Galmuri 11** (OFL), a pixel face with
Hangul, and lifts the pixel sizes a notch, because an 8px Hangul glyph is not
a glyph. The upper-case rule for pixel-face labels is switched off for Korean.
`?lang=ko` in a URL selects it too, and is remembered.
