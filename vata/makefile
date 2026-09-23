# Compiler environment and runtime configuration hooks
CARGO       := cargo
RUSTFLAGS   := RUSTFLAGS="-C target-cpu=native -g"
MALLOC_CONF := MALLOC_CONF="thp:always,metadata_thp:always"
BENCH_NAME  := arena_bench

.PHONY: all build bench profile-perf flamegraph clean

all: build

build:
	rustup override set nightly
	$(RUSTFLAGS) $(CARGO) build --release --benches

bench:
	$(RUSTFLAGS) $(MALLOC_CONF) $(CARGO) bench --bench $(BENCH_NAME)

## 1. Call raw perf to analyze IPC, cache-misses, and branch mispredictions
profile-perf: build
	@echo "==> Recording raw hardware performance counter metrics..."
	sudo env $(MALLOC_CONF) perf_7.0 stat -e instructions,cycles,cache-references,cache-misses,branches,branch-misses \
		./target/release/deps/arena_bench-* --bench --profile-time 5

## 2. Call perf c2c to investigate false sharing inside telemetry shards
profile-c2c: build
	@echo "==> Recording Cache-to-Cache metrics (False Sharing verification)..."
	sudo env $(MALLOC_CONF) perf c2c record -- ./target/release/deps/$(BENCH_NAME)-* --bench --profile-time 3
	sudo perf c2c report --stdio

## 3. Generate a vector SVG Flamegraph to pinpoint execution hot paths
flamegraph: build
	@echo "==> Sampling execution stacks to build flamegraph.svg..."
	$(RUSTFLAGS) $(MALLOC_CONF) cargo flamegraph --bench $(BENCH_NAME) -- --bench
	@echo "==> Complete! Open flamegraph.svg in your browser to inspect code paths."

clean:
	$(CARGO) clean
	rm -f flamegraph.svg perf.data perf.data.old
