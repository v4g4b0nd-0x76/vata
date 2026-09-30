mod arena;
mod conf;
pub mod cpu_tuning;
mod listener;
pub mod xdp;
use std::fmt::Display;

pub use arena::*;
pub use conf::*;
pub use listener::*;
#[derive(Debug)]
pub enum VataErr {
    ConfLoadFailed(String),
    XdpUnavailable(String),
    SpwanUdpReceiver(String),
    CpuTuningFailed(String),
}
impl Display for VataErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VataErr::ConfLoadFailed(err) => writeln!(f, "failed to load config: {}", err),
            VataErr::XdpUnavailable(err) => writeln!(f, "XDP is unavailable: {}", err),
            VataErr::SpwanUdpReceiver(err) => writeln!(f, "failed to spawn udp listener: {}", err),
            VataErr::CpuTuningFailed(err) => writeln!(f, "CPU tuning failed: {}", err),
        }
    }
}
