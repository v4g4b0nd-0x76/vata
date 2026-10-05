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
    Ingress(String),
    Client(String),
    CpuTuningFailed(String),
}
impl Display for VataErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VataErr::ConfLoadFailed(err) => writeln!(f, "failed to load config: {}", err),
            VataErr::XdpUnavailable(err) => writeln!(f, "XDP is unavailable: {}", err),
            VataErr::Ingress(err) => writeln!(f, "failed to start ingress: {}", err),
            VataErr::Client(err) => writeln!(f, "failed to start client endpoint: {}", err),
            VataErr::CpuTuningFailed(err) => writeln!(f, "CPU tuning failed: {}", err),
        }
    }
}
