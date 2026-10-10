# SuperFrontierLiquid — project front door.
#
# This is the Makefile you run from the repository root. It manages the whole
# stack: the chain, the gateway, the web app, the simulation and the docs.
#
# The deep, node-level targets live in rust/Makefile and are still there if you
# want them. Anything here that says "delegates" simply forwards to that file,
# so there is one implementation of each operation rather than two that drift.
#
#   make              show the menu
#   make start        start every backend, then print the URLs and how to log in
#   make start2       stress test, verification off: pre-verified orders straight into the matcher
#   make start3       stress test, verification on: signed orders over RPC, recovered on every core
#   make stop         stop everything
#   make open         print the URLs again
#
# There are no accounts to create. You log in with a BIP-39 recovery phrase
# and a wallet index; the browser derives your login key and signs a login
# challenge, and the same phrase opens the same address in any EVM wallet.
# The gateway then derives and holds the key that signs your orders (ADR 0003,
# custodial in v1). The operator ("admin") is the address one chosen phrase
# derives. See SECURITY.md before running this anywhere but your own machine.
#
#   make format       format the source code (the workspace and the web crate)
#
SHELL := /usr/bin/env bash

RUST_DIR := rust
RUST     := $(MAKE) --no-print-directory -C $(RUST_DIR)

# ---- where each piece listens -------------------------------------------
# Kept here as the single published list, because "which port is the gateway
# on" is the question every newcomer asks first.
NODE_RPC       ?= 127.0.0.1:7700
NODE_DASHBOARD ?= 127.0.0.1:8080
GATEWAY        ?= 127.0.0.1:8081
WEB_PORT       ?= 3000
DOCS_PORT      ?= 8082
# The oracle's scenario-control API. Nothing trades without the oracle: it is
# what gives every derivative market a mark price.
ORACLE_CONTROL ?= 127.0.0.1:8171

WEB_URL      := http://127.0.0.1:$(WEB_PORT)
GATEWAY_URL  := http://$(GATEWAY)
# The web app defaults to fixture data unless the page is told where the
# gateway is; this query string selects the live gateway and is remembered by
# the browser after the first visit.
LIVE         := ?gateway=live&api=$(GATEWAY_URL)/api/v1

# ---- who can log in ------------------------------------------------------
# Both are BIP-39 test vectors, published everywhere, so they hold nothing of
# value and never can. Override on the command line or in the environment:
#   make start ADMIN_MNEMONIC="..." ADMIN_INDEX=3
# The operator role is bound to the ADDRESS the admin phrase derives at
# ADMIN_INDEX (`make start` derives it and hands it to the gateway), so the
# admin page answers only to whoever holds that phrase. The user phrase is a
# suggestion: any valid phrase at any index is an account, empty until the
# faucet pays it.
ADMIN_MNEMONIC ?= abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about
ADMIN_INDEX    ?= 0
USER_MNEMONIC  ?= legal winner thank year wave sausage worth useful legal winner thank yellow
USER_INDEX     ?= 0

# ---- the massive test --------------------------------------------------
# `make start2` runs the ordinary stack and then points the RPC stress
# simulator at it: LOAD_WORKERS processes sharing LOAD_TRADERS funded accounts
# (the genesis funds 64), each signing real transactions and submitting them
# over the node's RPC for as long as the stack is up. LOAD_ORDERS_PCT of the
# submissions are spot orders — buyers and sellers, makers resting near the
# mid and takers crossing them, on BTC/USD and ETH/USD — and the rest are
# transfers. LOAD_TPS is the combined target rate; the node applies what it
# can and the rest waits in the simulator (bounded, so the accepted rate on
# the dashboard is the node's, not the simulator's).
#   make start2 LOAD_TPS=20000 LOAD_WORKERS=4 LOAD_TRADERS=64 LOAD_ORDERS_PCT=100
LOAD_TPS        ?= 20000
LOAD_TRADERS    ?= 64
LOAD_WORKERS    ?= 4
LOAD_ORDERS_PCT ?= 90

WEB_DIST     := $(RUST_DIR)/crates/cb-web/dist
WEB_PID      := $(RUST_DIR)/data/web.pid
WEB_LOG      := $(RUST_DIR)/data/web.log
CLIENT_BIN   := $(RUST_DIR)/target/release/cb-client
OPERATOR_URL := http://$(NODE_DASHBOARD)
DOCS_URL     := http://127.0.0.1:$(DOCS_PORT)
ORACLE_CONTROL_URL := http://$(ORACLE_CONTROL)

