# Security

SuperFrontierLiquid is a **personal research prototype** for local experiments
with simulated assets. It is unsuitable for real funds or production use, and
its shipped configuration is built for one developer's machine. This page says
what it trusts, what is known to be unsafe, and how to report something new.

Committed keys, mnemonic examples, and deterministic seeds are public test
fixtures. Never fund their addresses or reuse them outside disposable tests.
Create a fresh test wallet; never import a recovery phrase from a wallet
holding real assets. Keep local secrets and generated wallet stores out of Git.

## Do not run the shipped configuration on a network

Everything below is true of a chain started with `make start`,
`make startbft` or any of the shipped `genesis*.jsonl` files.

| What | Why it matters | Where |
|---|---|---|
| Validator keys are derived from published seeds (`cb-validator-N`) | Anyone with this repository can sign votes for every validator and forge quorum certificates | `rust/genesis*.jsonl`, `cb-node/src/keystore.rs`, `rust/scripts/bft-cluster.sh` |
| The oracle and treasury keys are in `rust/.env.example` | Anyone can post any price, or mint any amount with `Deposit` | `rust/.env.example` |
| The Makefile grants admin by handle when `GATEWAY_ADMIN_ADDRESS` is unset | Whoever registers the handle `admin` first on a fresh store is the administrator | `Makefile` (`--insecure-admin-by-handle`) |
| `/auth/dev-login` is on under `make gateway` | Loopback only, but a reverse proxy on the same host exposes it | `cb-gateway/src/config.rs` |

Generate fresh keys, write your own genesis, and set `GATEWAY_ADMIN_ADDRESS`
before any of it leaves `127.0.0.1`.

## The trust model

The consensus layer is real: under BFT (ADR 0008), four validators order
blocks with a chained-HotStuff protocol that tolerates one Byzantine
validator, and every validator re-executes every block. Everything above the
ordering is one operator's:

* **Custody.** The browser signs only the login challenge. Every order,
  cancel and transfer is signed by the gateway with a key derived as
  `HKDF(server_secret, login_address)` (`cb-gateway/src/auth/eth.rs`).
  Login addresses are public, so whoever holds `GATEWAY_SERVER_SECRET`
  can sign as every user. Withdrawals are disabled on chain.
* **Prices.** One key per market, fixed at genesis, posts oracle prices.
  Nothing bounds how far one update moves the price, and the mark follows
  the oracle to within 1%. That key can liquidate any position.
* **Supply.** The treasury key mints without limit through `Deposit`; the
  faucet uses it.
* **Validators.** The set is fixed at genesis. There is no staking, no
  set change and no slashing; equivocation is logged, not punished.
* **Sessions.** A 24-hour bearer token is enough to transfer funds out of
  an account.

## Known issues

Found by the audits of 2026-09-25 and 2026-09-27 and not yet fixed. The
three critical findings of the second audit (a forged timeout certificate
forking an honest validator, a replayed finality proof forking a syncing
node, funding charged without the price) are fixed in `73dd99b`.

**Denial of service**

* Any handshaked peer, not only a validator, may send `TxBatch` messages of
  any size; they share the verification queue RPC clients use.
* Transactions arriving by gossip bypass the mempool's size limit.
* The inbound peer cap is global with no per-address limit: 32 identities,
  which cost nothing, lock out a restarting validator.
* 1,024 parked `streamBlocks` calls hold every RPC slot indefinitely.
* The peer-to-peer listener exits on its first `accept()` error and is not
  restarted.
* The consensus inbox is bounded by message count, not bytes.

**Consensus and sync**

* A block's finality proof is one quorum certificate. It is now bound to
  the block's height and to the previous block's proposal, but a proposal
  certified on a branch later abandoned would still pass.
* The safety record (the last round a validator voted in) is written
  through RocksDB's write-ahead log without an fsync: a power cut can let
  a restarted validator vote twice in one round.
* Timeout signatures carry no chain or genesis identifier.

**Crash durability** (single-sequencer mode)

* Blocks are announced after the RocksDB write returns, not after an
  fsync. A power cut after the announcement can leave replicas holding a
  block the restarted sequencer no longer has, and it will produce a
  different one: a fork. `docs/crash-recovery.html` walks every case.
* Balance, position and order-book reads include up to eight executed but
  unwritten blocks, so a value read just before a crash can revert.

**Economics**

* Derivative trades and liquidation now require marks and their oracle source
  heights within 600 blocks. Funding and dated settlement still need a complete
  oracle-outage policy; the trading bound does not solve oracle compromise.
* A block proposer may add signed funding ticks of its own and choose when
  the rate is sampled.
* A reduce-only close into a colluding bid at the edge of the price band is
  paid for by the insurance fund.
* Once the insurance fund is empty, winners are still paid in full; there
  is no auto-deleveraging.
* Rounding dust goes to traders rather than to the exchange.

**Gateway**

* Under the sequencer scheme other people's transactions are assumed
  applied; a sequencer block never carries a refused one. Under BFT the
  block's `applied` mask decides, for everyone's transactions
  (2026-09-29).
* The login challenge has no domain or chain id, so a phishing site can
  relay a genuine one.
* Rate limiting trusts the first `X-Forwarded-For` entry.
* The faucet has no per-user claim limit, and accounts are free.

## Audit fixes (2026-09-30)

The five reproduced findings in `docs/audit-2026-09-29.md` are fixed: negative
cash remains a liability until an account is flat; makers are checked at fill
time; configured margin/leverage/caps are enforced alongside tier floors;
stale marks cannot authorize trades or liquidation; and block identity excludes
mutable certificate/signature witnesses. BFT timestamps must equal the signed
proposal timestamp. These are consensus changes requiring a fresh chain and
matching binaries on all nodes; see ADR 0009. The open issues above remain.

## Reporting a vulnerability

Please report it privately through GitHub: the repository's **Security** tab,
**Report a vulnerability**. Include the commit, what an attacker needs, the
steps, and what breaks. A failing test is the most useful form.

Only the latest commit on `main` is supported. There is no bounty; this
project has no company behind it.

Something already listed above does not need a report, though a pull request
that fixes it is welcome ([`CONTRIBUTING.md`](CONTRIBUTING.md)).
