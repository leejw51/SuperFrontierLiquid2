# ADR 0003 — Custodial wallets in v1

Status: accepted, revised the same day. Date: 2026-09-18.

**Revision.** This ADR originally decided a fully custodial model: the gateway
generated a keypair at registration, stored it, and authenticated the player
with a handle and a recovery code. That is no longer what is built. Identity is
now the player's own key and only the *trading* key is custodial. The decision
below is the revised one; the context and most of the consequences survived the
change, which is why this is a revision rather than a new record.

## Context

Every transaction on this chain is a secp256k1 signature over the `bincode`
encoding of a `Transaction`, recovered to an address the executor then checks
the nonce and balance of. Somebody has to hold that key.

The product is a playable simulator. The whole value of it is that a person can
open a page, pick a handle, claim faucet funds and be trading inside a minute,
against bots that are already quoting. Anything that asks them to install an
extension, save a seed phrase, or approve a signature per order will lose most
of them before the first fill — and this system wants people placing hundreds of
orders, not one.

The alternative is client-side signing in the browser. It is technically
available: the frontend is Rust compiled to WebAssembly, so the same signing
code the CLI uses could run there. The cost is not the cryptography, it is the
key management around it — generation, storage, backup, recovery, and a signing
prompt on a path where latency is the point.

There is also an honest framing available that a real exchange does not have:
nothing here is worth anything. The assets are faucet-minted, the equities are
cash-settled synthetics against a mocked feed, and there is no withdrawal to
anywhere.

## Decision

**Identity is self-custodial. Trading is custodial.** The two are separated,
and the split is visible in the API rather than hidden behind one address.

A player signs in with their own BIP-39 mnemonic, derived at `m/44'/60'/0'/0/i`
to a secp256k1 key and an EIP-55 address, and proves control by signing a
server-issued challenge with EIP-191 `personal_sign`. The phrase never reaches
the server, and there is no recovery code, because the phrase is the recovery.
That also makes the login ordinary: the same mnemonic opens the same account in
MetaMask or other compatible EVM tooling. This prototype uses disposable test
wallets only.

The gateway then derives a *separate* trading key as
`HKDF-SHA256(server_secret, login_address)`. That key holds the test money and
signs orders, so placing a trade stays one click. It is never written down
anywhere, because it can always be recomputed from a secret the gateway already
has — which is strictly better than the encrypted-at-rest store this ADR
originally proposed, since there is no key material in storage to leak.

Client-side signing of *orders* remains an explicit non-goal for v1 and a named
follow-on in `docs/PLAN.md`.

This decision is recorded, rather than left implicit, because it is the one
architectural choice here that would be flatly wrong in a system holding real
value.

## Consequences

Joining costs one signature and trading costs none. The player loop — sign in,
claim, trade, get liquidated, appear on the leaderboard — has a wallet step only
at the door.

Order submission is a plain authenticated HTTP request. The gateway can retry a
failed submission, resubmit on nonce conflict, and batch cancellations, none of
which is possible when the signature is minted on the client.

The faucet is simple: one authority key mints, and every claim is one signed
transfer.

Against that:

**The gateway can sign anything, as anyone.** It holds every player's key. A
compromise of the gateway process, its host, or its storage is total: every
account, every position, all at once. There is no per-user blast radius. The
mitigation is that no value exists to steal, which is a property of the
deployment and not of the design.

**The server secret is now the single point of failure.** No key is stored, but
anyone holding `server_secret` can derive every trading key in the system. It
has moved from a storage problem to a secrets-management one, which is easier to
reason about and no less total when it fails.

**Rotating the server secret orphans every account.** The same login address
would derive a different trading account, with none of the old one's money.
There is deliberately no default value, and the runbook says not to rotate it.

**Losing the phrase loses the account.** The gateway never had it and cannot
help. That is now a property the player already understands from every other
EVM wallet, rather than a bespoke recovery code this product invented, which is
part of why the change was worth making.

**Two addresses is a concept the interface has to carry.** A player sees an
unfamiliar address holding their balance, and if that is not explained in one
glance they will assume they have been robbed. The account page leads with the
reassurance and puts the derivation second.

**The audit trail carries the weight.** With custody, "the user did this" is
only ever a claim by the gateway. The SQLite audit log is the sole record that
distinguishes a player's order from one the gateway invented, and it is not
cryptographic evidence of anything.

**Reversing the remaining custody is a breaking change.** Moving order signing
to the client changes the frozen API contract: placement would carry a signed
payload rather than fields, and the trading address would disappear in favour of
the login address. Both `cb-gateway` and `cb-web` change together. The login
half of this is already done, so that migration is now half as large as it was.

**The simulator cannot model self-custody risk.** Key loss, phishing and
signing-prompt fatigue are real parts of on-chain trading. This system has
designed them out, so it teaches nothing about them.
