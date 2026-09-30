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
    #[serde(default)]
    pub cpu_cores: Vec<usize>,
}
#[derive(Deserialize, Default)]
pub struct TelemetryConf {
    pub report_interval_ms: u64,
}
#[derive(Deserialize)]
pub struct CoreConf {
    pub max_readers: usize,
    pub cap: usize, // user provide gb of pre-alloc we convert to number of slabs
    pub numa_cpu: Option<usize>,
}
#[derive(Deserialize)]
pub struct XdpConf {
    pub interface: String,
    pub udp_port: u16,
    pub cpu: Option<usize>,
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

    #[test]
    fn ingress_cpu_tuning_accepts_pinned_receivers_and_first_touch_cpu() {
        let conf: Conf = toml::from_str(
            r#"
            [core_conf]
            max_readers = 2
            cap = 2
            numa_cpu = 2

            [telemetry_conf]
            report_interval_ms = 1000

            [udp_listener]
            port = 9000
            processor = 2
            receiver = 2
            cpu_cores = [4, 6]

            [xdp_conf]
            interface = "eth0"
            udp_port = 5353
            cpu = 4
            "#,
        )
        .unwrap();

        assert_eq!(conf.core_conf.numa_cpu, Some(2));
        assert_eq!(conf.udp_listener.unwrap().cpu_cores, [4, 6]);
        assert_eq!(conf.xdp_conf.unwrap().cpu, Some(4));
    }
}
