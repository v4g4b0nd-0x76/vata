use std::{env, process, time::Duration};

use vata::{
    XdpConf,
    arena_alloc::Core,
    xdp::{XdpProbeStats, start_zerocopy},
};

fn usage() -> ! {
    eprintln!("usage: xdp_probe <interface> <udp-port> <packets> <timeout-seconds>");
    process::exit(2);
}

fn parse<T: std::str::FromStr>(value: Option<String>) -> T {
    match value.and_then(|value| value.parse().ok()) {
        Some(value) => value,
        None => usage(),
    }
}

fn print_stats(stats: &XdpProbeStats) {
    let seconds = stats.elapsed.as_secs_f64();
    let gbit_per_second = if seconds == 0.0 {
        0.0
    } else {
        stats.payload_bytes as f64 * 8.0 / seconds / 1_000_000_000.0
    };
    println!("received_packets={}", stats.packets);
    println!("payload_bytes={}", stats.payload_bytes);
    println!("elapsed_seconds={seconds:.6}");
    println!("payload_gbit_per_second={gbit_per_second:.3}");
    println!("kernel_rx_dropped={}", stats.rx_dropped);
    println!("kernel_rx_invalid_descs={}", stats.rx_invalid_descs);
    println!("kernel_rx_ring_full={}", stats.rx_ring_full);
    println!(
        "kernel_rx_fill_ring_empty_descs={}",
        stats.rx_fill_ring_empty_descs
    );
    println!("kernel_drops_total={}", stats.kernel_drops());
}

fn main() {
    let mut args = env::args().skip(1);
    let Some(interface) = args.next() else {
        usage();
    };
    let udp_port = parse(args.next());
    let packet_limit: usize = parse(args.next());
    let timeout_seconds: u64 = parse(args.next());
    if args.next().is_some() || packet_limit == 0 || timeout_seconds == 0 {
        usage();
    }

    let config = XdpConf {
        interface: interface.clone(),
        udp_port,
        cpu: None,
    };
    let mut ingress = start_zerocopy(&config).unwrap_or_else(|error| {
        eprintln!("xdp_probe_setup=failed");
        eprintln!("{error}");
        process::exit(1);
    });
    println!("strict_zero_copy_bind=confirmed");
    println!("interface={interface}");
    println!("queue=0");
    println!("udp_port={udp_port}");
    println!("expected_packets={packet_limit}");

    let core = Core::new(2, 0);
    let stats = ingress
        .run_for(&core, packet_limit, Duration::from_secs(timeout_seconds))
        .unwrap_or_else(|error| {
            eprintln!("{error}");
            process::exit(1);
        });
    print_stats(&stats);
    if stats.packets < packet_limit {
        eprintln!(
            "probe timed out before receiving every packet: expected {packet_limit}, got {}",
            stats.packets
        );
        process::exit(1);
    }
}