# The web app's routes. Named here so `make user`, `make admin` and
# `make faucet` have exactly one definition of where each page lives.
ROUTE_TRADE       := /trade
ROUTE_PORTFOLIO   := /portfolio
ROUTE_FAUCET      := /faucet
ROUTE_LEADERBOARD := /leaderboard
ROUTE_CHAIN       := /chain
ROUTE_USER        := /account
ROUTE_ADMIN       := /admin

# `open` on macOS, `xdg-open` on Linux, print-only if neither exists.
BROWSER := $(shell command -v open 2>/dev/null || command -v xdg-open 2>/dev/null)

.DEFAULT_GOAL := help
.PHONY: help setup doctor start _stack _require_stopped start2 start3 startbft loadbft stopbft load stop-sim stop up down restart status logs open login web-bg \
        dev web gateway node oracle \
        user admin faucet trade portfolio leaderboard chain \
        design docs \
        build test e2e e2e-fast e2e-clean web-e2e lint format format-check fmt check ci sim simgame bench orderbench \
        clean distclean

# ------------------------------------------------------------------ help
help: ## Show this menu
	@echo
	@echo "  SuperFrontierLiquid — an on-chain exchange you can play with."
	@echo
	@echo "  Run it:"
	@printf "    \033[36m%-18s\033[0m %s\n" "make start" "chain, oracle, gateway, web app and a trickle of buyers and sellers; print the URLs and logins"
	@printf "    \033[36m%-18s\033[0m %s\n" "make start2" "stress test, verification off: pre-verified buyers and sellers straight into the matcher"
	@printf "    \033[36m%-18s\033[0m %s\n" "make start3" "stress test, verification on: signed orders over RPC, recovered across every core"
	@printf "    \033[36m%-18s\033[0m %s\n" "make stop"  "stop everything"
	@printf "    \033[36m%-18s\033[0m %s\n" "make open"  "print the URLs and logins again"
	@printf "    \033[36m%-18s\033[0m %s\n" "make status" "what is running, and the chain head"
	@echo
	@echo "  Log in with a recovery phrase and a wallet index — there are no accounts to create."
	@printf "    \033[36m%-18s\033[0m %s\n" "user"  "any BIP-39 phrase, any index; claim from the faucet first"
	@printf "    \033[36m%-18s\033[0m %s\n" "admin" "the phrase in ADMIN_MNEMONIC at ADMIN_INDEX (make start prints it)"
	@echo
	@echo "  Pages:"
	@printf "    \033[36m%-18s\033[0m %s\n" "make faucet"      "open the faucet — claim test money"
	@printf "    \033[36m%-18s\033[0m %s\n" "make trade"       "open the trading terminal"
	@printf "    \033[36m%-18s\033[0m %s\n" "make user"        "open the user account page"
	@printf "    \033[36m%-18s\033[0m %s\n" "make admin"       "open the admin dashboard"
	@printf "    \033[36m%-18s\033[0m %s\n" "make portfolio"   "open the portfolio page"
	@printf "    \033[36m%-18s\033[0m %s\n" "make leaderboard" "open the leaderboard"
	@printf "    \033[36m%-18s\033[0m %s\n" "make chain"       "open the chain explorer"
	@printf "    \033[36m%-18s\033[0m %s\n" "make design"      "open the design system reference"
	@echo
	@echo "  Everything else:"
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
	  | grep -vE '^(help|setup|start|start2|start3|stop|open|status|faucet|trade|user|admin|portfolio|leaderboard|chain|design):' \
	  | awk 'BEGIN {FS = ":.*?## "}; {printf "    \033[36m%-18s\033[0m %s\n", $$1, $$2}'
	@echo
	@echo "  Node-level targets (cluster, benchmarks, perps): make -C rust help"
	@echo

# ------------------------------------------------------------------ setup
doctor: ## Check that every required tool is installed
	@echo "checking prerequisites"
	@ok=1; \
	for t in cargo rustc capnp; do \
	  if command -v $$t >/dev/null 2>&1; then \
	    printf "  \033[32m ok \033[0m %-8s %s\n" "$$t" "$$($$t --version 2>&1 | head -1)"; \
	  else \
	    printf "  \033[31mmiss\033[0m %-8s %s\n" "$$t" "required"; ok=0; \
	  fi; \
	done; \
	if command -v trunk >/dev/null 2>&1; then \
	  printf "  \033[32m ok \033[0m %-8s %s\n" "trunk" "$$(trunk --version)"; \
	else \
	  printf "  \033[33mmiss\033[0m %-8s %s\n" "trunk" "needed for the web app: cargo install trunk"; \
	fi; \
	if rustup target list --installed 2>/dev/null | grep -q wasm32-unknown-unknown; then \
	  printf "  \033[32m ok \033[0m %-8s %s\n" "wasm32" "target installed"; \
	else \
	  printf "  \033[33mmiss\033[0m %-8s %s\n" "wasm32" "rustup target add wasm32-unknown-unknown"; \
	fi; \
	echo; \
	if [ $$ok -eq 0 ]; then \
	  echo "  capnp is the usual missing one:  brew install capnp   (or apt install capnproto)"; \
	  exit 1; \
	fi; \
	echo "  prerequisites satisfied"

