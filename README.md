# Vata

Vata is an in-memory, preallocated record arena for fast local ingestion and
fan-out to readers. It is not a durable queue: a restart loses data, and slow
readers eventually hold up slab reuse.

Records are copied into fixed-size slabs. Writers publish a batch with one
cursor update; every reader sees every record. That makes it useful when
several local consumers need the same data, not when consumers should divide
work between themselves.

## Pick one ingress path

Run **one** ingress path per Vata process. The current startup sequence enters
the UDP listener first, so configuring both paths does not run them together.

### UDP listener

Use this as the default path. It uses normal Linux UDP sockets and `recvmmsg`:
each receiver owns a writer lane and appends its receive batch directly to the
arena.

```toml
[udp_listener]
port = 9000
receiver = 1
processor = 1 # must match receiver; the receiver owns the writer lane
cpu_cores = []
```

Use it when portability, a simple operational model, and ordinary UDP traffic
are more important than shaving the kernel network-stack work. It works on NICs
without XDP support and is the path to start with.

Its costs are normal UDP costs: the kernel places data in the socket receive
buffer, Vata receives it into a reusable buffer, then Vata copies the record to
the arena. Batching reduces syscall and cursor-publication overhead; it does
not make UDP reliable or durable. Kernel receive-buffer pressure and a slow
reader can still cause loss or backpressure.

### XDP / AF_XDP listener

Use this only on Linux, after the UDP path is measured as the bottleneck and
the target NIC has been shown to support it. The XDP program passes all traffic
except UDP packets for the configured port; matching packets are redirected to
an AF_XDP socket on RX queue 0.

```toml
[xdp_conf]
interface = "enp4s0"
udp_port = 9000
cpu = 2 # optional CPU pin
```

AF_XDP can avoid the NIC-to-userspace copy only in zero-copy mode. Vata still
copies the packet from UMEM into its arena, so this is not end-to-end zero-copy.
The normal listener permits the kernel's AF_XDP copy-mode fallback. Use the
strict probe to establish whether zero-copy is actually available:

```sh
make -C vata xdp-probe XDP_IFACE=enp4s0 XDP_PORT=9000 \
  XDP_PACKETS=20000 XDP_TIMEOUT_SECS=30
```

The target builds as the current user and asks for `sudo` only to run the
probe. Send UDP traffic from another host; localhost does not exercise NIC DMA.
Choose an unused port: matching traffic is diverted from the normal UDP stack
while the probe runs.

XDP has stricter operational requirements: Linux capabilities, eBPF/AF_XDP
support, a compatible NIC driver, and queue-aware deployment. This implementation
has one AF_XDP socket on queue 0; it is not a multi-queue scale-out path yet.

On the current test host, the strict probe on `enp4s0` failed before XDP was
attached:

```text
AF_XDP strict zero-copy bind ... failed: Operation not supported (EOPNOTSUPP)
```

That means this interface cannot use AF_XDP zero-copy here. It does not prove
that AF_XDP copy mode is unavailable, but it does rule out a NIC-DMA-to-UMEM
throughput result on this hardware.

## Measurements so far

These are local measurements from the stated workloads, not promises for a
different CPU, NIC, packet size, or reader count.

| Workload | Result | What it means |
| --- | ---: | --- |
| Direct arena, 100k records of 1 KiB | `append_bytes`: 9.829 ms; `append_batch(64)`: 5.841 ms write, 2.911 ms read | Publishing the cursor once per batch helps this synthetic in-memory path. |
| `make -C vata workload`, 2M records of 1 KiB, four readers | 9.965 ms median per 100k-record write batch; zero steady-state allocations | The workload moves about 512 MB through writer and reader copies per reported batch. It measures memory/cache work, not a network link. |
| `make -C vata udp-workload`, loopback, 100k UDP packets of 1 KiB | 100k received, no loss, about 563k packets/s or 0.576 GB/s payload | This covers `recvmmsg` → arena writer → reader on localhost. It is not NIC throughput. |
| Strict AF_XDP zero-copy probe on `enp4s0` | `EOPNOTSUPP` at bind | Zero-copy was unavailable, so no AF_XDP NIC throughput was measured. |

Run the two repeatable local checks with:

```sh
make -C vata workload
make -C vata udp-workload
```

For a real network result, use a separate sender on the target NIC, record link
speed, packet size, sender rate, CPU pinning, packet loss, and the exact commit.
Compare only runs with the same setup.

## Practical guidance

Start with the UDP listener and the end-to-end workload. Move to XDP only when
the measurements show UDP receive overhead matters and the deployment has a
supported NIC. If higher throughput is still needed, scale by RX queue with one
writer lane per queue; adding writers to one shared queue is not the next win.
