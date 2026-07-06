CARGO ?= cargo
BIN   := target/debug/doover

.PHONY: test fmt clippy unit build e2e canary

test: fmt clippy unit e2e
	@echo "ALL TEST SUITES GREEN"

fmt:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

unit:
	$(CARGO) test --workspace
	# deterministic newline-rich fuzz slice in the main gate (the full fuzz
	# matrix lives on the fuzz-hunt branch)
	DOOVER_FUZZ_ITERS=5000 DOOVER_FUZZ_SEED=42 $(CARGO) test -p doover-core --test fuzz_hunt -- --nocapture > target/fuzz-gate.log 2>&1 || (tail -5 target/fuzz-gate.log; exit 1)

build:
	$(CARGO) build --workspace

e2e: build
	DOOVER_BIN=$(abspath $(BIN)) bats tests/e2e

# Proves failure reporting works: the canary test MUST fail under
# DOOVER_CI_CANARY=1. This target succeeds only if that failure is observed.
canary:
	@mkdir -p target
	@if DOOVER_CI_CANARY=1 $(CARGO) test -p doover-core ci_canary > target/canary.log 2>&1; then \
		echo "FATAL: canary test passed under DOOVER_CI_CANARY=1 — failure reporting is broken"; \
		exit 1; \
	else \
		echo "OK: canary failed as designed — test failure reporting verified"; \
	fi
