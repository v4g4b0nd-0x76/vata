# Compiler environment and runtime configuration hooks
CARGO       := cargo
RUSTFLAGS   := RUSTFLAGS="-C target-cpu=native -g"
# Keep a shell-wide jemalloc configuration out of Cargo and perf.
unexport MALLOC_CONF
ifeq ($(shell uname -s),Linux)
MALLOC_CONF := MALLOC_CONF="abort_conf:true,thp:always,metadata_thp:always"
endif
BENCH_NAME  := arena_bench

.PHONY: all build bench workload profile-perf flamegraph clean

all: build

build:
	rustup override set nightly
	$(RUSTFLAGS) $(CARGO) build --release --benches

bench:
	$(RUSTFLAGS) $(MALLOC_CONF) $(CARGO) bench --bench $(BENCH_NAME)

workload:
	$(RUSTFLAGS) $(MALLOC_CONF) $(CARGO) bench --bench arena_workload

## 1. Call raw perf to analyze IPC, cache-misses, and branch mispredictions
profile-perf:
	@set -e; \
	BENCH_BIN="$$( env -u MALLOC_CONF $(RUSTFLAGS) $(CARGO) bench --no-run --bench $(BENCH_NAME) --message-format=json | jq -er 'select(.reason == "compiler-artifact" and .target.name == "$(BENCH_NAME)") | .executable')"; \
	env -u MALLOC_CONF sudo perf stat -e instructions,cycles,cache-references,cache-misses,branches,branch-misses -- env $(MALLOC_CONF) "$$BENCH_BIN" --bench --profile-time 5

## 2. Call perf c2c to investigate false sharing inside telemetry shards
profile-c2c:
	@set -e; \
	BENCH_BIN="$$( env -u MALLOC_CONF $(RUSTFLAGS) $(CARGO) bench --no-run --bench $(BENCH_NAME) --message-format=json | jq -er 'select(.reason == "compiler-artifact" and .target.name == "$(BENCH_NAME)") | .executable')"; \
	env -u MALLOC_CONF sudo perf c2c record -- env $(MALLOC_CONF) "$$BENCH_BIN" --bench --profile-time 3
	sudo perf c2c report --stdio

## 3. Generate a vector SVG Flamegraph to pinpoint execution hot paths
flamegraph: build
	@echo "==> Sampling execution stacks to build flamegraph.svg..."
	$(RUSTFLAGS) $(MALLOC_CONF) cargo flamegraph --bench $(BENCH_NAME) -- --bench
	@echo "==> Complete! Open flamegraph.svg in your browser to inspect code paths."

clean:
	$(CARGO) clean
	rm -f flamegraph.svg perf.data perf.data.old
