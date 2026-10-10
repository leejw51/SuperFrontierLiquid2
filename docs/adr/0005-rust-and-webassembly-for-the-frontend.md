# ADR 0005 — Rust and WebAssembly for the frontend

Status: accepted. Date: 2026-09-18.

## Context

The frontend is a trading terminal: an order book that updates several times a
second, a depth chart, candles on a canvas, order entry, positions with live
PnL. It talks to the gateway over JSON and a WebSocket.

The numbers it handles are the hard part. Prices, quantities, balances and PnL
cross the wire as decimal strings precisely because they do not fit a JavaScript
number (see ADR 0006). A TypeScript client would immediately need a decimal
library, and would need to agree — exactly, including rounding direction — with
the Rust that produced the values.

The team writing the backend is a Rust team. The types the UI renders are
already defined in Rust, in `cb-types` and in the gateway's response structs.

## Decision

`cb-web` is a Rust crate compiled to `wasm32-unknown-unknown`, using Leptos in
client-side-rendering mode, built and served by Trunk. `rust-toolchain.toml`
pins the wasm target so a fresh checkout can build it.

`cb-web` is deliberately **excluded** from the Cargo workspace. It needs its own
release profile (`opt-level = "z"`, `lto = true`, `codegen-units = 1`,
`panic = "abort"`) and its own lockfile, neither of which should apply to the
node.

Canvas rendering goes through `web-sys`; `rust_decimal` parses the decimal
strings on the way in, the same crate the gateway uses on the way out.

## Consequences

One language across the stack. A type shared between gateway and frontend can
be genuinely shared rather than hand-mirrored, and the decimal semantics on both
ends come from the same crate.

Compile-time exhaustiveness reaches the UI. Adding an order status or a market
kind fails the frontend build instead of rendering `undefined`.

Trunk makes the build one command (`make web-build`) and the dev loop another
(`make web-serve`), with hot reload.

The costs:

**The bundle is large and the app has not been written yet.** The current
`cb-web` is a build-chain spike — it mounts one paragraph — and its release
WebAssembly module is already 665 KB after `wasm-opt -Oz`, plus 25 KB of
JavaScript glue. That is the floor, not the ceiling. An equivalent JavaScript
app would start near zero and grow.

**Two more tools in the toolchain.** Contributors need `trunk`, and Trunk needs
`wasm-bindgen` and `wasm-opt`, which it fetches on first use. That is a slow,
network-dependent first build on top of an already slow first build.

**It is outside the workspace, so workspace commands miss it.** `cargo build
--workspace`, `cargo test --workspace`, `cargo clippy --workspace` do not see
`cb-web`. The Makefile's `fmt`, `fmt-check` and `web-build` targets run it
separately; anything that forgets to will produce a green CI and a broken
frontend.

**The ecosystem is thinner.** Charting, date handling, component libraries and
accessibility helpers are all richer in the JavaScript world. Much of what this
UI needs will be written by hand against `<canvas>`.

**Debugging is worse.** A panic in wasm is a stack trace only because
`console_error_panic_hook` is installed; browser devtools understand the DOM but
not Leptos's reactive graph.

**It narrows the contributor pool.** Frontend engineers who would happily pick
up a React codebase will not pick this one up. For an open-source project, that
is a real cost, paid continuously.
