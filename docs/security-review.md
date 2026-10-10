# Security review — SuperFrontierLiquid gateway, web and peer-to-peer layers

Date: 2026-09-18. Reviewer: security review at the project owner's request.
Scope: `rust/crates/cb-gateway`, `rust/crates/cb-web`, `rust/crates/cb-net/src/p2p`,
and the auth/admin surface of `docs/api-contract.md` §§1, 2, 9, 10 and
`docs/adr/0003`.

This is a defensive review of the project's own code. It is **read-only on
product code**; the only file written is this one.

## Threat model, as framed for this review

This is a simulator holding faucet-minted test money. There is no theft of value
to model. The questions are: can a user do what the design says they cannot; can
one user affect another; can a client wedge or crash the server; and would the
defect class survive into a system where the money was real. Each finding
therefore carries **two** severities: **Sim** (this deployment) and **Real** (the
same code with real value behind it).

## How findings were verified

A live stack was booted on the reviewer's own ports — node RPC `127.0.0.1:7740`,
dashboard `8140`, gateway `8141`, a second p2p-enabled node on `7741`/`7742` — from
the repository's own `Makefile` recipe and `.env`. Hypotheses read from the code
were then exercised against that instance. Every "Verified" finding below was
reproduced against the running gateway or node. No exploit was run against
anything outside the local instance, and no working exploit is written here for
anything dangerous outside this repository.

---

# Verified findings

## 1. Admin is whoever grabs the handle `admin` first — CRITICAL

**Weakness.** The operator role is bound to a *handle string*, not to an address
or any stable identity. `AuthService::roles_of` (`auth/mod.rs`) compares
`effective_handle(user)` against the configured `admin_handles` list, which the
`make gateway` target sets to `admin`. Handles are first-come: the first account
to register with `handle: "admin"` owns it, and `PATCH /user/profile` lets any
account rename into any handle nobody currently holds. Nothing ties "admin" to a
key the operator controls.

**Demonstrated sequence.** On a fresh gateway (memory store), an attacker with a
throwaway mnemonic — not the operator's — signs in choosing `handle: "admin"`:

- `POST /auth/dev-login {handle:"admin"}` → account created.
- `GET /auth/session` → `"roles":["user","admin"]`.
- `GET /admin/users` → `200`, full user list.

Verified live. The attacker held every `/admin/*` route: user list and detail,
suspend/unsuspend, market halt/resume/settle, risk configuration, faucet
configuration and **arbitrary faucet grants** (`POST /admin/faucet/grant`, which
mints to any user id).

**Escalation-by-rename** was also tested: a normal user (`alice`, roles `[user]`,
`/admin/users` → 403) renamed to a still-unclaimed admin handle
(`PATCH /user/profile {handle:"operator"}`) and immediately received `/admin/users`
→ 200 on the *same, unchanged session token*, because the `Admin` extractor
recomputes roles from the current handle on every request. The only thing that
saves a running production instance is that the real operator usually claims the
handle first; the "retired, not released" handle logic then refuses a second
claimant. That refusal was verified: with `admin` already held by user 1, a
second user's `PATCH /user/profile {handle:"admin"}` returned `409 HANDLE_TAKEN`
and that user's `/admin/users` stayed `403`. But on any fresh boot, or any
window where the admin handle is
unclaimed, admin is available to the first taker — this is a race the operator
cannot reliably win, and it is invisible until abused.

**Attacker gains.** Complete operator control of the exchange: suspend any user,
halt/settle any market, reconfigure risk and the faucet, and mint balances.

**Severity.** Sim: **Critical** (full cross-user and market control). Real:
**Critical** (this is the archetypal broken-access-control / privilege-by-claim
bug; in a real venue it is game over).

**Fix.** Bind the operator role to the *login address*, not the handle. Configure
`--admin-address 0x…` and check `roles_of` against `caller.login_address`.
Addresses are cryptographically owned and cannot be squatted. If handles must
stay the operator surface, reserve `admin`/`operator` at genesis so they can
never be registered by a user, and refuse any rename into a configured admin
handle.

**Status (fixed, 2026-09-19).** The operator role is now bound to a login
**address**: `--admin-address` (repeatable, EIP-55 checked at startup) and
`AuthService::with_admin_addresses`, compared in `roles_of` against the stored
login address. `--admin-handle` still exists for the local workflow and the
integration suite, but `Config::admin_policy` **refuses to start** unless
`--insecure-admin-by-handle` is passed with it, and the error names
`--admin-address` as the replacement; when the opt-in is used, `main.rs` logs a
warning spelling out that whoever registers the handle first owns every
`/admin` route. `make gateway` passes `--admin-address` when `.env` sets
`GATEWAY_ADMIN_ADDRESS` and otherwise the handle plus the opt-in, saying which
it chose. Covered by `auth::tests::the_operator_role_follows_the_login_address_not_the_handle`,
`auth::tests::renaming_into_an_admin_handle_grants_nothing_when_admins_are_addresses`,
`tests/api_admin.rs::claiming_the_handle_admin_does_not_make_you_the_operator`
(a non-operator address registering `admin` gets roles `["user"]` and 403 on
`/admin/users` and `/admin/faucet/grant`),
`tests/api_admin.rs::renaming_into_an_operator_handle_grants_nothing`, and the
`config` tests for the opt-in refusal and address checksum validation.

