# Operator shortcuts for the sum100 CLI. Recipes wrap
# `cargo run --release --` and do not add flags the CLI does not already
# accept. Demo is the default. Production is only the *-prod targets.
# No ticker is pinned; TICKERS (or TICKER for probe) is required.
#
#   make help
#   make dump-prod TICKERS=KXBTCD-... SECONDS=70
#   make dump-digest TICKERS=KXBTCD-... SECONDS=180
#   make replay FILE=data/production/kalshi-2026-09-14.ndjson.gz

CARGO_RUN := cargo run --release --
VENUE     ?= kalshi
OUT       ?= data
TODAY     := $(shell date -u +%Y-%m-%d)
FILE      ?= data/production/kalshi-$(TODAY).ndjson.gz
SERIES    ?= KXBTCD
EXTRA     ?=

seconds      := $(if $(SECONDS),--seconds $(SECONDS),)
digest_flag  := $(if $(DIGEST),--digest-out $(DIGEST),)
digest_file  := $(if $(DIGEST),$(DIGEST),live.digest)
session_flag := $(if $(SESSION),--session $(SESSION),)
pace_flag    := $(if $(PACE),--pace $(PACE),)
tickers_flag := $(if $(TICKERS),--tickers $(TICKERS),)
probe_ticker := $(or $(TICKER),$(TICKERS))

need-tickers = $(if $(strip $(TICKERS)),,$(error TICKERS is required, e.g. make dump TICKERS=KXBTCD-...))
need-ticker  = $(if $(strip $(probe_ticker)),,$(error TICKER is required, e.g. make probe TICKER=KXBTCD-...))

.PHONY: help record record-prod dump dump-prod dump-digest replay probe probe-prod markets markets-prod check build

help:
	@echo "Sum100 operator recipes (wrap cargo run --release --)."
	@echo "Demo is default. Production is the *-prod targets only."
	@echo ""
	@echo "  make record TICKERS=YOUR-DEMO-TICKER"
	@echo "  make dump-prod TICKERS=... SECONDS=70"
	@echo "  make record-prod TICKERS=... SECONDS=70"
	@echo "  make dump-digest TICKERS=... SECONDS=180"
	@echo "  make replay FILE=data/production/kalshi-YYYY-MM-DD.ndjson.gz"
	@echo "  make probe-prod TICKER=..."
	@echo "  make markets-prod SERIES=KXBTCD"
	@echo "  make check"
	@echo "  make build"
	@echo ""
	@echo "Knobs: TICKERS TICKER SECONDS OUT FILE DIGEST SESSION PACE SERIES EXTRA VENUE"
	@echo "FILE defaults to data/production/kalshi-YYYY-MM-DD.ndjson.gz (UTC today)"
	@echo "No SECONDS means run until Ctrl-C (record/dump). EXTRA='--interval-ms 250'"

record:
	$(need-tickers)$(CARGO_RUN) record --venue $(VENUE) --tickers $(TICKERS) --out $(OUT) $(seconds) $(EXTRA)

record-prod:
	$(need-tickers)$(CARGO_RUN) record --venue $(VENUE) --prod --tickers $(TICKERS) --out $(OUT) $(seconds) $(EXTRA)

dump:
	$(need-tickers)$(CARGO_RUN) dump --venue $(VENUE) --tickers $(TICKERS) --out $(OUT) $(seconds) $(digest_flag) $(EXTRA)

dump-prod:
	$(need-tickers)$(CARGO_RUN) dump --venue $(VENUE) --prod --tickers $(TICKERS) --out $(OUT) $(seconds) $(digest_flag) $(EXTRA)

dump-digest:
	$(need-tickers)$(CARGO_RUN) dump --venue $(VENUE) --prod --tickers $(TICKERS) --out $(OUT) $(seconds) --digest-out $(digest_file) $(EXTRA)

replay:
	$(CARGO_RUN) replay --venue $(VENUE) --file $(FILE) --verify --expect-file $(digest_file) $(tickers_flag) $(session_flag) $(pace_flag) $(EXTRA)

probe:
	$(need-ticker)$(CARGO_RUN) probe --ticker $(probe_ticker) $(seconds) $(EXTRA)

probe-prod:
	$(need-ticker)$(CARGO_RUN) probe --prod --ticker $(probe_ticker) $(seconds) $(EXTRA)

markets:
	$(CARGO_RUN) markets --series $(SERIES) $(EXTRA)

markets-prod:
	$(CARGO_RUN) markets --prod --series $(SERIES) $(EXTRA)

check:
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test

build:
	cargo build --release
