use serde::Deserialize;

use crate::VataErr;

#[derive(Deserialize)]
pub struct Conf {
    pub core_conf: CoreConf,
    pub telemetry_conf: TelemetryConf,
    pub xdp_conf: Option<XdpConf>,
    pub udp_listener: Option<UdpListener>,
}
#[derive(Deserialize, Default)]
pub struct UdpListener {
    pub port: u16,
    pub processor: usize,
    pub receiver: usize,
}
#[derive(Deserialize, Default)]
pub struct TelemetryConf {
    pub report_interval_ms: u64,
}
#[derive(Deserialize)]
pub struct CoreConf {
    pub max_readers: usize,
    pub cap: usize, // user provide gb of pre-alloc we convert to number of slabs
}
#[derive(Deserialize)]
pub struct XdpConf {
    pub interface: String,
    pub udp_port: u16,
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

#[cfg(test)]
mod tests {
    use super::Conf;

    #[test]
    fn xdp_conf_accepts_an_explicit_interface_and_udp_port() {
        let conf: Conf = toml::from_str(
            r#"
            [core_conf]
            max_readers = 1
            cap = 2

            [telemetry_conf]
            report_interval_ms = 1000

            [xdp_conf]
            interface = "eth0"
            udp_port = 5353
            "#,
        )
        .unwrap();

        let xdp = conf.xdp_conf.expect("xdp configuration should be present");
        assert_eq!(xdp.interface, "eth0");
        assert_eq!(xdp.udp_port, 5353);
    }

    #[test]
    fn xdp_conf_is_absent_when_the_section_is_not_configured() {
        let conf: Conf = toml::from_str(
            r#"
            [core_conf]
            max_readers = 1
            cap = 2

            [telemetry_conf]
            report_interval_ms = 1000
            "#,
        )
        .unwrap();

        assert!(conf.xdp_conf.is_none());
    }
}
