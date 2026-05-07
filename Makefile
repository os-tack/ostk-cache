# ostk-cache — kernel adapter for unowned process boundaries
#
# See docs/draft/ostk-cache-adapter.md (in haystack) for the spec.
# See ~/.claude/plans/yes-exactly-now-that-lazy-thimble.md for the plan.
#
# Quick start:
#   make build-release
#   make run-rebuild-local   # in one terminal
#   make export-url          # copy the export line into your claude-code shell
#   ... use claude-code as normal ...
#   make stats-rebuild-local # see the A/B ledger
#
# Variables:
#   PROXY_PORT — TCP port the proxy listens on (default 8089)
#   ANTHROPIC_BASE_URL — what the harness should point at (printed by `make export-url`)

.DEFAULT_GOAL := help

PROXY_PORT ?= 8089
DEBUG_BIN  := target/debug/ostk-cache
RELEASE_BIN := target/release/ostk-cache
STATS_BIN  := target/release/stats

CARGO ?= cargo

# ── build ────────────────────────────────────────────────────────────────────

.PHONY: build build-release rebuild test test-rebuild test-tail test-kernel-client \
        test-standalone clean

build:
	$(CARGO) build --bins

build-release:
	$(CARGO) build --release --bins

rebuild: clean build-release

clean:
	$(CARGO) clean

# ── tests ────────────────────────────────────────────────────────────────────

test:
	$(CARGO) test

test-rebuild:
	$(CARGO) test --lib rebuild::

test-tail:
	$(CARGO) test --lib transcript_tail::

test-kernel-client:
	$(CARGO) test --lib kernel_client::

test-standalone:
	$(CARGO) test --lib standalone::

# ── run targets (foreground proxy; Ctrl-C to stop) ───────────────────────────

# Each target builds the release binary first, then runs it with the env vars
# for that test configuration. Run in one terminal; in another set the
# ANTHROPIC_BASE_URL and use claude-code as normal.

.PHONY: run-passthrough run-mutate \
        run-rebuild-local run-rebuild-kernel \
        run-rebuild-local-tail run-rebuild-kernel-tail \
        run-tail-only

# L0 — passthrough (control). Byte-identical forward; n=47 baseline.
run-passthrough: build-release
	@echo "[ostk-cache] mode=passthrough — control baseline"
	@echo "[ostk-cache] in another terminal: $$(make -s export-url)"
	PROXY_PORT=$(PROXY_PORT) OSTK_CACHE_PASSTHROUGH=1 ./$(RELEASE_BIN)

# Default — file-handle SHA-256 substitution (the previous default).
run-mutate: build-release
	@echo "[ostk-cache] mode=mutate — file-handle rewriter only"
	@echo "[ostk-cache] in another terminal: $$(make -s export-url)"
	PROXY_PORT=$(PROXY_PORT) ./$(RELEASE_BIN)

# L1 standalone — request rebuild, no kernel IPC, no transcript tail.
run-rebuild-local: build-release
	@echo "[ostk-cache] mode=rebuild_local — Layer 1 standalone"
	@echo "[ostk-cache] in another terminal: $$(make -s export-url)"
	PROXY_PORT=$(PROXY_PORT) OSTK_CACHE_REBUILD=1 ./$(RELEASE_BIN)

# L2 federated — kernel IPC for live envelope; falls back to L1 if kernel down.
run-rebuild-kernel: build-release
	@echo "[ostk-cache] mode=rebuild_kernel — Layer 2 federated (requires .ostk/ostk.sock)"
	@echo "[ostk-cache] in another terminal: $$(make -s export-url)"
	PROXY_PORT=$(PROXY_PORT) OSTK_CACHE_REBUILD=kernel ./$(RELEASE_BIN)

# L1 + L3 — standalone rebuild plus transcript tailing.
run-rebuild-local-tail: build-release
	@echo "[ostk-cache] mode=rebuild_local + tail — Layer 1 + Layer 3 Pattern A"
	@echo "[ostk-cache] in another terminal: $$(make -s export-url)"
	PROXY_PORT=$(PROXY_PORT) OSTK_CACHE_REBUILD=1 OSTK_CACHE_TAIL_TRANSCRIPT=1 ./$(RELEASE_BIN)

# L2 + L3 — federated rebuild plus transcript tailing (full architectural test).
run-rebuild-kernel-tail: build-release
	@echo "[ostk-cache] mode=rebuild_kernel + tail — Layer 2 + Layer 3 Pattern A"
	@echo "[ostk-cache] in another terminal: $$(make -s export-url)"
	PROXY_PORT=$(PROXY_PORT) OSTK_CACHE_REBUILD=kernel OSTK_CACHE_TAIL_TRANSCRIPT=1 ./$(RELEASE_BIN)

