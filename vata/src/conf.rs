use serde::Deserialize;

use crate::VataErr;

#[derive(Deserialize)]
pub struct Conf {
    pub core_conf: CoreConf,
    pub telemetry_conf: TelemetryConf,
}
#[derive(Deserialize, Default)]
pub struct TelemetryConf {
    pub report_interval_ms: u64,
}
#[derive(Deserialize)]
pub struct CoreConf {
    pub max_readers: usize,
    pub cap: usize,
}

impl Conf {
    pub fn load() -> Result<Self, VataErr> {
        let conf_str = std::fs::read_to_string("conf.toml")
            .map_err(|err| VataErr::ConfLoadFailed(err.to_string()))?;
        let conf: Conf =
            toml::from_str(&conf_str).map_err(|err| VataErr::ConfLoadFailed(err.to_string()))?;
        Ok(conf)
    }
}
