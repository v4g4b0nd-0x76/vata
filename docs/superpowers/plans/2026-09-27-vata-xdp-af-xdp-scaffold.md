# Vata XDP + AF_XDP Scaffold Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Vata optionally load an Aya XDP program configured with a UDP port and wire it to one AF_XDP socket.

**Architecture:** A root Cargo workspace keeps the existing `vata` host crate as the default build and adds an eBPF-only `vata-ebpf` package. The host build script compiles and embeds the BPF ELF only for Linux `xdp` builds; a small host module owns Aya loading, AF_XDP UMEM/socket setup, map population, and link lifetime.

**Tech Stack:** Rust 2024, Aya 0.14, aya-ebpf 0.2, aya-build, xdp 0.8, Linux XDP/AF_XDP.

**Spec:** `docs/superpowers/specs/2026-09-27-vata-xdp-ingress-design.md`

## Global Constraints

- Keep `vata`'s existing default build and macOS path free of Aya/XDP runtime requirements.
- Enable the ingress only with Cargo feature `xdp` and only on Linux.
- Require explicit `[xdp] interface` and `udp_port`; do not invent defaults.
- Attach in XDP driver mode and require AF_XDP zero-copy; return an error instead of falling back.
- Redirect only matching UDP destination-port packets; return `XDP_PASS` for every other packet.
- Do not change `Core`, copy packet data into the arena, add a receive worker, add multi-queue support, or touch the user-deleted `README.md`.

## Review Focus

- Missing `[xdp]` configuration must preserve the existing startup path.
- An `[xdp]` section in a binary compiled without `xdp` must produce a clear error, not silently ignore it.
- Malformed/truncated Ethernet, IPv4, IPv6, or UDP packets must pass rather than read beyond packet bounds.
- A requested interface without driver-mode XDP or AF_XDP zero-copy must fail before it redirects traffic.
- Program/link/UMEM/socket resources must stay alive for the process lifetime so packet ownership is not invalidated.

### Task 1: Add feature-gated configuration and workspace build wiring

**Files:**
- Create: `Cargo.toml`
- Create: `vata/build.rs`
- Create: `vata-ebpf/Cargo.toml`
- Create: `vata-ebpf/src/main.rs`
- Modify: `vata/Cargo.toml`
- Modify: `vata/src/conf.rs`
- Modify: `vata/src/lib.rs`
- Test: `vata/src/conf.rs`

**Interfaces:**
- Produces: `XdpConf { interface: String, udp_port: u16 }` and `Conf::xdp_conf: Option<XdpConf>`.
- Produces: a build-time `vata-ebpf` ELF embedded by the host only for `cfg(all(target_os = "linux", feature = "xdp"))`.

- [ ] **Step 1: Write failing configuration tests**

Add tests showing TOML deserializes `[xdp] interface = "eth0"` and `udp_port = 5353`, and that absent `[xdp]` leaves `xdp_conf` as `None`.

- [ ] **Step 2: Run the configuration tests to verify they fail**

Run: `cargo test --manifest-path vata/Cargo.toml xdp_conf`

Expected: FAIL because `Conf` has no `xdp_conf` field.

- [ ] **Step 3: Add `XdpConf` and optional configuration support in `vata/src/conf.rs`**

Use `Option<XdpConf>` with Serde default behavior; do not assign interface or port defaults. Add an `XdpUnavailable` `VataErr` variant for configurations requesting XDP in an unsupported build/platform.

- [ ] **Step 4: Add the minimal Cargo workspace and feature-gated build linkage**

Create a root workspace whose default member is `vata`. Add optional Aya 0.14, xdp 0.8, and build-only Aya compilation support to `vata`; add `vata-ebpf` with `#![no_std]`, `#![no_main]`, aya-ebpf 0.2, a panic handler, and a placeholder named XDP entry point. Make `vata/build.rs` compile/embed the BPF object only for Linux `xdp` feature builds using Aya's build support.

- [ ] **Step 5: Run the configuration and default-build checks**

Run: `cargo test --manifest-path vata/Cargo.toml xdp_conf && cargo check --manifest-path vata/Cargo.toml`

Expected: PASS; the existing default host build does not require eBPF tooling.

- [ ] **Step 6: Commit Task 1**

```bash
git add Cargo.toml vata/Cargo.toml vata/build.rs vata/src/conf.rs vata/src/lib.rs vata-ebpf
git commit -m "feat: scaffold optional XDP build"
```

### Task 2: Create the bounded XDP filter and host listener linkage

**Files:**
- Create: `vata/src/xdp.rs`
- Modify: `vata-ebpf/src/main.rs`
- Modify: `vata/src/main.rs`
- Modify: `vata/src/lib.rs`
- Test: `vata/src/xdp.rs`

**Interfaces:**
- Consumes: `XdpConf { interface, udp_port }` from Task 1.
- Produces: `start(config: &XdpConf) -> Result<XdpIngress, VataErr>` under Linux `xdp`; `XdpIngress` owns the Aya BPF object, XDP link, UMEM, and AF_XDP socket.

- [ ] **Step 1: Write failing host-side configuration/dispatch tests**

Add tests for the pure configuration gate: no XDP config does not request startup; XDP config reaches the `start` dispatch only in a Linux `xdp` build; unsupported builds return `XdpUnavailable` before any privileged operation.

- [ ] **Step 2: Run the dispatch tests to verify they fail**

Run: `cargo test --manifest-path vata/Cargo.toml xdp`

Expected: FAIL because the `xdp` module and dispatch gate do not exist.

- [ ] **Step 3: Implement the eBPF `vata_xdp` entry point and maps**

Define a single-element UDP-port map and `XskMap`. In `vata_ebpf::vata_xdp`, prove bounds before every header access, accept IPv4 and IPv6 UDP destination-port matches only, and invoke the XSK redirect using `ctx.rx_queue_index()`. Return `XDP_PASS` for invalid, non-UDP, or non-matching packets.

- [ ] **Step 4: Implement `start(config: &XdpConf) -> Result<XdpIngress, VataErr>`**

Load the embedded object with Aya, create one AF_XDP UMEM/socket bound to queue 0 using `xdp`, populate the port map and `XskMap`, verify zero-copy, then attach `vata_xdp` to `config.interface` using driver mode. Store all handles in `XdpIngress`; no receive loop is started.

- [ ] **Step 5: Call the listener from `main.rs` only when `[xdp]` exists**

Store `XdpIngress` in the async main scope for its lifetime. On non-Linux or without the Cargo feature, return `XdpUnavailable` if configuration requests XDP; do not alter normal startup when the section is absent.

- [ ] **Step 6: Run host and BPF checks**

Run: `cargo test --manifest-path vata/Cargo.toml && cargo check --manifest-path vata/Cargo.toml --features xdp && cargo check --manifest-path vata-ebpf/Cargo.toml --target bpfel-unknown-none`

Expected: PASS on a Linux setup with the Aya prerequisites and BPF target/linker installed. If prerequisite tooling is absent, default tests still pass and the exact missing tool is reported.

- [ ] **Step 7: Commit Task 2**

```bash
git add vata/src/xdp.rs vata/src/main.rs vata/src/lib.rs vata-ebpf/src/main.rs
git commit -m "feat: wire configured XDP listener"
```
