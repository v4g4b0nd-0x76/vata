use tikv_jemallocator::Jemalloc;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

fn main() {
    // TODO:: arena allocator
    // TODO: load config in user space
    // TODO: connection to nic on udp port provided in config
    // TODO: linker from nic mem to allocator with bump allocation
}