## 2. The server secret is a shared, committed constant with no floor — CRITICAL

**Weakness.** Every account's trading key is `HKDF-SHA256(server_secret,
login_address)` (`auth/eth.rs::derive_trading_key`). The secret is the whole
custody model (ADR 0003). Three problems compound:

- The value shipped and used is the fixed string
  `local-development-secret-do-not-reuse`. It lives in the git-tracked
  `rust/.env.example` (`git ls-files` confirms it is tracked), and `make env`
  copies that file verbatim to `rust/.env`, which `make gateway` feeds to
  `--server-secret`. `diff rust/.env rust/.env.example` → identical. So every
  deployment that follows the documented boot path shares one secret.
- `AuthService::new` rejects only an **empty** secret. There is no minimum
  length, no entropy check, and no refusal of the known-published value.

**Demonstrated sequence.** The trading address is a public field of
`GET /auth/session` and `GET /user/profile`. With the committed secret and a
victim's public login address, the reviewer re-derived the victim's trading
address **offline**, with an independent HKDF-SHA256 + secp256k1 + keccak-256
implementation, and it matched the live gateway byte for byte:

```
login_address            0x2F07c220dC62CE9531bC44B695A6D93578806d8d
offline trading address  0x72098337a464e77abcacf590bb065f83ec5a8d2d
gateway trading address  0x72098337a464e77ABcAcF590bB065F83Ec5a8D2D   (same)
```

Deriving the address means deriving the **private key** (the key-derivation
match above is verified; the private key was reproduced, not just the address).
Because bots and `cb-client` submit signed transactions straight to the node
RPC, a holder of the secret does not even need the gateway: they can sign as any
user's trading account and place, cancel, transfer or close directly at the
sequencer. *That last step — submitting a transaction signed with a
derived key directly to the node RPC — is inferred from the architecture and
ADR 0003; it was not tested against the running node in this review.*

**Attacker gains.** Total control of every account's on-chain trading key from
nothing but the public repo and a victim's login address. One user trivially
affects every other.

**Severity.** Sim: **High** (no value, but one clone of the repo compromises
every account in any deployment left on the default). Real: **Critical** — this is
the single choice ADR 0003 itself flags as "flatly wrong in a system holding real
value", and shipping a known constant realises it.

Note: `TREASURY_KEY` (the faucet minting key) sits in the same committed file. In
this simulator that is by design — the header of `.env` discloses that every
seed-derived key is public on purpose. It is called out here only so the reader
knows it was seen and judged design-accepted; the `server_secret` is a different
matter because it is presented as a secret and secures every user, not a single
faucet.

**Fix.** Give the secret no committed value at all; require it from a secrets
manager or an untracked file at boot. Enforce a minimum length (≥32 bytes) and
refuse the known placeholder string outright. Document that a real deployment must
generate a fresh high-entropy secret and never the example.

**Status (fixed, 2026-09-19).** `auth::secrets::check_server_secret` runs in
`AuthService::new` — the one constructor every deployment and test passes
through — and refuses an empty secret, anything under 32 bytes, and the
published values by exact match (both the constant this repository shipped and
the new placeholder); a legal but hand-typed-looking secret starts with a
warning. `rust/.env.example` now carries `replace-me-run-make-env`, which the
binary refuses by name, and `make env` calls a new `env-secret` target that
writes 32 random bytes (`openssl rand -hex 32`, `/dev/urandom` fallback) into
`rust/.env` per machine; `check-env` detects a `.env` still carrying either
published value and rotates it, and `make gateway` now depends on `check-env`.
Covered by `auth::secrets::tests::*` and
`auth::tests::an_unusable_server_secret_is_refused_at_construction`.
Not fixed here: the secret is still passed on the command line rather than read
from a secrets manager, and rotating it orphans existing trading balances — the
`env-secret` target says so.

## 3. Rate limiting is defeated by a spoofable header — HIGH

**Weakness.** Mutating routes are limited on two keys (`ratelimit.rs`): the
session token and the "client address". The client address
(`http/mod.rs::client_address`) is taken from the `X-Forwarded-For` header when
present, which an unauthenticated client fully controls. For unauthenticated
routes — `POST /auth/challenge`, `POST /auth/login`, `POST /auth/dev-login` — the
per-IP key is the *only* limit, and it is attacker-chosen.

**Demonstrated sequence.** With per-IP and per-session limits set to 5:

- 12 `POST /auth/challenge` with **no** header → `200 200 200 200 200 429 429 …`
  (correctly limited).
- 12 `POST /auth/challenge` each with a **different forged `X-Forwarded-For`** →
  twelve `200`s. The limit never engages.

The session limit is then void too: login mints a fresh session every call with
no per-account cap, and login is gated only by the spoofable IP key. The reviewer
minted 9 sessions for a single account by rotating `X-Forwarded-For`, each
carrying its own independent 120/min session budget. Authenticated mutating
requests confirmed the same: 10 `POST /orders/cancel-all` with rotating XFF was
capped only by the (real) session key, and a caller who simply mints more sessions
escapes even that.

**Sub-finding — the limiter and challenge maps are themselves a memory-growth
vector (Verified).** Each distinct forged `X-Forwarded-For` creates a persistent
`ip:<value>` entry in the rate-limiter `DashMap`, and each unauthenticated
`POST /auth/challenge` creates a `ChallengeStore` entry keyed by any of the 2^160
possible addresses; both live until the 300 s janitor. `X-Forwarded-For` can be
kilobytes long. The reviewer sent 5,000 challenge POSTs each carrying a unique
~8 KB `X-Forwarded-For`: all 5,000 were accepted (the limit never engaged, again
confirming the bypass) and gateway RSS grew from ~14 MB to ~55 MB — the ~40 MB is
the forged header strings retained as limiter keys. So the rate limiter meant to
bound abuse is, under a forged header, an amplifier of it.

**Attacker gains.** Unbounded challenge/login/registration volume from a single
host; account-creation floods; the per-session throttle rendered ineffective;
attacker-controlled server-side map growth.

**Severity.** Sim: **High** (a single client can flood auth and registration and
grow unbounded server-side maps — see finding 7 and the challenge map). Real:
**High** (credential-stuffing / registration-abuse throttles that a header
defeats are a standard serious finding).

**Fix.** Do not trust `X-Forwarded-For` from arbitrary clients. Use the TCP peer
address (`ConnectInfo`, already wired in) as the rate-limit key, and honour
`X-Forwarded-For` only when the immediate peer is a configured, trusted proxy.
Add a per-address (login address) cap on session creation.

**Status (fixed, 2026-09-19).** `x-forwarded-for` is now honoured only when
the immediate TCP peer is named by `--trusted-proxy`, and then only its first
element and only if that element parses as an IP; otherwise the key is the real
socket address. The resolved value is attached once as a `ClientAddress`
request extension, so the limiter and session attribution (finding 9) agree.
The limiter map is bounded at 50,000 keys with eviction (stale windows first,
then oldest), chosen over refusing new keys at the cap so a flood cannot lock
out later callers. Covered by
`http::tests::a_forged_forwarded_header_does_not_choose_the_rate_limit_key`,
`http::tests::a_forwarded_header_is_honoured_only_from_a_configured_proxy`,
`ratelimit::tests::the_key_map_is_bounded_even_under_a_flood_of_new_keys`,
`ratelimit::tests::a_caller_at_the_ceiling_is_still_tracked_rather_than_locked_out`
and `tests/api_auth.rs::a_rotating_forwarded_header_does_not_evade_the_rate_limit`
(twelve forged values against a budget of five now yield 5/7, and an 8 KB header
is refused rather than stored). The report's suggested per-login-address cap on
session creation was **not** implemented.

## 4. A peer forces ~64 MiB of allocation per connection before the handshake — HIGH

**Weakness.** The p2p reader (`p2p/transport.rs`) reads a 4-byte length, checks it
against `MAX_FRAME_BYTES` (64 MiB), then does `let mut frame = vec![0u8; len];`
and blocks in `read_exact`. The length check is correct as far as it goes (the
codec docs are right that a single frame cannot exceed the cap), but the
allocation happens **per connection, before any handshake, identity or
authentication**, and before the engine's `max_peers` rejection can apply — that
rejection only fires on the decoded `Hello`, which never arrives. Inbound
connections are `accept`ed in an unbounded loop and each spawns a reader that will
allocate up to 64 MiB on the strength of 4 attacker-chosen bytes.

**Demonstrated sequence.** Against the p2p-enabled node, the reviewer opened 24
plain TCP connections, sent only a 4-byte big-endian `0x04000000` (exactly 64 MiB)
on each, and sent nothing further — 96 bytes of attacker input in total, no
handshake. Resident memory went from ~15 MB to ~940 MB and stayed there while the
sockets were held. The amplification factor is roughly ten-million-to-one
(bytes-sent to bytes-allocated).

**Attacker gains.** A remote, unauthenticated peer drives a node to memory
exhaustion / OOM with negligible bandwidth and no protocol knowledge.

**Severity.** Sim: **Medium–High** (a node can be OOM-killed; the reviewer only
opened 24 connections). Real: **High** (trivial remote DoS of a consensus node;
the transport module's own docs already flag that adding transport hardening "would
still be the first thing to add before running this on a public network", and this
is a concrete instance).

**Fix.** Do not allocate the full declared size up front. Cap the pre-handshake
frame far below 64 MiB (a `Hello` is tiny), grow the buffer incrementally as bytes
actually arrive, and impose a global cap on concurrent inbound half-open
connections and on total in-flight frame bytes across peers. Enforce `max_peers`
before spawning the reader, not after decoding `Hello`.

**Status (fixed, 2026-09-19).** The reader no longer allocates the declared
size: `read_frame_into` grows the buffer with the bytes that actually arrive,
so a declared 64 MiB with 100 bytes sent costs 100 bytes and an error. Before a
connection has decoded its first message the cap is `MAX_HANDSHAKE_FRAME_BYTES`
(16 KiB) rather than `MAX_FRAME_BYTES`, and the first frame is under a
`HANDSHAKE_TIMEOUT` (10 s) so a silent socket cannot hold a slot. Inbound
connections are capped at `max_peers` by `InboundSlots` **in the accept loop**,
before a reader task exists — the engine's own `max_peers` check needs a
decoded `Hello`, which this attacker never sends. Covered by
`p2p::transport::tests::a_declared_frame_costs_the_bytes_that_arrive_not_the_bytes_claimed`
(asserts the buffer's capacity stays under 1 MiB),
`…::a_complete_frame_is_read_exactly`,
`…::inbound_slots_cap_connections_and_release_on_drop` and
`…::the_handshake_cap_is_far_below_the_frame_cap`. Not fixed: there is still no
global cap on total in-flight frame bytes across peers, and no transport
encryption or authentication.

## 5. One unauthenticated WebSocket socket can hold unbounded subscriptions — MEDIUM–HIGH (resource)

**Weakness.** `Channel::parse` (`ws.rs`) accepts any non-empty uppercased symbol
string for `orderbook:`/`trades:`/`candles:` without checking it against the market
catalogue or bounding its length, and `Hub::subscribe` stores each accepted
channel in a per-connection `HashSet<Channel>` with **no cap on the number of
subscriptions**. WebSocket frames are not rate-limited (reads are exempt by
design), and a socket needs no session to subscribe to the public channels.

**Demonstrated sequence.** A single connection, never authenticated, sent 20,000
`subscribe` operations for distinct 500-character junk channel names (`trades:ZZZ…`)
while draining replies so it was never dropped as "slow". All were accepted; the
gateway's resident memory grew ~11 MB for that one socket, and the set persists for
the life of the connection. Multiplied across many sockets (finding 3 removes any
connection-rate limit) this is a straightforward memory-growth vector. The slow-client
drop (correctly implemented, see Controls) does *not* help here, because the client
keeps reading.

**Attacker gains.** Server memory growth proportional to attacker persistence, from
unauthenticated sockets.

**Severity.** Sim: **Medium**. Real: **Medium–High** (unauthenticated
memory-amplification on the public market feed).

**Fix.** Validate every subscribed symbol against the catalogue and reject unknown
ones (as the REST layer already does via `resolve_market`), bound channel-name
length, and cap the number of live subscriptions per connection. Consider a light
per-connection frame-rate limit on the socket.

**Status (fixed, 2026-09-19).** `Channel::parse` now rejects any name longer
than 64 bytes and requires the symbol component to be well formed
(`symbols::is_well_formed`, the same shape test the REST layer applies), and
`Hub::subscribe` caps a connection at 128 live subscriptions, returning
`RATE_LIMITED` beyond that. Symbols are deliberately **not** checked against the
market catalogue at subscribe time: the gateway serves `/ws` before its first
catalogue refresh by design, so a live-catalogue check would reject honest
subscriptions during startup; the length and shape bounds are what make the set
finite. Covered by
`ws::tests::a_junk_channel_name_is_refused_before_anything_stores_it` (the
reviewer's 500-character names, plus the exact boundary) and
`ws::tests::one_connection_cannot_hold_unbounded_subscriptions`.

## 6. Signature malleability: the low-s rule is not enforced — MEDIUM

**Weakness.** `parse_signature`/`recover_personal_sign` (`auth/eth.rs`) accept `v`
of 0/1/27/28 correctly, but never check that `s` is in the lower half of the curve
order. secp256k1 signatures are malleable: `(r, s, v)` and `(r, N−s, v⊕1)` recover
the same signer.

**Demonstrated sequence.** A valid low-s challenge signature was transformed into
its high-s twin (`s → N−s`, `v` flipped) and presented to `POST /auth/login`. It
returned `200` and logged the user in. Verified live. (For finding 7's
replaced-challenge case the wire returned HTTP `401`; the `CHALLENGE_EXPIRED`
classification there is read from source, since the error code is nested under
`error` in the body.)

**Impact here.** Limited. The login challenge is single-use and consumed on the
first `consume`, and the recovered address is compared in full, so the malleable
twin cannot be used to *replay* a login (the reviewer confirmed a used signature
re-presented → 401). So in this design the missing low-s rule does not currently
buy an attacker a second login. It is reported at Medium because the review
explicitly requires low-s handling, because the same `recover`/`parse` primitives
are shared with any future path that treats a signature as an identifier or
idempotency key, and because "malleable but currently unexploitable" is a fragile
property to rely on.

**Severity.** Sim: **Low**. Real: **Medium** (a signature that doubles as a
transaction id or a dedup key under a malleable scheme is a classic source of
double-processing bugs).

**Fix.** Reject `s > N/2` in `parse_signature` (canonical low-s), matching what the
signer already emits, so the two halves of the system enforce the same rule.

**Status (fixed, 2026-09-19).** `parse_signature` rejects `s` above
`floor(N/2)` with a new `EthError::HighS`, so the verifier now enforces the rule
the signer already obeyed. Covered by
`auth::eth::tests::the_high_s_twin_of_a_valid_signature_is_refused`, which
tests both twins — the shipped low-s signature authenticates, its `(r, N-s,
v^1)` twin is refused — plus
`…::the_half_order_constant_is_actually_half_the_curve_order` (checks the
constant against `N` rather than trusting it) and
`…::the_boundary_value_of_s_is_still_canonical`.

## 7. An unauthenticated party can knock a victim's login off its challenge — MEDIUM

**Weakness.** `ChallengeStore::issue` stores one challenge per address and
*replaces* any outstanding one (`auth/challenge.rs`), and `POST /auth/challenge`
requires no session and no proof of address ownership. Login addresses are public:
`GET /leaderboard` returns each player's **login** address (confirmed in source —
`leaderboard.rs` sets the row's `address` to `player.address`, which the same file
documents as the stored login address, deriving the trading address separately for
equity). The reviewer's live leaderboard was empty (no player had equity or
trades yet), so this precondition is confirmed from source rather than from a
populated response. So anyone who knows a victim's address can, at any
moment, request a challenge "for" that address and overwrite whatever challenge the
victim is mid-way through signing; the victim's signature then matches no stored
challenge and login fails with `CHALLENGE_EXPIRED`.

**Demonstrated sequence.** The reviewer confirmed `POST /auth/challenge` returns
`200` for an arbitrary address with no authentication, and that a second issue for
the same address replaces the first (the store's own tests assert the replacement).
An attacker looping this against a victim's address keeps the victim's login failing.

**Attacker gains.** A targeted, low-effort denial of login against a chosen user.
Not a takeover — the attacker still cannot produce the victim's signature.

**Severity.** Sim: **Low–Medium** (annoyance; the victim's retry can win the race).
Real: **Medium** (targeted account lockout).

**Fix.** Key challenges so a new issue does not evict an unrelated in-flight one
(e.g. keep a small set of live challenges per address and accept a signature over
any of them), and/or rate-limit issuance per address on the true peer IP (finding 3).

**Status (fixed, 2026-09-19).** `ChallengeStore` keeps up to four live
challenges per address (oldest evicted first) instead of one, bounded overall at
50,000 addresses with eviction, and a signature over **any** live challenge logs
in. Only the matched challenge is retired, so a signature is still single use
but a failed attempt — anybody's — retires nothing. That is a deliberate change
to the control this review listed as correct ("consumed whether or not the
signature verifies"): consuming on failure would have moved the same denial of
login from `POST /auth/challenge` to `POST /auth/login`, and the property given
up is not load-bearing because the nonce is 128 bits from `OsRng`. Covered by
`challenge::tests::a_second_issue_does_not_evict_a_challenge_someone_is_signing`,
`…::the_live_set_per_address_is_bounded_oldest_first`,
`…::the_address_map_does_not_grow_without_bound`,
`auth::tests::a_stranger_cannot_knock_a_pending_login_off_its_challenge`,
`auth::tests::a_failed_login_does_not_retire_anybody_elses_challenge` and
`auth::tests::retiring_the_matched_challenge_is_what_makes_a_signature_single_use`.
The retire is the atomic single-use point: verification no longer removes
anything, so `login` refuses when `retire` reports the challenge was already
gone, and two concurrent presentations of one captured signature cannot both
mint a session.

## 8. Internal backend error text is returned to the client — LOW–MEDIUM

**Weakness.** `ApiError::into_response` (`error.rs`) always serialises `self.message`
into the response body, including for `INTERNAL`. `PortError::Internal`/`Encoding`
map to `ApiError::internal(e.to_string())`, and several call sites build internal
errors from lower-layer error strings (`signer.rs`, `faucet.rs`, `auth/mod.rs`). A
`500` therefore can carry backend Rust error text to the client.

**Verified, with a concrete sample.** The `into_response` path copies `message`
for every code, and the conversion copies raw `e.to_string()` for
internal/encoding failures. A live sample was captured on a sibling path: with the
node stopped, the **unauthenticated** `GET /health` returned
`"detail":"connect to 127.0.0.1:7799: Connection refused (os error 61)"` — i.e. it
echoes the gateway's internal RPC target address and the OS error string to any
caller. The same `node_unavailable(e.to_string())` text reaches clients as a 503
`detail` on chain-backed endpoints when the sequencer is down. So the disclosure
is not merely a mechanism: the internal RPC endpoint address leaks unauthenticated.

**Severity.** Sim: **Low**. Real: **Low–Medium** (internal error disclosure; can
reveal storage/driver detail).

**Fix.** For `INTERNAL`, log the detail server-side (already done) but return a
generic message plus the request id to the client, as the auth layer already does
for `401`.

**Status (fixed, 2026-09-19).** `ApiError::into_response` now replaces the
message for `INTERNAL` and `NODE_UNAVAILABLE` with a generic sentence pointing
at the `x-request-id` header the middleware already sets; the full text is still
logged server-side. `/health` reports each dependency's status as before but
its `detail` is now `"unreachable; see the gateway log"` rather than the
backend's own string, so the unauthenticated sample in this finding — the
sequencer's RPC address and an OS error code — no longer leaves the process.
Covered by `error::tests::an_internal_error_does_not_tell_the_client_what_broke`
and `tests/api_market.rs::health_reports_every_dependency_separately`.

## 9. Client-controlled attribution — LOW

`http/auth.rs::describe` takes the session's recorded client address from the
first `X-Forwarded-For` element and stores it (truncated) as `ip_prefix` in the
session metadata the user page shows. Because the header is client-forgeable
(finding 3), the "where you signed in from" shown to a user, and the attribution a
future audit trail would lean on, is spoofable. ADR 0003 already notes the audit
log "is not cryptographic evidence of anything"; this makes the point concrete.
Sim: **Low**. Real: **Low** (audit-integrity caveat). Fix: derive it from the
trusted peer address.

**Status (fixed, 2026-09-19).** `describe` no longer reads the request's
`x-forwarded-for`; it uses the `ClientAddress` the router resolved under
finding 3's rules, so the address recorded in session metadata is the real peer
unless a configured proxy forwarded it.

## 10. CORS defaults to any-origin — LOW

`cors_layer` (`http/mod.rs`) treats an empty origin list, or `*`, as
`AllowOrigin::any()`. The `make gateway` target does pass explicit origins, so the
default only bites a hand-rolled launch. Auth is a bearer token in an
`Authorization` header, not a cookie, so a permissive origin does not by itself
enable a CSRF-style cross-site read (credentials mode is only enabled on the closed
list). Sim: **Low**. Real: **Low–Medium**. Fix: require an explicit origin list;
do not treat empty as permissive.

**Status (fixed, 2026-09-19).** An empty origin list no longer means
"any origin": `cors_layer` treats only an explicit `*` as permissive, and logs a
warning when it sees one. Nothing else changes — the credential is a bearer
token rather than a cookie, so this was never a CSRF vector here; the default
was tightened because "empty means permissive" silently disagrees with what the
operator wrote.

---

# Suspected / not fully measured

## S1. Trading-index rebuild as a CPU amplifier — SUSPECTED

`user_by_trading_address` (`auth/mod.rs`) rebuilds the whole trading index on a
lookup miss whenever the user count has changed since the last build, and the
rebuild derives an HKDF + secp256k1 key for **every** user (`list(true,
1_000_000)`). The watcher resolves trading addresses for block traffic dominated
by bot addresses that belong to no account — each a miss. Combined with unbounded
account creation (findings 1/3 remove the throttle), an attacker could keep the
user count changing so that block processing repeatedly pays a full O(users) EC
derivation pass. This is a plausible CPU-exhaustion amplifier but the reviewer did
**not** measure it, so it is labelled suspected. Fix: cap the rebuild rate, and
resolve trading addresses by iterating known accounts rather than rebuilding on
miss.

## S2. `POST /auth/dev-login` re-exposure behind a proxy — SUSPECTED (design note)

The dev-login endpoint accepts a mnemonic over the wire. Its guard is correct as a
bind-time check: `Config::dev_login_allowed` refuses to mount it unless
`--listen` is a loopback `SocketAddr` (verified in the config tests, and `--listen`
being a `SocketAddr` means a hostname resolving to loopback cannot be smuggled in).
The residual risk is operational, not a bypass of the check: `make gateway`
**always** passes `--dev-login`, so any reverse proxy placed in front of the
loopback-bound gateway re-exposes a mnemonic-accepting endpoint to the network,
and the gateway cannot tell. The endpoint does log a warning on every use. This is
called out as a deployment hazard, not a code defect. Fix: gate the `make gateway`
`--dev-login` flag behind an explicit opt-in, and have the endpoint additionally
refuse a request whose immediate peer is non-loopback.

---

# Controls verified correct

These were checked and found sound; a review that lists only problems hides what
was actually examined.

**Signature and challenge (the parts that are right).**
- The recovered address is compared **in full** to the claimed address
  (`login`: `recovered != *address` → reject), and the error deliberately does not
  reveal what it recovered — no login oracle. Verified.
- `v` values 0, 1, 27 and 28 are all accepted and normalised; malformed
  signatures and addresses are rejected; a bad recovery id is a clean error.
- EIP-191 uses the byte length (tested against a multi-byte character), and the
  EIP-55 checksum is enforced on mixed-case addresses (a flipped-case address is
  refused) — a wrong checksum cannot credit a different account.
- Challenges are single-use (consumed whether or not the signature verifies),
  address-bound (one address's challenge does not answer for another), and expire
  in five minutes. A replayed, already-used signature → 401. Verified live.

**Sessions.**
- Tokens are 256 bits from `OsRng`, prefixed `sess_`. Only the SHA-256 **hash** is
  stored; the comparison uses `subtle::ct_eq` even though the lookup is already by
  hash. Verified the token never appears in the gateway log (`grep sess_` → 0) and
  is never placed in a URL.
- Logout revokes: `GET /auth/session` returns 200 before logout and 401 after.
  Verified live.
- The four failure modes (missing / unknown / expired / revoked / orphaned) are
  distinguished internally and collapsed to a single uniform 401 at the boundary.
- **No session fixation.** Tokens are always server-minted at login; there is no
  code path that accepts a client-proposed token, and each login issues a fresh
  one (a second login for the same account returned a different token — verified).
- **No address-existence oracle.** `POST /auth/challenge` for a never-seen address
  (`0x…0001`) returns the same 200 and the same response shape as for a known one —
  verified live; the endpoint does not reveal whether an account exists. The
  address comparison after recovery is not constant-time, which is correct: both
  the recovered and claimed addresses are public values, so there is no secret to
  leak through timing.

**Trading key.** `HKDF-SHA256` derivation is deterministic and one-way, never
stored, with the derived scalar checked against the curve order. Given the secret
is sound (finding 2), the primitive itself is correct.

**Authorisation and IDOR.**
- Every `/admin/*` handler takes the `Admin` extractor, which recomputes the role
  from the server's own record on **every** request and never trusts client input;
  a non-admin gets 403. (The role *binding* is the flaw in finding 1; the
  *enforcement* mechanism is correct.)
- All `/user/*` and `/account/*` routes are scoped to `caller.user.user_id`; none
  takes a user id from the request body. `DELETE /orders/{id}` checks
  `record.user_id != caller.user.user_id` and returns 403, not a 404 that would
  leak existence. The single endpoint that takes a `user_id` from the body
  (`POST /admin/faucet/grant`) is `Admin`-gated. The classic IDOR was looked for
  specifically and not found.

**Input handling.**
- SQL is fully parameterised: every repository uses `.bind(...)`; the only
  `format!` into a query string interpolates a compile-time `COLUMNS` constant, not
  user input. No `LIKE`/concatenation of caller data was found.
- A 10 MB body → 413 (`DefaultBodyLimit`, ~2 MB); deeply nested JSON (10k deep) →
  422, no crash; `?limit=999999999` is clamped by `clamp_limit`; `?limit=-1` → 400.
  The gateway stayed healthy throughout.

**WebSocket privacy and backpressure.** The `account` channel requires a session
and is delivered by user id; there is no path that broadcasts it beyond its owner
(guarded by the crate's own test). Each connection has a bounded queue and a slow
client is dropped rather than buffered — verified live (a non-draining client was
closed with a "fell behind" warning), which is the correct answer to head-of-line
memory growth. (The unbounded *subscription-set* is a separate gap — finding 5.)

**P2P framing and hygiene.** The 4-byte length prefix is validated against
`MAX_FRAME_BYTES` before decode; IP-address fields are length-checked (4 or 16
bytes) before use; peers are scored and banned rather than trusted; `max_peers` is
enforced on handshake; a poisoned chain mutex is recovered rather than propagated.
The framing check is correct — the defect in finding 4 is *where* the allocation
happens, not the length validation itself.
- **A peer cannot make the node dial or remember peers without bound** (read in
  `engine.rs`): a `Peers` response longer than `2 × peers_per_response` is rejected
  (`engine.rs:759`), the known-peer table is capped at `max_known_peers`
  (`engine.rs:1193`), and outbound dialing is bounded by `max_peers` minus the
  current connection count (`engine.rs:1240`). So gossip discovery cannot be turned
  into an amplified outbound-connection or memory-growth vector via the peer list.

**Browser key handling.** The mnemonic never reaches the API layer (no `mnemonic`
/`phrase` field in any request builder in `cb-web/src/api`), and dev-login is
deliberately not implemented client-side. Only the session token is persisted, in
`localStorage`; the phrase and derived key are not stored, and a reload asks the
user to sign in again. `Wallet`'s `Debug` is hand-written to omit the phrase and
key; there is no download (the UI states why); the phrase is dropped from memory
once used; and `WalletError::BadPhrase` deliberately does not narrate which word is
wrong. The canonical derivation vector is pinned in tests.

---

# What could not be tested, and why

- **A full malicious-peer handshake / ban cycle.** Exercising the gossip state
  machine end to end needs a Cap'n Proto-speaking peer; the reviewer tested the
  pre-handshake transport (finding 4) but did not drive the scoring/ban logic with
  a live adversarial peer. The ban logic was read, not run.
- **Session expiry over real wall-clock.** The TTL is 24 h; expiry was confirmed by
  code reading and by the logout path, not by waiting out a real session.
- **The trading-index CPU cost (S1).** Identified from code but not measured; hence
  "suspected".
- **Direct signed-transaction submission to the node RPC using a derived trading
  key (finding 2).** The key derivation was reproduced; the end-to-end submission at
  the RPC boundary was not exercised and is inferred from the architecture.

## A note on the working tree

The only file this review created or modified is `docs/security-review.md`. During
the review `cargo build` was run to produce the release binaries; that re-resolved
`rust/Cargo.lock` because the workspace already contained an uncommitted `cb-e2e`
crate (added to `rust/Cargo.toml`) that is a **parallel reviewer's** work, not this
review's. The untracked `docs/flow-review.md`, `docs/ui-review/` and
`rust/crates/cb-e2e/` are likewise not from this review. No `rust/` source file was
edited here.

---

# Priority

1. **Finding 1** — bind admin to an address, not a squattable handle. (Critical,
   trivially exploited on any fresh boot.)
2. **Finding 2** — stop shipping a shared committed `server_secret`; require a real
   one and enforce a floor.
3. **Finding 3** — stop trusting `X-Forwarded-For` for rate limiting.
4. **Finding 4** — bound p2p pre-handshake allocation and inbound connections.
5. **Finding 5** — cap and validate WebSocket subscriptions.
6. **Finding 6** — enforce canonical low-s.
7. Findings 7–10 and the suspected items as capacity allows.

---

# Remediation status, 2026-09-19

Findings 1–10 are fixed; each carries a **Status** line above the heading that
follows it, naming the change and the test that now covers it. Deliberately
**not** done:

- **S1 (trading-index rebuild as a CPU amplifier).** Unmeasured in the review
  and untouched here; it needs a rework of `user_by_trading_address`, not a
  guard.
- **S2 (`--dev-login` behind a proxy).** `make gateway` still passes
  `--dev-login`, and the endpoint still does not check its immediate peer. The
  bind-time loopback check is unchanged and correct.
- **A per-login-address cap on session creation** (suggested under finding 3).
  The address key is now honest, which is the part that was broken.
- **A global cap on in-flight p2p frame bytes across peers** (suggested under
  finding 4). Per-connection allocation is now proportional to bytes received
  and inbound connections are capped, which bounds the product.
- **Moving the server secret out of the process's configuration entirely**,
  into a secrets manager (suggested under finding 2). What changed is that a
  published or weak value is refused, and that the Makefile now passes the
  secret through `CB_GATEWAY_SERVER_SECRET` rather than argv, so it is no
  longer visible in `ps` to everyone on the machine.

Two things other engineers' work needs to know about. The end-to-end suite
(`rust/crates/cb-e2e`) starts the gateway with `--admin-handle admin`: that now
needs `--insecure-admin-by-handle` beside it, which belongs in the stack's
`extra_gateway_args`, or `--admin-address` naming the operator's login address.
And `shipped::stack_env()` falls back to `.env.example` when there is no
`rust/.env`, which on a fresh clone is now a secret the gateway refuses by name
— the stack should generate a per-run secret, or `make e2e` (which now depends
on `check-env`) will write a real `.env` first. `rust/.env` on this machine
already carries a generated secret, so nothing local is orphaned by the change.

One listed control changed deliberately. Under **Controls verified correct**,
"challenges are single-use (consumed whether or not the signature verifies)" is
no longer true of the failure case: a challenge is now retired only by the
signature that matches it, because consuming on failure would have moved
finding 7's denial-of-login from `POST /auth/challenge` to `POST /auth/login`.
Single use on *success* is unchanged and is now enforced by the store's removal
rather than by verification, so two concurrent presentations of one signature
cannot both mint a session.

Two Makefile gaps that this work surfaced were fixed while in the file. `make
dev` used to background `cargo run --release -p cb-gateway -- $(GATEWAY_ARGS)`
with no flags at all — no secret, no treasury key, no operator — so with the
secret now mandatory it could only ever have started a gateway that died on
launch. And `make up` called the foreground `gateway` target, so it never
returned and never printed the URLs it promises. There is now one definition of
how the gateway is launched (`gateway_launch`), a foreground `gateway`, a
background `gateway-bg` with a log, a pid file, a readiness check on `/health`
and the operator binding echoed into both terminal and log, and `make stop`
kills it by that pid file. `dev` and the root `up` both call `gateway-bg`. The
two secrets now reach the process through `CB_GATEWAY_SERVER_SECRET` and
`CB_GATEWAY_FAUCET_TREASURY_KEY` rather than argv, where `ps` would show them
to every other user on the machine.