setup: doctor ## One-time setup: tools, rust/.env, and genesis
	@$(RUST) env
	@$(RUST) init
	@echo
	@echo "  setup complete. now run:  make start"

# ------------------------------------------------------------------ run
node: ## Start just the chain node in the background
	@$(RUST) start

gateway: ## Start just the REST/WebSocket gateway in the background
	@$(RUST) gateway

oracle: ## Start just the oracle price publisher (without it, no derivative market has a mark price)
	@$(RUST) oracle

web: ## Serve the web app in the foreground with hot reload
	@$(RUST) web-serve WEB_PORT=$(WEB_PORT)

# `make start` is the whole exchange: the chain, the price oracle (without it
# no derivative market has a mark price and none accepts an order), the
# gateway the browser talks to, and the web app. Each piece is idempotent —
# already running means "left alone" — so `make start` twice is harmless.
#
# The operator role is bound to an address, never to a name somebody can
# claim first. The address is derived here from ADMIN_MNEMONIC at ADMIN_INDEX
# with the same BIP-44 path the browser uses, and handed to the gateway as
# GATEWAY_ADMIN_ADDRESS; the gateway's launch reads it from the environment
# unless rust/.env pins its own.
# `make start` is the stack for using and testing: every backend, the web
# app, and a default buyer and seller. The buyer and seller are cb-sim
# workers the GATEWAY launches — the admin page's simulation switch, whose
# start-up position is SIM_MODE (off, on, max) — so the operator can turn
# them off, on or up to a full stress test from the browser, and nothing
# else has to be told. `make start SIM_MODE=off` boots a quiet exchange.
SIM_MODE ?= on
start: ## Start every backend and the web app with a default buyer and seller (SIM_MODE=off|on|max), then print the URLs and how to log in
	@$(RUST) stop-sim >/dev/null 2>&1 || true
	@$(MAKE) --no-print-directory _stack NODE_EXTRA_ARGS="" SIM_MODE=$(SIM_MODE)
	@printf "  \033[1msimulation\033[0m  switch starts %s; move it from the admin page (off = no default buyer or seller, on = a trickle, max = the stress test)\n" "$(SIM_MODE)"
	@echo

# The stack, with whatever extra flags the node should carry. `start` passes
# none; `start2` passes the in-process order generator.
_stack:
	@$(RUST) check-env
	@$(RUST) start NODE_EXTRA_ARGS="$(NODE_EXTRA_ARGS)"
	@$(RUST) oracle || echo "  oracle not started — derivative markets will have no mark price"
	@$(RUST) build-client >/dev/null
	@addr=$$($(CLIENT_BIN) addr --mnemonic "$(ADMIN_MNEMONIC)" --index $(ADMIN_INDEX)) || { echo "could not derive the admin address"; exit 1; }; \
	GATEWAY_ADMIN_ADDRESS="$$addr" $(RUST) gateway-bg SIM_MODE=$(SIM_MODE) || echo "  gateway not started (see message above)"
	@$(MAKE) --no-print-directory web-bg
	@$(MAKE) --no-print-directory open

up: start ## Alias for start

