mod arena;
mod conf;
use std::fmt::Display;

pub use arena::*;
pub use conf::*;
#[derive(Debug)]
pub enum VataErr {
    ConfLoadFailed(String),
    XdpUnavailable(String),
}
impl Display for VataErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VataErr::ConfLoadFailed(err) => writeln!(f, "failed to load config: {}", err),
            VataErr::XdpUnavailable(err) => writeln!(f, "XDP is unavailable: {}", err),
        }
    }
}
