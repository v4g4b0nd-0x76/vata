# Compiler environment and runtime configuration hooks
CARGO       := cargo
RUSTFLAGS   := RUSTFLAGS="-C target-cpu=native -g"
# Keep a shell-wide jemalloc configuration out of Cargo and perf.
unexport MALLOC_CONF
ifeq ($(shell uname -s),Linux)
MALLOC_CONF := MALLOC_CONF="abort_conf:true,thp:always,metadata_thp:always"
endif
BENCH_NAME  := arena_bench
XDP_IFACE ?=
XDP_PORT ?=
XDP_PACKETS ?= 100000
XDP_TIMEOUT_SECS ?= 30

.PHONY: all build bench workload udp-workload xdp-probe profile-perf flamegraph clean

all: build

build:
	rustup override set nightly
	$(RUSTFLAGS) $(CARGO) build --release --benches

bench:
	$(RUSTFLAGS) $(MALLOC_CONF) $(CARGO) bench --bench $(BENCH_NAME)

workload:
	$(RUSTFLAGS) $(MALLOC_CONF) $(CARGO) bench --bench arena_workload

udp-workload:
	$(RUSTFLAGS) $(MALLOC_CONF) $(CARGO) bench --bench udp_workload

xdp-probe:
	@test -n "$(XDP_IFACE)" || (echo "set XDP_IFACE, for example enp4s0"; exit 2)
	@test -n "$(XDP_PORT)" || (echo "set XDP_PORT, for example 9000"; exit 2)
	$(RUSTFLAGS) $(CARGO) build --release --features xdp --bin xdp_probe
	sudo "$$($(CARGO) metadata --no-deps --format-version 1 | jq -r '.target_directory')/release/xdp_probe" $(XDP_IFACE) $(XDP_PORT) $(XDP_PACKETS) $(XDP_TIMEOUT_SECS)

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