# `make start2` is `make start` under load. The stack is exactly the one
# `make start` runs — same node flags, same genesis, gateway and web app
# included — with the RPC stress simulator (cb-sim) aimed at it afterwards.
# The simulator signs real transactions and submits them over the same
# Cap'n Proto RPC a wallet would use, so what the dashboard shows is the
# end-to-end path: signature recovery, mempool, matching, block, state root.
# Buyers and sellers place spot orders (the ORDERS, SIDE and MARKET tiles),
# transfers fill in the rest (the TRANSACTIONS tile). `make stop` stops it
# with the rest of the stack.
# `make start2` is the pure stress test. The premise: order validation is a
# risk engine's job, in front of this node, so the matcher may treat what it
# receives as already checked. The node therefore runs its in-process
# generator — STRESS_TRADERS funded accounts, buyers and sellers, makers and
# takers on BTC/USD and ETH/USD — signing on STRESS_WORKERS threads and
# handing every order to the sequencer pre-verified, sender attached. No RPC,
# no signature recovery: what the dashboard shows is the block loop and the
# matcher, with nothing in front of them. The gateway, oracle and web app
# still run, so the terminal shows the books moving.
#
# `make load` is still there for the other test — real signed orders over
# RPC from cb-sim, which exercises admission as a client would.
STRESS_TRADERS   ?= 64
STRESS_WORKERS   ?= 6
STRESS_INTERVAL  ?= 50
STRESS_BLOCK_TXS ?= 50000
# Every node keeps the last KEEP_BLOCKS block bodies and prunes the rest;
# state, the tries and the head are never pruned. The default is 10, which is
# what a node needs to serve the present. The stress tiers keep more so a run
# can be inspected afterwards.
STRESS_KEEP_BLOCKS ?= 2000
STRESS_NODE_ARGS  = --benchmark --bench-workload orders --bench-traders $(STRESS_TRADERS) --bench-workers $(STRESS_WORKERS) --max-block-txs $(STRESS_BLOCK_TXS)
# The stress tiers start the node with their own flags, so a node that is
# already running — `make start`'s, say — would be silently kept as it is and
# the stress banner would describe a test that is not happening. Refuse.
_require_stopped:
	@if [ -f $(RUST_DIR)/data/sequencer.pid ] && kill -0 $$(cat $(RUST_DIR)/data/sequencer.pid) 2>/dev/null; then \
		echo "a node is already running (pid $$(cat $(RUST_DIR)/data/sequencer.pid)); this target starts one with its own flags."; \
		echo "run  make stop  first."; exit 1; \
	fi

start2: _require_stopped ## Start the stack with the in-process order generator: pre-verified buyers and sellers straight into the matcher (pure stress test)
	@rm -rf $(RUST_DIR)/data
	@$(RUST) init GENESIS=genesis-stress.jsonl >/dev/null
	@$(MAKE) --no-print-directory _stack NODE_EXTRA_ARGS="$(STRESS_NODE_ARGS)" KEEP_BLOCKS=$(STRESS_KEEP_BLOCKS) INTERVAL_MS=$(STRESS_INTERVAL) SIM_MODE=off
	@echo
	@printf "  \033[1mstress\033[0m  (in-process order generator; nothing in front of the matcher)\n"
	@printf "    %-14s %s\n" "chain"       "a fresh one from genesis-stress.jsonl (rust/data is wiped): every trader holds 1e32 of every asset, nothing is refused for balance"
	@printf "    %-14s %s\n" "traders"     "$(STRESS_TRADERS) funded accounts, even = makers (Gtc), odd = takers (Ioc), both buy and sell"
	@printf "    %-14s %s\n" "signing"     "$(STRESS_WORKERS) worker threads sign; the node recovers no signatures"
	@printf "    %-14s %s\n" "blocks"      "every $(STRESS_INTERVAL) ms or $(STRESS_BLOCK_TXS) transactions, whichever first"
	@printf "    %-14s %s\n" "watch"       "$(OPERATOR_URL)  — ORDERS / S SUBMITTED and APPLIED, buy · sell, BTC · ETH, CHANNEL DEPTH"
	@printf "    %-14s %s\n" "tune"        "make start2 STRESS_WORKERS=8 STRESS_INTERVAL=50 STRESS_BLOCK_TXS=50000"
	@printf "    %-14s %s\n" "no signing"  "CB_BENCH_UNSIGNED=1 make start2   (placeholder signatures: the block loop alone, nothing spent signing)"
	@printf "    %-14s %s\n" "over RPC"    "make start && make load   (real signed orders through admission instead)"
	@printf "    %-14s %s\n" "stop"        "make stop"
	@echo

