# SuperFrontierLiquid Makefile
#
# Run `make` (or `make help`) for the menu. Every target carries a `## ` line
# and appears there.
#
# Targets whose help line says UNVERIFIED drive code that does not exist yet —
# `cb-gateway` and `cb-simgame` are still stub crates. Those targets fail with
# an explanation rather than a wall of compiler output.
SHELL := /usr/bin/env bash

# ---- genesis environment ----------------------------------------------------
# `genesis.jsonl` and `genesis-perps.jsonl` contain `${VAR}` tokens. cb-genesis
# substitutes them from the process environment and auto-loads a sibling `.env`
# first; a real shell variable always wins over the file. A referenced variable
# that is set nowhere is a hard error, by design.
ENV_FILE     := .env
ENV_EXAMPLE  := .env.example

# ---- single-node config ----
DATA_DIR     ?= ./data
GENESIS      ?= genesis-full.jsonl
RPC_LISTEN   ?= 127.0.0.1:7700
DASH_LISTEN  ?= 127.0.0.1:8080
INTERVAL_MS  ?= 100
# Block bodies the single node keeps. The gateway follows the chain by
# streaming bodies, and at ten blocks a second a keep-window of ten gives it
# one second to fall behind before its stream breaks and re-anchors at the
# head, losing the blocks in between. Five minutes is room to breathe; state,
# tries and the head are never pruned whatever this says.
KEEP_BLOCKS  ?= 3000
# Extra flags for the node `start` launches. The root Makefile's `start2`
# passes the in-process order generator through here.
NODE_EXTRA_ARGS ?=

# Every cargo test phase opens its databases with a small RocksDB open-file
# cap. A node's default is 16,384 — a chain has thousands of SST files and
# anything less thrashes — but the suite opens dozens of databases at once and
# has no such chain, so the product of the two is descriptors spent for
# nothing. `make testall` already warned about `ulimit -n`; this is the same
# concern, bounded.
export CB_MAX_OPEN_FILES ?= 64
PID_FILE     := $(DATA_DIR)/sequencer.pid
LOG_FILE     := $(DATA_DIR)/sequencer.log
NODE_BIN     := target/release/cb-node
SIM_BIN      := target/release/cb-sim
CLIENT_BIN   := target/release/cb-client
SIM_PID_FILE := $(DATA_DIR)/sim.pid
SIM_LOG_FILE := $(DATA_DIR)/sim.log
SIM_TRADERS  ?= 32
# Share of the simulator's submissions that are spot orders (buyers and
# sellers, makers and takers on BTC/USD and ETH/USD); the rest are transfers.
SIM_ORDERS_PCT ?= 0
# Parallel simulator processes. Each signs in its own thread and takes a
# disjoint slice of SIM_TRADERS, so several together push the node harder
# than one can; the genesis funds 64 traders.
SIM_WORKERS  ?= 1

# ---- perps-mode single-node config ----
# Uses a separate data dir from the spot single-node so the two genesis files
# (`genesis.jsonl` vs `genesis-perps.jsonl`) don't fight over CURRENT.
DATA_DIR_PERPS  ?= ./data-perps
GENESIS_PERPS   ?= genesis-perps.jsonl
RPC_LISTEN_PERPS  ?= 127.0.0.1:7700
DASH_LISTEN_PERPS ?= 127.0.0.1:8080
PERPS_PID_FILE  := $(DATA_DIR_PERPS)/sequencer.pid
PERPS_LOG_FILE  := $(DATA_DIR_PERPS)/sequencer.log
# Realistic RPC-path TPS — bound by signing plus ECDSA recovery in the mempool
# pipeline. For higher numbers use `make startbenchmark` (in-process generator).
SIM_TPS      ?= 10000
# Submits each worker keeps awaiting a reply; offered load is bounded by
# SIM_INFLIGHT * SIM_WORKERS / commit latency.
SIM_INFLIGHT ?= 1500

# ---- benchmark mode (in-process tx generator) ----
BENCH_INTERVAL_MS    ?= 50
BENCH_MAX_BLOCK_TXS  ?= 50000
BENCH_TRADERS        ?= 32
BENCH_WORKERS        ?= 0
BENCH_CHANNEL_CAP    ?= 100000
BENCH_KEEP_BLOCKS    ?= 10
BENCH_LOG_FILE       := $(DATA_DIR)/sequencer-bench.log
BIN_DIR              := ./bin

# ---- multi-node (3-validator) config ----
# node0 sequencer: RPC 7700, dashboard 18080. node1/node2 replicas: no RPC
# listener, dashboards 18081/18082. See scripts/supervisord/supervisord.conf.
SUPERVISOR_DIR := scripts/supervisord
MULTI_SEQ_RPC    := 127.0.0.1:7700
MULTI_DASH_PORTS := 18080 18081 18082
# Stress profile used ONLY by `make startmultisimulation`, deliberately well
# above the single-node `startsimulation` defaults.
MULTI_SIM_TRADERS  ?= 128
MULTI_SIM_TPS      ?= 100000
MULTI_SIM_DURATION ?= 30

# ---- exchange layer (gateway, frontend, simulation game) ----
# These three crates are in progress. Ports are chosen not to collide with the
# node dashboard (8080) or the multi-node dashboards (18080-18082).
GATEWAY_LISTEN ?= 127.0.0.1:8081
GATEWAY_ARGS   ?=
# The admin page's simulation switch. The gateway launches cb-sim workers
# itself, so `make start` no longer starts one by hand: two populations on
# the same trader range would share nonces and strand each other. SIM_MODE
# is the switch's position at start-up; the operator moves it from /admin.
SIM_MODE       ?= on
GATEWAY_BIN    := target/release/cb-gateway
GATEWAY_LOG    := $(DATA_DIR)/gateway.log
GATEWAY_PID_FILE := $(DATA_DIR)/gateway.pid
SIMGAME_ARGS   ?=
WEB_DIR        := crates/cb-web
WEB_PORT       ?= 3000

# ---- oracle (the price publisher every derivative market needs) ----
# Without this running, no derivative market has a mark price: the ticker reads
# zero, no position can be opened and nothing is ever liquidated. The control
# port is next to the node dashboard's range and away from the gateway (8081).
ORACLE_BIN       := target/release/cb-oracle
ORACLE_LOG       := $(DATA_DIR)/oracle.log
ORACLE_PID_FILE  := $(DATA_DIR)/oracle.pid
ORACLE_PERIOD_MS ?= 1000
ORACLE_SEED      ?= 42
ORACLE_SCENARIO  ?= calm
ORACLE_CONTROL   ?= 127.0.0.1:8171
ORACLE_ARGS      ?=

.DEFAULT_GOAL := help

.PHONY: help \
        start stop startmulti startsimulation startmultisimulation startbenchmark startmultibenchmark \
        dev gateway gateway-bg build-gateway simgame web-build web-serve \
        oracle build-oracle oracle-logs \
        env env-secret check-env \
        build build-sim build-client build-all build-orderbench package init status logs sim-status sim-logs bench-logs gateway-logs \
        orderbench orderbench-rpc orderbench-profile orderbench-determinism \
        fmt fmt-check lint check \
        benchmark test testmulti integrationtest testall testallslow testblockchain \
        simperps simshock simcollapse simfeestress clean \
        startperps stopperps testperps oraclehit perps-logs testclient \
        e2e e2e-fast e2e-one build-e2e e2e-clean