# Tail-only — no rebuild, just observe the harness transcript. Useful for
# verifying the tailer can locate the active session file.
run-tail-only: build-release
	@echo "[ostk-cache] mode=mutate + tail — observation only"
	@echo "[ostk-cache] in another terminal: $$(make -s export-url)"
	PROXY_PORT=$(PROXY_PORT) OSTK_CACHE_TAIL_TRANSCRIPT=1 ./$(RELEASE_BIN)

# ── stats / A-B analysis ─────────────────────────────────────────────────────

# Reads .ostk/memory/ledger.jsonl in the current working directory. Run from
# the same workspace where you exercised the proxy.

.PHONY: stats stats-all stats-passthrough stats-mutate \
        stats-rebuild-local stats-rebuild-kernel build-stats

build-stats:
	$(CARGO) build --release --bin stats

stats: stats-all

stats-all: build-stats
	./$(STATS_BIN) --mode all

stats-passthrough: build-stats
	./$(STATS_BIN) --mode passthrough

stats-mutate: build-stats
	./$(STATS_BIN) --mode mutate

stats-rebuild-local: build-stats
	./$(STATS_BIN) --mode rebuild_local

stats-rebuild-kernel: build-stats
	./$(STATS_BIN) --mode rebuild_kernel

# Turns where rebuild was configured but couldn't apply (e.g. first-turn
# requests with no prior history). These rows reflect *unmodified* body
# forwards under rebuild config — distinct from the passthrough baseline.
stats-rebuild-skip: build-stats
	./$(STATS_BIN) --mode rebuild_skip

# ── helpers ──────────────────────────────────────────────────────────────────

.PHONY: export-url ledger-tail ledger-clear help

# Print the export line the user should run in the claude-code shell.
# Use as: `eval "$(make -s export-url)"` or copy/paste.
export-url:
	@echo "export ANTHROPIC_BASE_URL=http://127.0.0.1:$(PROXY_PORT)"

# Show the last 5 ledger rows for quick verification.
ledger-tail:
	@if [ -f .ostk/memory/ledger.jsonl ]; then \
	  tail -5 .ostk/memory/ledger.jsonl | sed 's/^/  /'; \
	else \
	  echo "no ledger at .ostk/memory/ledger.jsonl"; \
	fi

# DESTRUCTIVE — wipes the ledger. Use only at the start of a clean A/B run.
ledger-clear:
	@printf "This will delete .ostk/memory/ledger.jsonl in $$(pwd). Type 'yes' to confirm: " && \
	read ans && [ "$$ans" = "yes" ] && rm -f .ostk/memory/ledger.jsonl && echo "[ledger cleared]" || echo "[aborted]"

help:
	@echo "ostk-cache — kernel adapter for unowned process boundaries"
	@echo ""
	@echo "Build:"
	@echo "  make build               — debug build"
	@echo "  make build-release       — release build (used by all run-* targets)"
	@echo "  make rebuild             — clean + build-release"
	@echo "  make clean               — cargo clean"
	@echo ""
	@echo "Test:"
	@echo "  make test                — full test suite"
	@echo "  make test-rebuild        — only rebuild module tests"
	@echo "  make test-tail           — only transcript_tail module tests"
	@echo "  make test-kernel-client  — only kernel_client module tests"
	@echo "  make test-standalone     — only standalone module tests"
	@echo ""
	@echo "Run (foreground proxy; Ctrl-C to stop; in another terminal: make export-url):"
	@echo "  make run-passthrough         — L0  control (byte-identical forward)"
	@echo "  make run-mutate              — file-handle rewriter (legacy default)"
	@echo "  make run-rebuild-local       — L1  standalone request rebuild"
	@echo "  make run-rebuild-kernel      — L2  federated (kernel IPC)"
	@echo "  make run-rebuild-local-tail  — L1 + L3 Pattern A"
	@echo "  make run-rebuild-kernel-tail — L2 + L3 Pattern A (full)"
	@echo "  make run-tail-only           — tailer-only diagnostic run"
	@echo ""
	@echo "A/B stats (run in the workspace whose ledger you want):"
	@echo "  make stats-all               — all modes"
	@echo "  make stats-passthrough       — passthrough rows only"
	@echo "  make stats-mutate            — mutate rows only"
	@echo "  make stats-rebuild-local     — rebuild_local rows only"
	@echo "  make stats-rebuild-kernel    — rebuild_kernel rows only"
	@echo ""
	@echo "Helpers:"
	@echo "  make export-url          — print the ANTHROPIC_BASE_URL export line"
	@echo "  make ledger-tail         — show last 5 ledger rows"
	@echo "  make ledger-clear        — wipe ledger (destructive; prompts for confirmation)"
	@echo ""
	@echo "Variables:"
	@echo "  PROXY_PORT=$(PROXY_PORT)    — change with: make run-... PROXY_PORT=NNNN"