load: ## Aim the RPC stress simulator at the running chain (LOAD_TPS, LOAD_WORKERS, LOAD_TRADERS, LOAD_ORDERS_PCT)
	@echo
	@printf "  \033[1mload\033[0m  (buyers, sellers and transfers over RPC; the chain dashboard shows them land)\n"
	@$(RUST) stop-sim >/dev/null 2>&1 || true
	@set -o pipefail; $(RUST) startsimulation SIM_TPS=$(LOAD_TPS) SIM_WORKERS=$(LOAD_WORKERS) SIM_TRADERS=$(LOAD_TRADERS) SIM_ORDERS_PCT=$(LOAD_ORDERS_PCT) | sed 's/^/    /'
	@echo
	@printf "    %-14s %s\n" "watch"       "$(OPERATOR_URL)  — TRANSACTIONS / S, ORDERS / S, buy · sell, BTC · ETH, REJECTED"
	@printf "    %-14s %s\n" "sim stats"   "make status      (accepted / rejected / orders / buys / sells as the simulator counts them)"
	@printf "    %-14s %s\n" "sim log"     "$(RUST_DIR)/data/sim.log  (one per worker: sim-1.log, sim-2.log, ...)"
	@printf "    %-14s %s\n" "tune"        "make start2 LOAD_TPS=20000 LOAD_WORKERS=4 LOAD_TRADERS=64 LOAD_ORDERS_PCT=100"
	@printf "    %-14s %s\n" ""            "LOAD_TPS is the combined target; what the node cannot apply waits in the simulator."
	@printf "    %-14s %s\n" ""            "ETH/USD is priced synthetically (the genesis cannot express a real ETH price)."
	@printf "    %-14s %s\n" "stop"        "make stop        (stops the simulator with everything else)"
	@echo

# `make start3` is the stress test with signature verification on. Every
# order is signed by a cb-sim worker and recovered by the node — on a pool
# spread across every core, admission kept in arrival order (`--verify-threads`,
# see cb_rpc::verify) — so what the dashboard shows is the whole path a real
# client takes. On one machine the workers' signing and the node's recovery
# share the same cores; two machines would show the node's side alone.
STRESS3_WORKERS ?= 6
STRESS3_TPS     ?= 40000
start3: _require_stopped ## Start the stack, then stress it with signed orders over RPC, signatures verified across every core
	@$(MAKE) --no-print-directory _stack NODE_EXTRA_ARGS="--max-block-txs $(STRESS_BLOCK_TXS)" KEEP_BLOCKS=$(STRESS_KEEP_BLOCKS) INTERVAL_MS=$(STRESS_INTERVAL) SIM_MODE=off
	@$(MAKE) --no-print-directory load LOAD_WORKERS=$(STRESS3_WORKERS) LOAD_TPS=$(STRESS3_TPS)
	@printf "    %-14s %s\n" "verifying"   "every signature, recovered on a pool across all cores (cb-node --verify-threads, 0 = all)"
	@printf "    %-14s %s\n" "compare"     "make start2 is the same stress with verification off; make start is the trickle"
	@echo

# `make startbft` is the exchange on a BFT validator cluster (ADR 0008):
# BFT_VALIDATORS `cb-node validator` processes on this machine, each with its
# own data directory under rust/data-bft, ordering blocks by quorum. The
# gateway, oracle and web app talk to validator 0 on the same port the single
# node uses, so nothing else changes. `make loadbft` aims the RPC stress
# simulator at it; `make stopbft` stops the validators and the rest.
BFT_VALIDATORS ?= 4
BFT_LOAD_TPS ?= 40000
BFT_LOAD_WORKERS ?= 6
BFT_LOAD_TRADERS ?= 64
BFT_LOAD_INFLIGHT ?= 1500
startbft: _require_stopped ## Start the stack on a fresh BFT cluster of BFT_VALIDATORS validators (ADR 0008); rust/data-bft is wiped
	@$(RUST) check-env
	@$(RUST) build >/dev/null
	@# A BFT chain is one scheme from genesis, and this target's job is a
	@# fresh one, as `make start2` wipes rust/data. Restarting all four
	@# validators on the previous run's state produced no blocks at all while
	@# every submission was refused for its nonce. One validator at a time can
	@# still be restarted in place: rust/scripts/bft-cluster.sh restart N.
	@rm -rf $(RUST_DIR)/data-bft
	@echo "wiped $(RUST_DIR)/data-bft: the cluster starts from genesis"
	@$(RUST_DIR)/scripts/bft-cluster.sh start $(BFT_VALIDATORS)
	@$(RUST) oracle || echo "  oracle not started — derivative markets will have no mark price"
	@$(RUST) build-client >/dev/null
	@addr=$$($(CLIENT_BIN) addr --mnemonic "$(ADMIN_MNEMONIC)" --index $(ADMIN_INDEX)) || { echo "could not derive the admin address"; exit 1; }; \
	GATEWAY_ADMIN_ADDRESS="$$addr" $(RUST) gateway-bg SIM_MODE=off || echo "  gateway not started (see message above)"
	@$(MAKE) --no-print-directory web-bg
	@$(MAKE) --no-print-directory open
	@printf "  \033[1mvalidators\033[0m  %s on this machine; dashboards http://127.0.0.1:18080 .. 1808%d; status: rust/scripts/bft-cluster.sh status\n" "$(BFT_VALIDATORS)" $$(( $(BFT_VALIDATORS) - 1 ))
	@printf "    %-14s %s\n" "load"        "make loadbft   (BFT_LOAD_TPS=$(BFT_LOAD_TPS) signed orders/s over RPC at validator 0, forwarded to the rest)"
	@printf "    %-14s %s\n" "kill one"    "rust/scripts/bft-cluster.sh kill 1; ... restart 1   (the chain continues with three; the fourth rejoins)"
	@printf "    %-14s %s\n" "watch"       "grep 'consensus status' rust/data-bft/node0.log   (rounds, blocks, commit latency p50/p99)"
	@echo