# --------------------------------------------------------------------- help
help: ## Show this menu
	@echo "SuperFrontierLiquid"
	@echo
	@echo "Primary:"
	@printf "  \033[36m%-26s\033[0m %s\n" "make"                       "show this menu"
	@printf "  \033[36m%-26s\033[0m %s\n" "make start"                 "start single-node blockchain (sequencer + dashboard)"
	@printf "  \033[36m%-26s\033[0m %s\n" "make stop"                  "stop everything (single, multi, simulator, gateway)"
	@printf "  \033[36m%-26s\033[0m %s\n" "make dev"                   "node + gateway + frontend for local development"
	@printf "  \033[36m%-26s\033[0m %s\n" "make startmulti"            "start 3-node cluster (1 sequencer + 2 replicas)"
	@printf "  \033[36m%-26s\033[0m %s\n" "make startsimulation"       "RPC stress test: cb-sim against a running node (auto-starts one)"
	@printf "  \033[36m%-26s\033[0m %s\n" "make startmultisimulation"  "boot the 3-node cluster AND launch the RPC stress simulator"
	@printf "  \033[36m%-26s\033[0m %s\n" "make startbenchmark"        "max-TPS bench: single node, in-process tx generator"
	@printf "  \033[36m%-26s\033[0m %s\n" "make startmultibenchmark"   "max-TPS bench: 3-node cluster, leader uses in-process generator"
	@echo
	@echo "Other:"
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
	  | grep -vE '^(start|stop|dev|startmulti|startsimulation|startmultisimulation|startbenchmark|startmultibenchmark|help):' \
	  | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-26s\033[0m %s\n", $$1, $$2}'

# --------------------------------------------------------------------- genesis env
env: ## Write rust/.env from .env.example with a freshly generated server secret (FORCE=1 overwrites)
	@if [ ! -f $(ENV_EXAMPLE) ]; then echo "missing $(ENV_EXAMPLE)"; exit 1; fi
	@if [ -f $(ENV_FILE) ] && [ -z "$(FORCE)" ]; then \
		echo "$(ENV_FILE) exists — leaving it alone. Use 'make env FORCE=1' to overwrite."; \
	else \
		sed -E 's/[[:space:]]+#.*$$//' $(ENV_EXAMPLE) > $(ENV_FILE); \
		echo "wrote $(ENV_FILE) from $(ENV_EXAMPLE)"; \
		echo "  (trailing '# ...' comments are stripped: cb-genesis's .env parser"; \
		echo "   keeps them inside the value, which makes every address unparseable)"; \
		$(MAKE) --no-print-directory env-secret; \
	fi

# The one value in .env that must NOT come from the committed example. Every
# account's trading key is HKDF(server_secret, login_address), so a shared
# constant means anyone with a clone of this repository can derive every user's
# on-chain key from their public login address. The example therefore ships a
# placeholder the gateway refuses, and this target replaces it per machine.
env-secret: ## Write a fresh random GATEWAY_SERVER_SECRET into rust/.env
	@if [ ! -f $(ENV_FILE) ]; then echo "no $(ENV_FILE) — run 'make env' first"; exit 1; fi
	@secret=$$( (openssl rand -hex 32 2>/dev/null) || (LC_ALL=C tr -dc 'a-f0-9' < /dev/urandom | head -c 64) ); \
	if [ $${#secret} -lt 32 ]; then echo "could not generate a server secret"; exit 1; fi; \
	awk -v s="$$secret" '/^[[:space:]]*(export[[:space:]]+)?GATEWAY_SERVER_SECRET=/ \
		{ print "export GATEWAY_SERVER_SECRET=" s; next } { print }' $(ENV_FILE) > $(ENV_FILE).tmp; \
	if ! grep -q 'GATEWAY_SERVER_SECRET=' $(ENV_FILE).tmp; then \
		printf 'export GATEWAY_SERVER_SECRET=%s\n' "$$secret" >> $(ENV_FILE).tmp; \
	fi; \
	mv $(ENV_FILE).tmp $(ENV_FILE); \
	chmod 600 $(ENV_FILE) 2>/dev/null || true; \
	echo "generated a fresh GATEWAY_SERVER_SECRET in $(ENV_FILE) (32 random bytes, this machine only)"; \
	echo "  NOTE: rotating this orphans existing trading accounts in $(DATA_DIR)/gateway.sqlite —"; \
	echo "        the same login now derives a different on-chain account. Wipe it if balances look empty."

check-env: ## Make sure rust/.env exists and the genesis loader can parse it
	@if [ ! -f $(ENV_FILE) ]; then \
		echo "no $(ENV_FILE) — creating one from $(ENV_EXAMPLE)"; \
		$(MAKE) --no-print-directory env; \
	fi
	@# cb-genesis's .env parser does not strip a trailing `# comment`: it keeps
	@# it inside the value, so a commented address line fails genesis with
	@# `Error: Invalid character ' ' at position 40`. Repair it here — the
	@# comments themselves are preserved in $(ENV_EXAMPLE).
	@if grep -qE '^[[:space:]]*(export[[:space:]]+)?[A-Za-z_][A-Za-z0-9_]*=[^#]*[[:space:]]+#' $(ENV_FILE); then \
		sed -E 's/[[:space:]]+#.*$$//' $(ENV_FILE) > $(ENV_FILE).tmp && mv $(ENV_FILE).tmp $(ENV_FILE); \
		echo "repaired $(ENV_FILE): stripped trailing '# ...' comments from value lines"; \
		echo "  (cb-genesis keeps them inside the value; the comments are still in $(ENV_EXAMPLE))"; \
	fi
	@# A .env carrying the placeholder, or the constant this repository used to
	@# ship, is a .env whose gateway refuses to start — every account derived
	@# from a published secret is compromised by definition. Rotate it here
	@# rather than making the operator read a startup error to find that out.
	@if grep -qE '^[[:space:]]*(export[[:space:]]+)?GATEWAY_SERVER_SECRET=(replace-me-run-make-env|local-development-secret-do-not-reuse)[[:space:]]*$$' $(ENV_FILE); then \
		echo "$(ENV_FILE) carries a published GATEWAY_SERVER_SECRET; generating a private one"; \
		$(MAKE) --no-print-directory env-secret; \
	fi

# --------------------------------------------------------------------- primary
start: build ## Start the single-node sequencer (RPC + dashboard) in the background
	@mkdir -p $(DATA_DIR)
	@if [ ! -f $(DATA_DIR)/CURRENT ]; then $(MAKE) init; fi
	@# One shell for the check and the launch. They used to be separate
	@# recipe lines, and `exit 0` in the "already running" branch only ended
	@# that line's shell — the launch on the next line still ran, died on the
	@# RocksDB lock, and (before the pid was written only after survival)
	@# overwrote the live node's pid file and truncated its log. That is why
	@# `make status` once said "not running" about a node that was. The pid
	@# file is written only once the process has survived its first second,
	@# and the log is appended to rather than truncated.
	@if [ -f $(PID_FILE) ] && kill -0 $$(cat $(PID_FILE)) 2>/dev/null; then \
		echo "single-node sequencer already running (pid $$(cat $(PID_FILE)))"; \
	else \
		nohup $(NODE_BIN) --data-dir $(DATA_DIR) sequencer \
			--listen $(RPC_LISTEN) \
			--dashboard-listen $(DASH_LISTEN) \
			--block-interval-ms $(INTERVAL_MS) \
			--keep-blocks $(KEEP_BLOCKS) \
			$(NODE_EXTRA_ARGS) \
			>> $(LOG_FILE) 2>&1 & \
		pid=$$!; sleep 1; \
		if kill -0 $$pid 2>/dev/null; then \
			echo $$pid > $(PID_FILE); \
		else \
			echo "sequencer exited at once; the end of $(LOG_FILE) says why:"; \
			tail -5 $(LOG_FILE) | sed 's/^/    /'; exit 1; \
		fi; \
		echo "single-node sequencer started (pid $$pid)"; \
		echo "  RPC:       $(RPC_LISTEN)"; \
		echo "  dashboard: http://$(DASH_LISTEN)"; \
		echo "  logs:      $(LOG_FILE)   (set RUST_LOG=info for anything in them)"; \
	fi

stop-sim: ## Stop the RPC load simulator only (the trickle `make start` runs, or a stress load)
	@if [ -f $(SIM_PID_FILE) ]; then \
		PIDS=$$(cat $(SIM_PID_FILE) | tr '\n' ' '); \
		for PID in $$PIDS; do kill $$PID 2>/dev/null || true; done; sleep 1; \
		rm -f $(SIM_PID_FILE); \
		echo "stopped simulator pid(s) $$PIDS"; \
	fi
	@pkill -x cb-sim 2>/dev/null || true

stop: ## Stop everything: single-node, cluster, simulator, gateway, strays
	@# 1) the simulator
	@if [ -f $(SIM_PID_FILE) ]; then \
		PIDS=$$(cat $(SIM_PID_FILE) | tr '\n' ' '); \
		for PID in $$PIDS; do kill $$PID 2>/dev/null || true; done; sleep 1; \
		rm -f $(SIM_PID_FILE); \
		echo "stopped simulator pid(s) $$PIDS"; \
	fi
	@pkill -x cb-sim 2>/dev/null || true
	@# 2) the single-node sequencer
	@if [ -f $(PID_FILE) ]; then \
		PID=$$(cat $(PID_FILE)); \
		if kill -0 $$PID 2>/dev/null; then kill $$PID; sleep 1; fi; \
		rm -f $(PID_FILE); \
		echo "stopped single-node sequencer pid $$PID"; \
	fi
	@# 2b) the perps single-node sequencer
	@if [ -f $(PERPS_PID_FILE) ]; then \
		PID=$$(cat $(PERPS_PID_FILE)); \
		if kill -0 $$PID 2>/dev/null; then kill $$PID; sleep 1; fi; \
		rm -f $(PERPS_PID_FILE); \
		echo "stopped perps sequencer pid $$PID"; \
	fi
	@# 3) the multi-node supervisord cluster (no-op if not running)
	@if [ -f $(SUPERVISOR_DIR)/logs/supervisord.pid ] || [ -S $(SUPERVISOR_DIR)/logs/supervisor.sock ]; then \
		python3 $(SUPERVISOR_DIR)/teardown.py >/dev/null 2>&1 || true; \
		echo "stopped multi-node cluster"; \
	fi
	@# 3b) the oracle price publisher
	@if [ -f $(ORACLE_PID_FILE) ]; then \
		PID=$$(cat $(ORACLE_PID_FILE)); \
		if kill -0 $$PID 2>/dev/null; then kill $$PID; sleep 1; fi; \
		rm -f $(ORACLE_PID_FILE); \
		echo "stopped oracle pid $$PID"; \
	fi
	@# 3c) the backgrounded gateway
	@if [ -f $(GATEWAY_PID_FILE) ]; then \
		PID=$$(cat $(GATEWAY_PID_FILE)); \
		if kill -0 $$PID 2>/dev/null; then kill $$PID; sleep 1; fi; \
		rm -f $(GATEWAY_PID_FILE); \
		echo "stopped gateway pid $$PID"; \
	fi
	@# 4) belt and braces: any stray node, simulator, gateway or supervisord
	@pkill -f "$(NODE_BIN)" 2>/dev/null || true
	@pkill -f "target/release/cb-gateway" 2>/dev/null || true
	@pkill -f "$(ORACLE_BIN)" 2>/dev/null || true
	@pkill -f "supervisord -c .*supervisord.conf" 2>/dev/null || true
	@echo "all cb-node + cb-sim + cb-gateway + supervisord processes stopped"

startmulti: build ## Start the 3-node cluster under supervisord (needs `pip3 install supervisor`)
	@$(MAKE) --no-print-directory check-env
	python3 $(SUPERVISOR_DIR)/run.py
	@echo
	@echo "dashboards:"
	@for p in $(MULTI_DASH_PORTS); do echo "  http://127.0.0.1:$$p/"; done

# The simulator signs with the chain id in .env (MYCHAINID, the same value the
# genesis was built with). It used to sign with cb-sim's default of 1, so on a
# chain with any other id the executor rejected every transaction it sent
# ("wrong chain id") while the simulator reported them all as accepted.
startsimulation: check-env build build-sim ## Run the RPC stress simulator (auto-starts a single node if nothing is up)
	@# Prefer the single-node sequencer if its dashboard answers, else the
	@# cluster sequencer (dashboard 18080, RPC 7700). If neither is up, start
	@# the single node.
	@TARGET_RPC=""; \
	if curl -fsS --max-time 1 "http://$(DASH_LISTEN)/api/status" >/dev/null 2>&1; then \
		TARGET_RPC=$(RPC_LISTEN); echo "targeting single-node sequencer at $$TARGET_RPC"; \
	elif curl -fsS --max-time 1 "http://127.0.0.1:18080/api/status" >/dev/null 2>&1; then \
		TARGET_RPC=$(MULTI_SEQ_RPC); echo "targeting cluster sequencer at $$TARGET_RPC"; \
	else \
		echo "no node running — starting the single node first ('make startmultisimulation' for the cluster)"; \
		$(MAKE) --no-print-directory start; \
		for i in 1 2 3 4 5 6 7 8 9 10; do \
			if curl -fsS --max-time 1 "http://$(DASH_LISTEN)/api/status" >/dev/null 2>&1; then break; fi; \
			sleep 0.5; \
		done; \
		TARGET_RPC=$(RPC_LISTEN); echo "single-node sequencer ready at $$TARGET_RPC"; \
	fi; \
	mkdir -p $(DATA_DIR); \
	if [ -f $(SIM_PID_FILE) ] && kill -0 $$(cat $(SIM_PID_FILE)) 2>/dev/null; then \
		echo "simulator already running (pid $$(cat $(SIM_PID_FILE) | tr '\n' ' '))"; exit 0; \
	fi; \
	set -a; . ./$(ENV_FILE); set +a; \
	workers=$(SIM_WORKERS); traders=$(SIM_TRADERS); \
	if [ "$$workers" -lt 1 ]; then workers=1; fi; \
	per=$$(( traders / workers )); \
	if [ "$$per" -lt 2 ]; then echo "SIM_TRADERS=$$traders is too few for SIM_WORKERS=$$workers (each worker needs 2)"; exit 1; fi; \
	tps=$$(( $(SIM_TPS) / workers )); if [ "$$tps" -lt 1 ]; then tps=1; fi; \
	: > $(SIM_PID_FILE); \
	i=0; while [ $$i -lt $$workers ]; do \
		log=$(SIM_LOG_FILE); if [ $$i -gt 0 ]; then log=$(DATA_DIR)/sim-$$i.log; fi; \
		nohup $(SIM_BIN) run \
			--sequencer $$TARGET_RPC \
			--chain-id "$${MYCHAINID:-1}" \
			--num-traders $$per \
			--trader-offset $$(( i * per )) \
			--orders-pct $(SIM_ORDERS_PCT) \
			--tx-per-sec $$tps \
			--max-in-flight $(SIM_INFLIGHT) \
			--rng-seed $$(( 0xC0FFEE + i )) \
			> $$log 2>&1 & \
		echo $$! >> $(SIM_PID_FILE); \
		i=$$(( i + 1 )); \
	done; \
	sleep 1; \
	for pid in $$(cat $(SIM_PID_FILE)); do \
		if ! kill -0 $$pid 2>/dev/null; then \
			echo "a simulator worker exited at once; the end of $(SIM_LOG_FILE) says why:"; \
			tail -5 $(SIM_LOG_FILE) 2>/dev/null | sed 's/^/    /'; \
			for p in $$(cat $(SIM_PID_FILE)); do kill $$p 2>/dev/null || true; done; \
			rm -f $(SIM_PID_FILE); exit 1; \
		fi; \
	done; \
	echo "simulator started ($$workers worker(s), pid $$(cat $(SIM_PID_FILE) | tr '\n' ' '))"; \
	echo "  chain id:   $${MYCHAINID:-1}"; \
	echo "  traders:    $$traders ($$per per worker, disjoint ranges)"; \
	echo "  target tps: $(SIM_TPS) ($$tps per worker)"; \
	echo "  orders:     $(SIM_ORDERS_PCT)% of submissions are spot orders (buyers and sellers, makers and takers); the rest are transfers"; \
	echo "  logs:       $(SIM_LOG_FILE)$$( [ $$workers -gt 1 ] && echo ", $(DATA_DIR)/sim-<n>.log" )"

startmultisimulation: check-env build build-sim ## Boot the 3-node cluster AND run the stress simulator against it
	@$(MAKE) --no-print-directory startmulti
	@echo
	@echo "waiting briefly for replicas to attach + sync..."
	@sleep 2
	@MAX_TXS=$$(($(MULTI_SIM_TPS) * $(MULTI_SIM_DURATION))); \
	echo "stress profile: $(MULTI_SIM_TRADERS) traders, target $(MULTI_SIM_TPS) tps, $(MULTI_SIM_DURATION)s (max-txs=$$MAX_TXS)"; \
	mkdir -p $(DATA_DIR); \
	echo "running cb-sim in the foreground; logs: $(SIM_LOG_FILE)"; \
	set -a; . ./$(ENV_FILE); set +a; \
	$(SIM_BIN) run \
		--sequencer $(MULTI_SEQ_RPC) \
		--chain-id "$${MYCHAINID:-1}" \
		--num-traders $(MULTI_SIM_TRADERS) \
		--tx-per-sec $(MULTI_SIM_TPS) \
		--max-txs $$MAX_TXS \
		2>&1 | tee $(SIM_LOG_FILE); \
	echo; \
	echo "==== cluster stress summary ===="; \
	SUSTAINED=$$(grep "sustained_tps" $(SIM_LOG_FILE) | tail -1 | sed -E 's/.*sustained_tps=//; s/[[:space:]].*$$//' || echo "?"); \
	echo "client-side sustained submit rate (cb-sim accepted/elapsed): $$SUSTAINED tps"; \
	echo; \
	echo "per-node apply-side metrics:"; \
	for p in $(MULTI_DASH_PORTS); do \
		JSON=$$(curl -fsS --max-time 2 "http://127.0.0.1:$$p/api/status" 2>/dev/null); \
		if [ -z "$$JSON" ]; then \
			echo "  port $$p: dashboard unreachable"; \
		else \
			echo "$$JSON" | PORT=$$p python3 scripts/multi_sim_summary.py 2>/dev/null \
				|| echo "  port $$p: parse error"; \
		fi; \
	done
	@echo
	@echo "stress run complete. Cluster still running — stop with 'make stop'."

startbenchmark: build ## Max-TPS bench: single node with the in-process tx generator
	@mkdir -p $(DATA_DIR)
	@if [ ! -f $(DATA_DIR)/CURRENT ]; then $(MAKE) init; fi
	@if [ -f $(PID_FILE) ] && kill -0 $$(cat $(PID_FILE)) 2>/dev/null; then \
		echo "sequencer already running (pid $$(cat $(PID_FILE))) — run 'make stop' first"; exit 1; \
	fi
	@nohup $(NODE_BIN) --data-dir $(DATA_DIR) sequencer \
		--listen $(RPC_LISTEN) \
		--dashboard-listen $(DASH_LISTEN) \
		--block-interval-ms $(BENCH_INTERVAL_MS) \
		--max-block-txs $(BENCH_MAX_BLOCK_TXS) \
		--benchmark \
		--bench-traders $(BENCH_TRADERS) \
		--bench-workers $(BENCH_WORKERS) \
		--bench-channel-capacity $(BENCH_CHANNEL_CAP) \
		--keep-blocks $(BENCH_KEEP_BLOCKS) \
		> $(BENCH_LOG_FILE) 2>&1 & \
		echo $$! > $(PID_FILE)
	@sleep 1
	@echo "single-node benchmark started (pid $$(cat $(PID_FILE)))"
	@echo "  RPC:            $(RPC_LISTEN)"
	@echo "  dashboard:      http://$(DASH_LISTEN)"
	@echo "  block interval: $(BENCH_INTERVAL_MS) ms (max $(BENCH_MAX_BLOCK_TXS) txs/block)"
	@echo "  bench traders:  $(BENCH_TRADERS)  workers: $(BENCH_WORKERS) (0 = auto)"
	@echo "  pruning:        block bodies only, keep $(BENCH_KEEP_BLOCKS)"
	@echo "  logs:           $(BENCH_LOG_FILE)"

benchmark: startbenchmark ## Alias for startbenchmark

# ---------------------------------------------------------------- order bench
#
# `startbenchmark` above measures TRANSACTION throughput: its generator signs
# Transfers and never places an order. These targets measure ORDER throughput —
# submitted, applied and matched, reported separately — over both the
# in-process path and the real RPC path. See docs/performance.md.
ORDERBENCH_BIN     ?= ./target/release/cb-order-bench
ORDERBENCH_BLOCKS  ?= 200
ORDERBENCH_DEPTH   ?= 500
ORDERBENCH_TAKERS  ?= 250
ORDERBENCH_MAKERS  ?= 250
ORDERBENCH_ARGS    ?=

build-orderbench: ## Build the release cb-order-bench binary
	cargo build --release -p cb-node --bin cb-order-bench

orderbench: build-orderbench ## Order throughput, in-process (matcher + executor + trie commit)
	@$(ORDERBENCH_BIN) --blocks $(ORDERBENCH_BLOCKS) --book-depth $(ORDERBENCH_DEPTH) 		--takers-per-block $(ORDERBENCH_TAKERS) --makers-per-block $(ORDERBENCH_MAKERS) 		--max-block-txs 2000 --channel-capacity 4000 $(ORDERBENCH_ARGS) in-process

orderbench-rpc: build-orderbench ## Order throughput over RPC (adds ECDSA recovery, mempool, capnp, TCP)
	@$(ORDERBENCH_BIN) --blocks $(ORDERBENCH_BLOCKS) --book-depth $(ORDERBENCH_DEPTH) 		--takers-per-block $(ORDERBENCH_TAKERS) --makers-per-block $(ORDERBENCH_MAKERS) 		--max-block-txs 20000 $(ORDERBENCH_ARGS) rpc

orderbench-profile: build-orderbench ## ECDSA recovery (serial vs rayon) and mempool round-trip cost
	@$(ORDERBENCH_BIN) profile --samples 100000

orderbench-determinism: build-orderbench ## Replay one workload twice and compare state roots
	@$(ORDERBENCH_BIN) --blocks 20 --book-depth 200 --takers-per-block 50 		--makers-per-block 50 --num-makers 16 --num-takers 16 --producers 1 determinism

startmultibenchmark: build ## Max-TPS bench: 3-node cluster, leader uses the in-process generator
	@$(MAKE) --no-print-directory check-env
	python3 $(SUPERVISOR_DIR)/run.py --conf $(SUPERVISOR_DIR)/supervisord_benchmark.conf
	@echo
	@echo "dashboards:"
	@echo "  http://127.0.0.1:18080/   (leader, in-process tx generator)"
	@echo "  http://127.0.0.1:18081/   (fast-mirror replica, raw-bytes mirror)"
	@echo "  http://127.0.0.1:18082/   (fast-mirror replica, raw-bytes mirror)"
	@echo
	@echo "node0 generates txs in-process at max throughput."
	@echo "node1 + node2 mirror block bytes only (no re-execution); votes are async."
	@echo "2/3 quorum forms each block: leader self-vote plus at least one replica."
	@echo "Stop with 'make stop'."

# --------------------------------------------------------- gateway / web / game
# `cb-gateway` and `cb-simgame` are stub crates today (one-line lib.rs, no
# binary). These targets check for a binary first and explain themselves rather
# than failing inside cargo.
define needs_binary
	@if [ ! -f crates/$(1)/src/main.rs ] && ! ls crates/$(1)/src/bin/*.rs >/dev/null 2>&1; then \
		echo "$(1) has no binary yet — the crate is still library-only (docs/PLAN.md, milestones M4 and M6)."; \
		echo "This target starts working the moment crates/$(1)/src/main.rs lands."; \
		exit 1; \
	fi
endef

# ---- how the gateway is launched, defined once ------------------------------
# `gateway`, `gateway-bg` and `dev` all start the same process. Three
# hand-written copies of this flag list is exactly how one of them ends up
# without `--faucet-treasury-key` six weeks from now, so there is one copy and
# the three callers differ only in what they do with it.
#
# The two secrets go through the **environment**, not the command line: clap
# reads `CB_GATEWAY_SERVER_SECRET` and `CB_GATEWAY_FAUCET_TREASURY_KEY`, and a
# secret in argv is visible in `ps` to every other user on the machine. The
# rest of the flags are built into `"$$@"` with `set --`, so quoting survives.
#
# The operator role is bound to an address when .env names one, because an
# address is held by whoever holds its key. Without one we fall back to the
# handle `admin`, which now requires an explicit opt-in flag: handles are
# first-come, so on a fresh store that role belongs to whoever logs in first.
# Acceptable on a loopback simulator; nowhere else — hence the loud line, which
# every caller prints somewhere a human will find it.
define gateway_launch
set -a; . ./$(ENV_FILE); set +a; \
if [ -z "$$GATEWAY_SERVER_SECRET" ]; then \
  echo "GATEWAY_SERVER_SECRET is unset in $(ENV_FILE) — run 'make env-secret'"; exit 1; \
fi; \
if [ -z "$$TREASURY_KEY" ]; then \
  echo "TREASURY_KEY is unset in $(ENV_FILE) — the faucet would have nothing to sign with"; exit 1; \
fi; \
export CB_GATEWAY_SERVER_SECRET="$$GATEWAY_SERVER_SECRET"; \
export CB_GATEWAY_FAUCET_TREASURY_KEY="$$TREASURY_KEY"; \
if [ -n "$$GATEWAY_ADMIN_ADDRESS" ]; then \
  admin_args="--admin-address $$GATEWAY_ADMIN_ADDRESS"; \
  operator="operator: $$GATEWAY_ADMIN_ADDRESS (the role is bound to that address)"; \
else \
  admin_args="--admin-handle admin --insecure-admin-by-handle"; \
  operator="operator: INSECURE, by handle — whoever registers 'admin' first holds every /admin route. Set GATEWAY_ADMIN_ADDRESS in $(ENV_FILE) to bind it to your own key."; \
fi; \
set -- --listen $(GATEWAY_LISTEN) \
  --node-rpc $(RPC_LISTEN) \
  --store sqlite --sqlite-path $(DATA_DIR)/gateway.sqlite \
  --cors-origin 'http://127.0.0.1:$(WEB_PORT)' \
  --cors-origin 'http://localhost:$(WEB_PORT)' \
  --dev-login $$admin_args \
  --block-time-ms $(INTERVAL_MS) \
  --sim-bin $(abspath $(SIM_BIN)) --sim-mode $(SIM_MODE) --sim-log-dir $(DATA_DIR) \
  --oracle-control http://$(ORACLE_CONTROL) \
  $(GATEWAY_ARGS)
endef

build-gateway: ## Build the release cb-gateway binary
	cargo build --release -p cb-gateway

gateway: check-env build-gateway build-sim ## Start the REST/WebSocket gateway in the foreground
	@mkdir -p $(DATA_DIR)
	@$(gateway_launch); \
	echo "$$operator"; \
	exec $(GATEWAY_BIN) "$$@"

gateway-bg: check-env build-gateway build-sim ## Start the gateway in the background (log + pid file)
	@mkdir -p $(DATA_DIR)
	@# One shell for the whole target on purpose: `exit 0` in a guard ends only
	@# the recipe *line* it is written in, so a guard on its own line would be
	@# followed by the launch anyway — which is how you end up with two
	@# gateways fighting over one port and a pid file naming the loser.
	@if [ -f $(GATEWAY_PID_FILE) ] && kill -0 $$(cat $(GATEWAY_PID_FILE)) 2>/dev/null; then \
		echo "gateway already running (pid $$(cat $(GATEWAY_PID_FILE))); 'make stop' first"; \
		exit 0; \
	fi; \
	$(gateway_launch); \
	echo "$$operator" | tee -a $(GATEWAY_LOG); \
	nohup $(GATEWAY_BIN) "$$@" >> $(GATEWAY_LOG) 2>&1 & \
	pid=$$!; \
	echo $$pid > $(GATEWAY_PID_FILE); \
	ready=""; \
	for _ in $$(seq 1 30); do \
		kill -0 $$pid 2>/dev/null || break; \
		if command -v curl >/dev/null 2>&1; then \
			if curl -fsS -o /dev/null http://$(GATEWAY_LISTEN)/health 2>/dev/null; then ready=1; break; fi; \
		else \
			sleep 3; ready=unverified; break; \
		fi; \
		sleep 1; \
	done; \
	if [ -n "$$ready" ] && kill -0 $$pid 2>/dev/null; then \
		echo "gateway started (pid $$pid)"; \
		echo "  API:  http://$(GATEWAY_LISTEN)"; \
		echo "  logs: $(GATEWAY_LOG)"; \
	else \
		echo "gateway did not answer http://$(GATEWAY_LISTEN)/health within 30s;"; \
		echo "the end of $(GATEWAY_LOG) says why:"; \
		tail -5 $(GATEWAY_LOG) 2>/dev/null | sed 's/^/    /'; \
		kill $$pid 2>/dev/null || true; \
		rm -f $(GATEWAY_PID_FILE); \
		exit 1; \
	fi

simgame: ## Run the bot-driven simulation game
	cargo run --release -p cb-simgame -- $(SIMGAME_ARGS)

web-build: ## Build the Rust/WASM frontend into crates/cb-web/dist (needs `cargo install trunk`)
	@command -v trunk >/dev/null || { echo "trunk not found: cargo install trunk"; exit 1; }
	cd $(WEB_DIR) && trunk build --release
	@echo "built $(WEB_DIR)/dist"

web-serve: ## Serve the frontend with hot reload (WEB_PORT, default 3000)
	@command -v trunk >/dev/null || { echo "trunk not found: cargo install trunk"; exit 1; }
	cd $(WEB_DIR) && trunk serve --port $(WEB_PORT)

dev: ## Local development: node in the background, then gateway and frontend
	@command -v trunk >/dev/null || { echo "trunk not found: cargo install trunk"; exit 1; }
	@$(MAKE) --no-print-directory start
	@mkdir -p $(DATA_DIR)
	@# One launch definition, so `dev` cannot drift from `gateway`: it used to
	@# background `cargo run` with no flags at all, which since the server
	@# secret became mandatory means a gateway that dies on launch and a web app
	@# with nothing behind it.
	@$(MAKE) --no-print-directory gateway-bg
	@$(MAKE) --no-print-directory oracle
	@echo
	@echo "node RPC:  $(RPC_LISTEN)"
	@echo "dashboard: http://$(DASH_LISTEN)"
	@echo "frontend:  http://127.0.0.1:$(WEB_PORT)   (Ctrl-C stops it; 'make stop' stops the node)"
	@echo
	@cd $(WEB_DIR) && trunk serve --port $(WEB_PORT)

# --------------------------------------------------------------------- build
build: ## Build the release cb-node binary
	cargo build --release -p cb-node

build-sim: ## Build the release cb-sim binary
	cargo build --release -p cb-sim

build-client: ## Build the release cb-client binary
	cargo build --release -p cb-client

build-oracle: ## Build the release cb-oracle binary
	cargo build --release -p cb-oracle

# The price publisher. Every derivative market needs a mark price before it
# can accept an order, and this is the only thing that provides one. It signs
# as the genesis oracle authority, whose key `.env` carries as
# ORACLE_AUTHORITY_KEY; the binary reads that from the environment so the key
# never appears in `ps`. `.PHONY` and `stop` have known this target for a
# while; the recipe itself had gone missing, so `make up` started no oracle
# and said nothing.
oracle: check-env build-oracle ## Start the oracle price publisher in the background (log + pid file)
	@mkdir -p $(DATA_DIR)
	@if [ -f $(ORACLE_PID_FILE) ] && kill -0 $$(cat $(ORACLE_PID_FILE)) 2>/dev/null; then \
		echo "oracle already running (pid $$(cat $(ORACLE_PID_FILE)))"; exit 0; \
	fi; \
	set -a; . ./$(ENV_FILE); set +a; \
	if [ -z "$$ORACLE_AUTHORITY_KEY" ]; then \
		echo "ORACLE_AUTHORITY_KEY is unset in $(ENV_FILE) — the oracle has nothing to sign with"; exit 1; \
	fi; \
	export ORACLE_AUTHORITY_KEY; \
	nohup $(ORACLE_BIN) --rpc $(RPC_LISTEN) \
		--period-ms $(ORACLE_PERIOD_MS) --seed $(ORACLE_SEED) --scenario $(ORACLE_SCENARIO) \
		--control-listen $(ORACLE_CONTROL) $(ORACLE_ARGS) \
		> $(ORACLE_LOG) 2>&1 & \
	echo $$! > $(ORACLE_PID_FILE); \
	sleep 1; \
	if kill -0 $$(cat $(ORACLE_PID_FILE)) 2>/dev/null; then \
		echo "oracle started (pid $$(cat $(ORACLE_PID_FILE)))"; \
		echo "  scenario: $(ORACLE_SCENARIO), seed $(ORACLE_SEED), every $(ORACLE_PERIOD_MS) ms"; \
		echo "  control:  http://$(ORACLE_CONTROL)/scenario"; \
		echo "  logs:     $(ORACLE_LOG)"; \
	else \
		echo "oracle exited at once; the end of $(ORACLE_LOG) says why:"; \
		tail -5 $(ORACLE_LOG) 2>/dev/null | sed 's/^/    /'; \
		rm -f $(ORACLE_PID_FILE); exit 1; \
	fi

oracle-logs: ## Tail the oracle log
	@tail -f $(ORACLE_LOG)

build-all: ## Build the whole workspace (cb-web is excluded; use web-build)
	cargo build --workspace --release

client: build-client ## Run the cb-client REPL, e.g. `make client ARGS="wallets"`
	@$(CLIENT_BIN) $(ARGS)

package: build ## Copy the release node binary to ./bin
	@mkdir -p $(BIN_DIR)
	@cp $(NODE_BIN) $(BIN_DIR)/
	@echo "packaged: $(BIN_DIR)/cb-node"

init: build check-env ## Apply genesis to a fresh single-node data dir
	@mkdir -p $(DATA_DIR)
	$(NODE_BIN) --data-dir $(DATA_DIR) init --genesis $(GENESIS)

# --------------------------------------------------------------- format / lint
fmt: ## Format the workspace and the frontend crate
	cargo fmt --all
	cd $(WEB_DIR) && cargo fmt --all

fmt-check: ## Check formatting without writing (what CI should run)
	cargo fmt --all -- --check
	cd $(WEB_DIR) && cargo fmt --all -- --check

lint: ## Clippy over the workspace, warnings are errors
	cargo clippy --workspace --all-targets -- -D warnings

check: ## Fast type-check of the workspace
	cargo check --workspace --all-targets

# --------------------------------------------------------------------- status / logs
status: ## Show process status, chain head, mempool and simulator stats
	@echo "## processes"
	@if [ -f $(PID_FILE) ] && kill -0 $$(cat $(PID_FILE)) 2>/dev/null; then \
		echo "  single-node:  running (pid $$(cat $(PID_FILE)))"; \
	else \
		echo "  single-node:  not running"; \
	fi
	@if [ -f $(SUPERVISOR_DIR)/logs/supervisord.pid ] && kill -0 $$(cat $(SUPERVISOR_DIR)/logs/supervisord.pid) 2>/dev/null; then \
		echo "  multi-node:   running (supervisord pid $$(cat $(SUPERVISOR_DIR)/logs/supervisord.pid))"; \
	else \
		echo "  multi-node:   not running"; \
	fi
	@if [ -f $(SIM_PID_FILE) ] && kill -0 $$(cat $(SIM_PID_FILE)) 2>/dev/null; then \
		echo "  simulator:    running ($$(wc -l < $(SIM_PID_FILE) | tr -d ' ') worker(s), pid $$(cat $(SIM_PID_FILE) | tr '\n' ' '))"; \
	else \
		echo "  simulator:    not running"; \
	fi
	@echo
	@echo "## chain (via http://$(DASH_LISTEN)/api/status)"
	@JSON=$$(curl -fsS --max-time 1 "http://$(DASH_LISTEN)/api/status" 2>/dev/null); \
	if [ -z "$$JSON" ]; then \
		echo "  single-node dashboard unreachable"; \
	else \
		printf '%s' "$$JSON" | python3 scripts/print_status.py 2>/dev/null \
			|| printf '%s' "$$JSON" | python3 -c "import json,sys;d=json.load(sys.stdin);print('  height:',d.get('head_height'),' state_root:',d.get('state_root'))"; \
	fi
	@echo
	@echo "## simulator"
	@if [ -f $(SIM_LOG_FILE) ]; then \
		LAST=$$(grep -E "stats" $(SIM_LOG_FILE) | tail -1); \
		if [ -n "$$LAST" ]; then \
			echo "  $$LAST" | sed -E 's/.*INFO[^:]*:[[:space:]]*//; s/[[:space:]]+/ /g'; \
		else \
			echo "  no submission stats logged yet ($(SIM_LOG_FILE))"; \
		fi; \
	else \
		echo "  no simulator log ($(SIM_LOG_FILE))"; \
	fi

logs: ## Tail the single-node sequencer log
	@tail -f $(LOG_FILE)

sim-status: ## Say whether the simulator is running
	@if [ -f $(SIM_PID_FILE) ] && kill -0 $$(cat $(SIM_PID_FILE)) 2>/dev/null; then \
		echo "simulator running ($$(wc -l < $(SIM_PID_FILE) | tr -d ' ') worker(s), pid $$(cat $(SIM_PID_FILE) | tr '\n' ' '))"; \
	else \
		echo "simulator not running"; \
	fi

sim-logs: ## Tail the simulator log
	@tail -f $(SIM_LOG_FILE)

bench-logs: ## Tail the benchmark sequencer log
	@tail -f $(BENCH_LOG_FILE)

gateway-logs: ## Tail the gateway log written by `make dev`
	@tail -f $(GATEWAY_LOG)

# --------------------------------------------------------------------- testing
test: ## Unit tests only (cargo --lib --bins, fast)
	cargo test --workspace --lib --bins

testmulti: ## Multi-node sync test: 3-node cluster (sync, accounting, transfers, QCs); needs `pip3 install supervisor`
	@command -v supervisord >/dev/null 2>&1 || { echo "supervisord not found: pip3 install supervisor   (the full suite skips this phase without it)"; exit 1; }
	cargo build --release -p cb-node -p cb-sim
	@$(MAKE) --no-print-directory check-env
	python3 $(SUPERVISOR_DIR)/integration_test.py

integrationtest: ## Cargo integration tests (--tests) plus the multi-node sync test
	@echo "==> 1/2  cargo test --workspace --tests"
	cargo test --workspace --tests
	@echo
	@echo "==> 2/2  multi-node sync test"
	@$(MAKE) --no-print-directory testmulti

testallslow: build build-client ## Full suite SERIALLY (--test-threads=1) — safe on a tight ulimit
	@# Each phase owns its own server lifecycle:
	@#   - cargo tests use ephemeral tempdir databases (no server)
	@#   - testblockchain/testperps each start an isolated single-node sequencer
	@#   - testmulti runs scripts/supervisord/integration_test.py, which calls
	@#     run.py + teardown.py to manage the 3-node cluster
	@# Serial cargo avoids FD exhaustion where `ulimit -n` is low. Use `make
	@# testall` when it is not.
	@echo "==> 0/5  pre-flight: tear down any stray supervisor + single-node"
	@python3 $(SUPERVISOR_DIR)/teardown.py >/dev/null 2>&1 || true
	@$(MAKE) --no-print-directory stop >/dev/null 2>&1 || true
	@echo
	@set -e; trap 'echo; echo "==> post: tearing down supervisor + single-node"; python3 $(SUPERVISOR_DIR)/teardown.py >/dev/null 2>&1 || true; $(MAKE) --no-print-directory stop >/dev/null 2>&1 || true' EXIT INT TERM; \
	  echo "==> 1/5  cargo unit tests (serial)"; \
	  cargo test --workspace --lib --bins -- --test-threads=1; \
	  echo; \
	  echo "==> 2/5  cargo integration tests (serial)"; \
	  cargo test --workspace --tests -- --test-threads=1; \
	  echo; \
	  echo "==> 3/5  blockchain end-to-end"; \
	  $(MAKE) --no-print-directory testblockchain; \
	  echo; \
	  if command -v supervisord >/dev/null 2>&1; then \
	    echo "==> 4/5  multi-node sync (3-node cluster)"; \
	    $(MAKE) --no-print-directory testmulti; \
	  else \
	    echo "==> 4/5  multi-node sync SKIPPED: supervisord is not installed (pip3 install supervisor); run  make testmulti  once it is"; \
	  fi; \
	  echo; \
	  echo "==> 5/5  perps end-to-end"; \
	  $(MAKE) --no-print-directory testperps; \
	  echo
	@echo "==> testallslow PASSED (serial; all servers stopped)"

testall: build build-client ## Full suite in PARALLEL — needs `ulimit -n` high enough
	@# Same phases as testallslow, with cargo running tests in parallel. Faster,
	@# but RocksDB opens a lot of files: raise `ulimit -n` (65536 is plenty) if
	@# this trips on "Too many open files".
	@echo "==> 0/5  pre-flight: tear down any stray supervisor + single-node"
	@python3 $(SUPERVISOR_DIR)/teardown.py >/dev/null 2>&1 || true
	@$(MAKE) --no-print-directory stop >/dev/null 2>&1 || true
	@echo
	@set -e; ulimit -n 10240 2>/dev/null || true; trap 'echo; echo "==> post: tearing down supervisor + single-node"; python3 $(SUPERVISOR_DIR)/teardown.py >/dev/null 2>&1 || true; $(MAKE) --no-print-directory stop >/dev/null 2>&1 || true' EXIT INT TERM; \
	  echo "==> 1/5  cargo unit tests (parallel)"; \
	  cargo test --workspace --lib --bins; \
	  echo; \
	  echo "==> 2/5  cargo integration tests (parallel)"; \
	  cargo test --workspace --tests; \
	  echo; \
	  echo "==> 3/5  blockchain end-to-end"; \
	  $(MAKE) --no-print-directory testblockchain; \
	  echo; \
	  if command -v supervisord >/dev/null 2>&1; then \
	    echo "==> 4/5  multi-node sync (3-node cluster)"; \
	    $(MAKE) --no-print-directory testmulti; \
	  else \
	    echo "==> 4/5  multi-node sync SKIPPED: supervisord is not installed (pip3 install supervisor); run  make testmulti  once it is"; \
	  fi; \
	  echo; \
	  echo "==> 5/5  perps end-to-end"; \
	  $(MAKE) --no-print-directory testperps; \
	  echo
	@echo "==> testall PASSED (parallel; all servers stopped)"

testblockchain: ## Single-node end-to-end over RPC via cb-client (blocks, accounting, transfers)
	cargo build --release -p cb-node -p cb-client
	python3 scripts/blockchain_test.py

testperps: build build-client ## End-to-end perps smoke test (boot, oracle update, place, position, stop)
	cargo build --release -p cb-node -p cb-client
	python3 scripts/perps_test.py

testclient: build build-client ## End-to-end client trade smoke (login, faucet, spot/perp/future place)
	cargo build --release -p cb-node -p cb-client
	python3 scripts/client_trade_smoke.py
	@rm -rf ./data-test-client

# ------------------------------------------------------------- end-to-end suite
# `cb-e2e` starts the real cb-node and cb-gateway binaries, on ports and in
# temporary directories it picks itself, and drives them over HTTP and WebSocket
# exactly as a browser would. It needs nothing running first and collides with
# nothing that is: a developer's `make up` and two parallel `make e2e` runs can
# all share a machine.
#
# The binary paths are passed explicitly rather than discovered, so the suite
# can never silently test a stale build.
#
# CB_E2E_PARALLEL   how many stacks may run at once (default 2). Each stack is
#                   two processes, a RocksDB, a SQLite and three sockets, so
#                   raising it needs `ulimit -n` to match.
# CB_E2E_KEEP_DATA  keep each stack's data directory and logs after the run.
E2E_JOBS ?= 2

e2e: check-env build-e2e ## End-to-end: boot the real node + gateway and drive them like a browser
	@echo "==> cb-e2e (CB_E2E_PARALLEL=$(E2E_JOBS))"
	CB_E2E_NODE_BIN=$(abspath target/release/cb-node) \
	CB_E2E_GATEWAY_BIN=$(abspath target/release/cb-gateway) \
	CB_E2E_PARALLEL=$(E2E_JOBS) \
	cargo test -p cb-e2e --no-fail-fast -- --include-ignored --test-threads=$(E2E_JOBS)

e2e-fast: ## Only the cb-e2e checks that need no running stack (shipped genesis, env, ws path)
	cargo test -p cb-e2e

e2e-one: check-env build-e2e ## One end-to-end test by name, e.g. `make e2e-one T=the_product_journey`
	@test -n "$(T)" || { echo "usage: make e2e-one T=<test name substring>"; exit 2; }
	CB_E2E_NODE_BIN=$(abspath target/release/cb-node) \
	CB_E2E_GATEWAY_BIN=$(abspath target/release/cb-gateway) \
	cargo test -p cb-e2e -- --include-ignored --test-threads=1 --nocapture $(T)

build-e2e: ## Build the release binaries the end-to-end suite drives
	cargo build --release -p cb-node -p cb-gateway

e2e-clean: ## Kill any node or gateway a killed e2e run left behind, and wipe its temp dirs
	@# Stack::drop cleans up after a failing test. It cannot clean up after the
	@# test *binary* being killed, which is the one case this covers.
	@pkill -f 'cb-node .*--data-dir .*cb-e2e-' 2>/dev/null || true
	@pkill -f 'cb-gateway .*--sqlite-path .*cb-e2e-' 2>/dev/null || true
	@rm -rf $${TMPDIR:-/tmp}/cb-e2e-* 2>/dev/null || true
	@echo "e2e strays cleared"

# --------------------------------------------------------------- in-process sims
# These run the engine and executor directly, with no sequencer process and no
# RPC. They are the fastest way to exercise margin, funding and liquidation.
simperps: ## In-process perps load: 8 traders, 1 market, 200 blocks
	cargo run --release -p cb-sim -- perps-load --num-traders 8 --num-markets 1 --blocks 200 --rng-seed 12648430

simshock: ## Oracle-shock cascade: 100 traders, 2 markets, 30% drop at block 200
	cargo run --release -p cb-sim -- perp-shock --out $(DATA_DIR)/sim-metrics

simcollapse: ## Long-tail bear: 60 traders, 10 epochs of -5%, 50% total drop
	cargo run --release -p cb-sim -- market-collapse --out $(DATA_DIR)/sim-metrics

simfeestress: ## Fee revenue stress: 50 traders, 1000 blocks, ~50 orders/block
	cargo run --release -p cb-sim -- fee-stress --out $(DATA_DIR)/sim-metrics

# ------------------------------------------------------------- perps single-node
# `make startperps` boots a single node against `genesis-perps.jsonl`, which
# adds perps, a dated future, an insurance fund, demo trader margin and
# distinct oracle authorities on top of the spot genesis. It uses its own data
# dir so it can coexist on disk with the spot chain in ./data.
startperps: build build-client ## Start a single node on the perps genesis (separate data dir)
	@mkdir -p $(DATA_DIR_PERPS)
	@if [ -f $(PERPS_PID_FILE) ] && kill -0 $$(cat $(PERPS_PID_FILE)) 2>/dev/null; then \
		echo "perps sequencer already running (pid $$(cat $(PERPS_PID_FILE)))"; exit 0; \
	fi
	@echo "deriving perps genesis env via scripts/perps_env.sh ..."
	@set -a; eval "$$(scripts/perps_env.sh)"; set +a; \
	if [ ! -f $(DATA_DIR_PERPS)/CURRENT ]; then \
		echo "applying $(GENESIS_PERPS) to $(DATA_DIR_PERPS) ..."; \
		$(NODE_BIN) --data-dir $(DATA_DIR_PERPS) init --genesis $(GENESIS_PERPS); \
	fi; \
	nohup $(NODE_BIN) --data-dir $(DATA_DIR_PERPS) sequencer \
		--listen $(RPC_LISTEN_PERPS) \
		--dashboard-listen $(DASH_LISTEN_PERPS) \
		--block-interval-ms $(INTERVAL_MS) \
		> $(PERPS_LOG_FILE) 2>&1 & \
		echo $$! > $(PERPS_PID_FILE)
	@sleep 1
	@echo "perps sequencer started (pid $$(cat $(PERPS_PID_FILE)))"
	@echo "  RPC:       $(RPC_LISTEN_PERPS)"
	@echo "  dashboard: http://$(DASH_LISTEN_PERPS)"
	@echo "  logs:      $(PERPS_LOG_FILE)"
	@echo "  genesis:   $(GENESIS_PERPS)"

stopperps: ## Stop the perps single node (leaves data and logs on disk)
	@if [ -f $(PERPS_PID_FILE) ]; then \
		PID=$$(cat $(PERPS_PID_FILE)); \
		if kill -0 $$PID 2>/dev/null; then kill $$PID; sleep 1; fi; \
		rm -f $(PERPS_PID_FILE); \
		echo "stopped perps sequencer pid $$PID"; \
	else \
		echo "no perps sequencer pid file at $(PERPS_PID_FILE)"; \
	fi

perps-logs: ## Tail the perps single-node log
	@tail -f $(PERPS_LOG_FILE)

oraclehit: build-client ## Sign and submit an OracleUpdate (MARKET=BTC|ETH PRICE=<whole usd>)
	@if [ -z "$(MARKET)" ] || [ -z "$(PRICE)" ]; then \
		echo "usage: make oraclehit MARKET=BTC PRICE=50000 [HEIGHT=<u64>]"; \
		echo "  MARKET is BTC or ETH"; \
		echo "  PRICE  is whole USD; scaled to 1e8 internally (50000 -> 5_000_000_000_000)"; \
		echo "  HEIGHT defaults to the current head height + 1, read from the dashboard"; \
		exit 2; \
	fi; \
	set -a; eval "$$(scripts/perps_env.sh)"; set +a; \
	case "$(MARKET)" in \
		BTC|btc|btc_perp) MARKET_REF=perp:0; ORACLE_M=$$ORACLE_BTC_MNEMONIC; ORACLE_I=$$ORACLE_BTC_INDEX ;; \
		ETH|eth|eth_perp) MARKET_REF=perp:1; ORACLE_M=$$ORACLE_ETH_MNEMONIC; ORACLE_I=$$ORACLE_ETH_INDEX ;; \
		*) echo "unknown MARKET=$(MARKET); pick BTC or ETH"; exit 2 ;; \
	esac; \
	if [ -z "$(HEIGHT)" ]; then \
		H=$$(curl -fsS --max-time 1 "http://$(DASH_LISTEN_PERPS)/api/status" 2>/dev/null \
			| python3 -c "import json,sys;d=json.load(sys.stdin);print(d.get('head_height',0))" 2>/dev/null); \
		if [ -z "$$H" ]; then H=1; fi; \
		HEIGHT_VAL=$$((H + 1)); \
	else \
		HEIGHT_VAL=$(HEIGHT); \
	fi; \
	PRICE_Q8=$$(python3 -c "print(int($(PRICE)) * 100_000_000)"); \
	mkdir -p $(DATA_DIR_PERPS)/oracle-home; \
	HOME=$(DATA_DIR_PERPS)/oracle-home $(CLIENT_BIN) login \
		--label oracle --mnemonic "$$ORACLE_M" --index $$ORACLE_I --force >/dev/null; \
	echo "oraclehit: market=$$MARKET_REF price_q8=$$PRICE_Q8 height=$$HEIGHT_VAL"; \
	HOME=$(DATA_DIR_PERPS)/oracle-home $(CLIENT_BIN) --rpc $(RPC_LISTEN_PERPS) \
		oracle $$MARKET_REF $$PRICE_Q8 --height $$HEIGHT_VAL

# --------------------------------------------------------------------- cleanup
clean: stop ## Stop everything and wipe the single-node, cluster and perps data
	@rm -rf $(DATA_DIR)
	@rm -rf $(DATA_DIR_PERPS)
	@rm -rf ./data-test-client ./data-test-blockchain
	@rm -rf $(SUPERVISOR_DIR)/data $(SUPERVISOR_DIR)/logs
	@echo "wiped $(DATA_DIR), $(DATA_DIR_PERPS), ./data-test-* and $(SUPERVISOR_DIR)/{data,logs}"
	@echo "(build output is left alone: 'cargo clean' and 'rm -rf $(WEB_DIR)/dist' are separate)"
