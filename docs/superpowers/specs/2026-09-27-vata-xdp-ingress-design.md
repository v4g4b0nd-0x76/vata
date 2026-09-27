# Vata XDP ingress design

## Goal

Add an optional Linux-only XDP + AF_XDP ingress scaffold to Vata. It loads a
configured interface and UDP destination port into the XDP program and attaches
it without changing the arena or copying packet payloads into it.

## Scope

- Keep `vata` as the host binary and add a separate `vata-ebpf` crate for the
  verifier-constrained XDP program.
- Use Aya for loading/attaching the XDP program and its `XskMap`; use the Rust
  `xdp` crate only for AF_XDP socket/UMEM setup.
- Add optional `xdp` feature-gated dependencies, so existing macOS and normal
  `cargo test` users do not need Linux eBPF support.
- Add an optional `[xdp]` config section with `interface` and `udp_port`.
- At startup with `--features xdp` on Linux, load the compiled BPF object,
  write `udp_port` to a BPF map, create the AF_XDP socket/UMEM, add its FD to
  `XskMap`, then attach XDP to `interface` in driver mode.
- In the BPF program, only UDP packets for the configured destination port are
  redirected to AF_XDP. All other traffic returns `XDP_PASS`.

## Explicit non-goals

- No packet-to-`Core::append_bytes` copy, payload parser, worker loop, metrics,
  multi-queue scaling, generic-mode fallback, or DPDK.
- The XDP mode must fail clearly when driver-mode or AF_XDP zero-copy is
  unavailable; it must not silently take ownership of traffic through a slower
  fallback path.

## Boundaries

`conf.rs` owns deserialization. A new Linux-only host ingress module owns
resource lifetime and Aya/XSK setup. `vata-ebpf` only validates bounded packet
headers, reads the port map, and redirects through `XskMap`; it never calls the
arena. `main.rs` only selects the optional ingress mode after loading `Conf`.

## Validation

- Unit-test that the optional XDP configuration accepts a valid `u16` port and
  that its absence preserves the existing configuration path.
- Compile host code and BPF crate separately on Linux.
- Provide a privileged manual attach command for a real interface; it must
  report unsupported driver/zero-copy capability rather than falling back.