loadbft: ## Aim the RPC stress simulator at the BFT cluster (BFT_LOAD_TPS, BFT_LOAD_WORKERS, BFT_LOAD_TRADERS)
	@$(RUST) stop-sim >/dev/null 2>&1 || true
	@$(RUST) startsimulation SIM_TPS=$(BFT_LOAD_TPS) SIM_WORKERS=$(BFT_LOAD_WORKERS) SIM_TRADERS=$(BFT_LOAD_TRADERS) SIM_INFLIGHT=$(BFT_LOAD_INFLIGHT) SIM_ORDERS_PCT=100 | sed 's/^/    /'

stopbft: ## Stop the BFT validators and the rest of the stack
	@$(RUST_DIR)/scripts/bft-cluster.sh stop
	@$(MAKE) --no-print-directory stop

# The web app is a static WebAssembly bundle. Build it with trunk when trunk
# is installed (a release build, a couple of megabytes); otherwise serve the
# bundle already in crates/cb-web/dist, which is what a checkout that has
# never run trunk still has. Served by a small loopback server that answers
# every route with index.html, which a client-side router needs and
# `python3 -m http.server` does not do.
web-bg: ## Build (if trunk is installed) and serve the web app in the background
	@mkdir -p $(RUST_DIR)/data
	@if [ -f $(WEB_PID) ] && kill -0 $$(cat $(WEB_PID)) 2>/dev/null; then \
		echo "web app already serving (pid $$(cat $(WEB_PID)))"; exit 0; \
	fi; \
	if command -v trunk >/dev/null 2>&1; then \
		echo "building the web app (trunk build --release)"; \
		(cd $(RUST_DIR)/crates/cb-web && trunk build --release >> ../../data/web.log 2>&1) \
		  || { echo "  web build failed; the end of $(WEB_LOG) says why"; tail -5 $(WEB_LOG) | sed 's/^/    /'; exit 1; }; \
	elif [ ! -f $(WEB_DIST)/index.html ]; then \
		echo "  no web bundle in $(WEB_DIST) and trunk is not installed: cargo install trunk"; exit 1; \
	else \
		echo "  trunk not installed; serving the existing bundle in $(WEB_DIST) (cargo install trunk to rebuild it)"; \
	fi; \
	nohup python3 $(RUST_DIR)/scripts/serve_web.py $(WEB_DIST) $(WEB_PORT) >> $(WEB_LOG) 2>&1 & \
	echo $$! > $(WEB_PID); \
	sleep 1; \
	if kill -0 $$(cat $(WEB_PID)) 2>/dev/null; then \
		echo "web app serving (pid $$(cat $(WEB_PID)))  $(WEB_URL)"; \
	else \
		echo "web server exited at once; the end of $(WEB_LOG) says why:"; tail -5 $(WEB_LOG) | sed 's/^/    /'; \
		rm -f $(WEB_PID); exit 1; \
	fi

dev: ## Start chain + gateway, then the web app in the foreground with hot reload (Ctrl-C to stop)
	@$(RUST) dev WEB_PORT=$(WEB_PORT)

stop: ## Stop everything: node, oracle, gateway, web app, cluster, simulator
	@$(RUST_DIR)/scripts/bft-cluster.sh stop >/dev/null 2>&1 || true
	@$(RUST) stop
	@if [ -f $(WEB_PID) ]; then kill $$(cat $(WEB_PID)) 2>/dev/null || true; rm -f $(WEB_PID); echo "stopped web app"; fi
	@pkill -f "serve_web.py" 2>/dev/null || true
	@pkill -f "trunk serve" 2>/dev/null || true
	@pkill -f "http.server $(DOCS_PORT)" 2>/dev/null || true
	@echo "stack stopped"

