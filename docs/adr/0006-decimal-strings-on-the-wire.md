# ADR 0006 — Decimal strings on the wire, not JSON numbers

Status: accepted. Date: 2026-09-18.

## Context

On-chain quantities are unsigned 128-bit integers at fixed scales. Prices for
derivatives are carried at 1e8 (`px_q8`). Collateral and notional are micro-USD,
1e6. Derivative quantities are contract units at 1e8. Spot quantities are in the
asset's own base units, so one ETH is 1e18 and one synthetic AAPL share is 1e6.
Funding rates are signed, at 1e9.

A JSON number is an IEEE-754 double in every mainstream parser. Doubles are
exact up to 2^53, about 9.0e15. One ETH, in base units, is 1e18. A single
ETH balance therefore cannot survive a JSON number, and values well below that
threshold lose their last digits silently — no error, no warning, just a
balance that is wrong in the cents.

In a trading interface, the failure is worse than wrong output. A user sees a
size, submits it, and the value that arrives at the exchange is not the value
they saw.

## Decision

Every price, quantity, balance, notional, fee, rate and PnL crosses the gateway
boundary as a JSON **string**: `"64250.50"`, `"1.00000000"`, `"0.000125"`.

The gateway owns the conversion in both directions, between these strings and
the on-chain fixed-point integers. Both sides use `rust_decimal`. The rule is
written into `docs/api-contract.md` §1, which is frozen.

Timestamps are RFC 3339 UTC strings; durations are integer seconds. Those two
are ordinary JSON scalars, because neither has a precision problem.

## Consequences

Values are exact end to end. A quantity typed into the order ticket is the
quantity the executor sees, digit for digit.

The conversion has exactly one home. Scale conversion is fiddly — the same
"quantity" is 1e8 contract units on a perpetual and 1e18 base units on ETH spot
— and confining it to the gateway means there is one place to read, and one
place to test.

Nothing in the browser needs to know about scales at all. The frontend receives
human-scale decimals and can render them directly.

The costs:

**Every consumer must parse.** No client can sort, compare or sum a field
without first converting it. A sort that forgets will order `"9"` after `"10"`,
and that bug looks like a rendering glitch rather than a type error.

**The type system stops helping at the boundary.** A JSON schema can say
"string"; it cannot say "decimal with at most 8 fractional digits". Validation
that a number type would have given free is now the gateway's job, and a
malformed decimal is a runtime error rather than a parse error.

**Payloads are bigger.** An order book with 200 levels per side ships 800
quoted strings. Over a WebSocket at several updates per second this is
measurable, and it is the price of correctness.

**It does not fix the on-chain scales, and it can hide them.** Behind the
strings, the executor still hard-codes its divisors: perpetual notional divides
by 1e10 on the assumption of an 8-decimal underlying, and spot notional divides
by 1e8 without consulting the asset's declared decimals. A clean decimal string
on the wire can therefore be a faithful rendering of a wrong number. The
string convention protects transport, not arithmetic.

**Two conventions now exist in one system.** Internally, code passes `q8`,
`q9` and micro-USD integers; externally, everything is a decimal string. A
contributor moving between the gateway and the engine has to switch mental
models at exactly one line, and that line is not marked by a type.