down: stop ## Alias for stop

restart: stop start ## Stop everything, then start it again

status: ## Show what is running and where the chain head is
	@$(RUST) status

logs: ## Tail the node log
	@$(RUST) logs

open: ## Print every URL the running stack serves, and how to log in
	@echo
	@printf "  \033[1mdashboards\033[0m  (open in a browser; replace 127.0.0.1 with this machine's address from elsewhere)\n"
	@printf "    %-14s %s\n" "chain"       "$(OPERATOR_URL)"
	@printf "    %-14s %s\n" ""            "the node's operator view: height, blocks, books, positions, oracle prices, insurance fund. No login."
	@printf "    %-14s %s\n" ""            "JSON: $(OPERATOR_URL)/api/status   $(OPERATOR_URL)/api/perps/oracle"
	@printf "    %-14s %s\n" "admin"       "$(WEB_URL)$(ROUTE_ADMIN)$(LIVE)"
	@printf "    %-14s %s\n" ""            "the exchange operator UI: the simulation switch (off / on / max), users, halts, faucet, risk. Log in with the admin phrase below."
	@printf "    %-14s %s\n" "user"        "$(WEB_URL)/$(LIVE)"
	@printf "    %-14s %s\n" ""            "the trading app. Log in with any phrase, claim from the faucet, trade."
	@echo
	@printf "  \033[1mlog in\033[0m  (recovery phrase + wallet index; the browser derives the key, nothing is sent)\n"
	@printf "    %-14s %s\n" "user"        "any BIP-39 phrase at any index — or, to try it:"
	@printf "    %-14s %s\n" ""            "phrase  $(USER_MNEMONIC)"
	@printf "    %-14s %s\n" ""            "index   $(USER_INDEX)      then claim test money at $(WEB_URL)$(ROUTE_FAUCET)"
	@printf "    %-14s %s\n" "admin"       "phrase  $(ADMIN_MNEMONIC)"
	@printf "    %-14s %s\n" ""            "index   $(ADMIN_INDEX)      the operator role is bound to the address this derives"
	@echo
	@printf "  \033[1mpages\033[0m\n"
	@printf "    %-14s %s\n" "faucet"      "$(WEB_URL)$(ROUTE_FAUCET)"
	@printf "    %-14s %s\n" "trade"       "$(WEB_URL)$(ROUTE_TRADE)"
	@printf "    %-14s %s\n" "portfolio"   "$(WEB_URL)$(ROUTE_PORTFOLIO)"
	@printf "    %-14s %s\n" "account"     "$(WEB_URL)$(ROUTE_USER)"
	@printf "    %-14s %s\n" "leaderboard" "$(WEB_URL)$(ROUTE_LEADERBOARD)"
	@printf "    %-14s %s\n" "chain"       "$(WEB_URL)$(ROUTE_CHAIN)"
	@echo
	@printf "  \033[1mservices\033[0m\n"
	@printf "    %-14s %s\n" "gateway API" "$(GATEWAY_URL)/api/v1"
	@printf "    %-14s %s\n" "gateway WS"  "ws://$(GATEWAY)/ws"
	@printf "    %-14s %s\n" "oracle"      "$(ORACLE_CONTROL_URL)/scenario  (price feed control)"
	@printf "    %-14s %s\n" "node RPC"    "$(NODE_RPC)  (Cap'n Proto, not HTTP)"
	@echo
	@echo "  make stop  when you are done"
	@echo

login: open ## Alias for open

# ------------------------------------------------------------------ pages
# Each opens a page in a browser, falling back to printing the URL.
define open_url
@if [ -n "$(BROWSER)" ]; then $(BROWSER) "$(1)" >/dev/null 2>&1 || echo "  $(1)"; else echo "  $(1)"; fi
@echo "  $(1)"
endef

faucet: ## Open the faucet page, where you claim test money
	$(call open_url,$(WEB_URL)$(ROUTE_FAUCET)$(LIVE))

trade: ## Open the trading terminal
	$(call open_url,$(WEB_URL)$(ROUTE_TRADE)$(LIVE))

user: ## Open the user account page
	$(call open_url,$(WEB_URL)$(ROUTE_USER)$(LIVE))

admin: ## Open the admin dashboard
	$(call open_url,$(WEB_URL)$(ROUTE_ADMIN)$(LIVE))

portfolio: ## Open the portfolio page
	$(call open_url,$(WEB_URL)$(ROUTE_PORTFOLIO)$(LIVE))

leaderboard: ## Open the leaderboard
	$(call open_url,$(WEB_URL)$(ROUTE_LEADERBOARD)$(LIVE))

chain: ## Open the chain explorer
	$(call open_url,$(WEB_URL)$(ROUTE_CHAIN)$(LIVE))

design: ## Serve and open the design system reference
	@pkill -f "http.server $(DOCS_PORT)" 2>/dev/null || true
	@cd docs/design && nohup python3 -m http.server $(DOCS_PORT) --bind 127.0.0.1 >/dev/null 2>&1 &
	@sleep 1
	$(call open_url,$(DOCS_URL)/reference.html)

docs: ## List the documentation
	@echo
	@printf "  %-28s %s\n" "README.md"              "what this is, and how to run it"
	@printf "  %-28s %s\n" "CONTRIBUTING.md"        "build, test, lint, commit conventions"
	@printf "  %-28s %s\n" "docs/STATUS.md"         "where it stands, what is open, what to do next"
	@printf "  %-28s %s\n" "docs/PLAN.md"           "architecture, milestones, invariants"
	@printf "  %-28s %s\n" "docs/api-contract.md"   "REST + WebSocket contract (frozen)"
	@printf "  %-28s %s\n" "docs/architecture.md"   "a transaction from click to state root"
	@printf "  %-28s %s\n" "docs/runbook.md"        "operator guide and failure modes"
	@printf "  %-28s %s\n" "docs/design-system.md"  "the design language"
	@printf "  %-28s %s\n" "docs/adr/"              "why each big decision was made"
	@echo

# ------------------------------------------------------------------ build and test
build: ## Build everything, workspace and web app
	@$(RUST) build-all
	@$(RUST) web-build

test: ## Run the full test suite
	@$(RUST) testall

# The end-to-end suite starts the real node and gateway itself, on ports and in
# temporary directories it picks, so it needs nothing running and collides with
# nothing that is. Delegates to rust/Makefile.
e2e: ## End-to-end: boot the real stack and drive it like a browser would
	@$(RUST) e2e

e2e-fast: ## The end-to-end checks that need no running stack (shipped genesis, env, ws path)
	@$(RUST) e2e-fast

e2e-clean: ## Clear any node or gateway a killed end-to-end run left behind
	@$(RUST) e2e-clean

# The browser suite drives the stack `make start` runs — the real chain, the
# real gateway, the real WebAssembly bundle — with Playwright. It needs Node.
web-e2e: ## Browser end-to-end with Playwright against the running stack (make start first)
	@curl -fsS --max-time 2 $(GATEWAY_URL)/health >/dev/null || { echo "the stack is not up: run  make start  first"; exit 1; }
	@cd web-e2e && { [ -d node_modules ] || npm install --no-audit --no-fund; } && npx playwright install chromium >/dev/null && \
		E2E_BASE_URL=$(WEB_URL) E2E_API=$(GATEWAY_URL)/api/v1 ADMIN_MNEMONIC="$(ADMIN_MNEMONIC)" USER_MNEMONIC="$(USER_MNEMONIC)" npx playwright test $(PW_ARGS)

check: ## Fast type-check
	@$(RUST) check

lint: ## Clippy over everything, warnings are errors
	@$(RUST) lint

format: ## Format the source code: the Rust workspace and the web crate (cb-web)
	@$(RUST) fmt

format-check: ## Check formatting without writing anything (what CI runs)
	@$(RUST) fmt-check

fmt: format ## Alias for format

# What .github/workflows/ci.yml runs, so a contributor can run the same gate
# before pushing: formatting, clippy with warnings as errors, and every
# workspace test.
ci: format-check lint ## Formatting, clippy and the workspace tests, as CI runs them
	@cd $(RUST_DIR) && cargo test --workspace

# ------------------------------------------------------------------ simulation
sim: simgame ## Alias for simgame

simgame: ## Run the bot-driven exchange simulation
	@$(RUST) simgame

bench: ## Max-throughput benchmark (TRANSACTIONS — the generator signs transfers, not orders)
	@$(RUST) startbenchmark

orderbench: ## Order-matching throughput: submitted, applied and MATCHED, reported separately
	@$(RUST) orderbench

# ------------------------------------------------------------------ cleanup
clean: ## Stop everything and wipe chain data
	@$(RUST) clean

distclean: clean ## Also remove build artefacts
	@rm -rf $(RUST_DIR)/target $(RUST_DIR)/crates/cb-web/dist
	@echo "build artefacts removed"
