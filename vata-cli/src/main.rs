use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::time::Duration;

use toml::Value;
use vata::Conf;
use vata::MAX_UDP_DATAGRAM;
use vata::client::VataClient;

const DEFAULT_BATCH: u16 = 64;
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_millis(1000);
const HELP: &str = r#"Vata CLI commands:
  add NAME IP TCP_PORT [UDP_PORT]     save a server
  add-udp NAME IP UDP_PORT            save a UDP-only target
  load-config PATH [NAME]             import [client]/[ingress] from Vata config
  servers                             list saved servers
  select NAME                         choose active server
  remove NAME                         delete saved server
  current                             show active server
  connect [BATCH]                     open TCP read_write connection
  ping                                TCP ping selected server
  write TEXT                          TCP write one record
  bulk PATH [CHUNK_BYTES]             TCP write file chunks
  read [TIMEOUT_MS]                   read one TCP delivery batch
  udp TEXT                            send one UDP datagram
  udp-bulk PATH [CHUNK_BYTES]         send file chunks over UDP
  state                               print state file path
  help                                show this help
  exit                                leave the REPL

Examples:
  vata-cli
  vata-cli add local 127.0.0.1 9100 8030
  vata-cli load-config conf.toml local
  vata-cli write hello
  vata-cli bulk ./events.bin 1024
  vata-cli udp-bulk ./packets.bin 1024
"#;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct State {
    active: Option<String>,
    servers: BTreeMap<String, ServerTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerTarget {
    tcp: Option<SocketAddr>,
    udp: Option<SocketAddr>,
}

struct Cli {
    state_path: PathBuf,
    state: State,
    client: Option<VataClient>,
    batch_size: u16,
}

enum CommandResult {
    Continue,
    Exit,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{HELP}");
        return Ok(());
    }

    let state_path = state_path();
    let state = load_state(&state_path)?;
    let mut cli = Cli {
        state_path,
        state,
        client: None,
        batch_size: DEFAULT_BATCH,
    };

    if args.is_empty() {
        cli.repl()
    } else {
        cli.run_line(&args.join(" "))?;
        cli.save()
    }
}

fn state_path() -> PathBuf {
    if let Some(path) = env::var_os("VATA_CLI_STATE") {
        return PathBuf::from(path);
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".vata-cli.toml")
}

fn load_state(path: &Path) -> io::Result<State> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(State::default()),
        Err(err) => return Err(err),
    };
    let value = text
        .parse::<toml::Table>()
        .map_err(|err| invalid_data(format!("invalid state TOML: {err}")))?;
    let active = value
        .get("active")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut servers = BTreeMap::new();
    if let Some(table) = value.get("servers").and_then(Value::as_table) {
        for (name, server) in table {
            let server = server
                .as_table()
                .ok_or_else(|| invalid_data(format!("servers.{name} must be a table")))?;
            let tcp = socket_field(server, "tcp")?;
            let udp = socket_field(server, "udp")?;
            if tcp.is_none() && udp.is_none() {
                return Err(invalid_data(format!(
                    "servers.{name} needs tcp or udp address"
                )));
            }
            servers.insert(name.clone(), ServerTarget { tcp, udp });
        }
    }
    Ok(State { active, servers })
}

fn socket_field(
    table: &toml::map::Map<String, Value>,
    key: &str,
) -> io::Result<Option<SocketAddr>> {
    table
        .get(key)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| invalid_data(format!("{key} must be a string")))?
                .parse()
                .map_err(|err| invalid_data(format!("invalid {key} address: {err}")))
        })
        .transpose()
}

fn save_state(path: &Path, state: &State) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut text = String::new();
    if let Some(active) = &state.active {
        text.push_str(&format!("active = \"{}\"\n\n", escape(active)));
    }
    for (name, server) in &state.servers {
        text.push_str(&format!("[servers.\"{}\"]\n", escape(name)));
        if let Some(addr) = server.tcp {
            text.push_str(&format!("tcp = \"{addr}\"\n"));
        }
        if let Some(addr) = server.udp {
            text.push_str(&format!("udp = \"{addr}\"\n"));
        }
        text.push('\n');
    }
    fs::write(path, text)
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn server_from_config(path: &Path, name: Option<&str>) -> io::Result<(String, ServerTarget)> {
    let text = fs::read_to_string(path)?;
    let conf = toml::from_str::<Conf>(&text)
        .map_err(|err| invalid_data(format!("invalid Vata config: {err}")))?;
    let tcp = conf
        .client
        .as_ref()
        .map(|client| {
            client
                .addr
                .parse()
                .map_err(|err| invalid_data(format!("invalid client.addr: {err}")))
        })
        .transpose()?;
    let udp_port = conf
        .ingress
        .as_ref()
        .map(|ingress| ingress.port)
        .or_else(|| conf.xdp_conf.as_ref().map(|xdp| xdp.udp_port));
    let udp = udp_port.map(|port| SocketAddr::new(tcp_host(tcp), port));
    if tcp.is_none() && udp.is_none() {
        return Err(invalid_data(
            "config has no [client], [ingress], or [xdp_conf] endpoint",
        ));
    }
    let name = name
        .map(str::to_owned)
        .or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "default".into());
    validate_name(&name)?;
    Ok((name, ServerTarget { tcp, udp }))
}

fn tcp_host(tcp: Option<SocketAddr>) -> IpAddr {
    tcp.map(|addr| addr.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        return Err(invalid_data(
            "server names must use only letters, digits, '.', '_', '-'",
        ));
    }
    Ok(())
}

impl Cli {
    fn repl(&mut self) -> io::Result<()> {
        println!("vata-cli; type help for commands, exit to quit");
        let stdin = io::stdin();
        loop {
            print!("{}> ", self.prompt());
            io::stdout().flush()?;
            let mut line = String::new();
            if stdin.lock().read_line(&mut line)? == 0 {
                break;
            }
            match self.run_line(&line) {
                Ok(CommandResult::Continue) => self.save()?,
                Ok(CommandResult::Exit) => {
                    self.save()?;
                    break;
                }
                Err(err) => eprintln!("error: {err}"),
            }
        }
        Ok(())
    }

    fn prompt(&self) -> String {
        self.state
            .active
            .as_deref()
            .map(|name| format!("vata({name})"))
            .unwrap_or_else(|| "vata".into())
    }

    fn run_line(&mut self, line: &str) -> io::Result<CommandResult> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(CommandResult::Continue);
        }
        let (cmd, rest) = split_cmd(line);
        match cmd {
            "help" | "?" => print!("{HELP}"),
            "exit" | "quit" | "q" => return Ok(CommandResult::Exit),
            "servers" | "ls" => self.print_servers(),
            "current" => self.print_current(),
            "state" => println!("{}", self.state_path.display()),
            "add" => self.add_server(rest)?,
            "add-udp" => self.add_udp(rest)?,
            "remove" | "del" => self.remove_server(rest)?,
            "select" | "use" => self.select_server(rest)?,
            "load-config" | "config" => self.load_config(rest)?,
            "connect" => self.connect_cmd(rest)?,
            "ping" => {
                let addr = self.tcp_addr()?;
                VataClient::warm(addr)?;
                println!("OK");
            }
            "write" => self.write_text(rest)?,
            "bulk" => self.write_file(rest, false)?,
            "read" => self.read_batch(rest)?,
            "udp" => self.udp_text(rest)?,
            "udp-bulk" => self.write_file(rest, true)?,
            _ => return Err(invalid_data(format!("unknown command {cmd}; try help"))),
        }
        Ok(CommandResult::Continue)
    }

    fn save(&self) -> io::Result<()> {
        save_state(&self.state_path, &self.state)
    }

    fn print_servers(&self) {
        for (name, target) in &self.state.servers {
            let mark = if self.state.active.as_deref() == Some(name) {
                "*"
            } else {
                " "
            };
            println!(
                "{mark} {name:16} tcp={} udp={}",
                opt_addr(target.tcp),
                opt_addr(target.udp)
            );
        }
    }

    fn print_current(&self) {
        match self.selected() {
            Ok((name, target)) => println!(
                "{name}: tcp={} udp={}",
                opt_addr(target.tcp),
                opt_addr(target.udp)
            ),
            Err(_) => println!("no server selected"),
        }
    }

    fn add_server(&mut self, rest: &str) -> io::Result<()> {
        let parts = rest.split_whitespace().collect::<Vec<_>>();
        if !(3..=4).contains(&parts.len()) {
            return Err(invalid_data("usage: add NAME IP TCP_PORT [UDP_PORT]"));
        }
        validate_name(parts[0])?;
        let tcp = parse_addr(parts[1], parts[2])?;
        let udp = parts
            .get(3)
            .map(|port| parse_addr(parts[1], port))
            .transpose()?;
        self.state.servers.insert(
            parts[0].into(),
            ServerTarget {
                tcp: Some(tcp),
                udp,
            },
        );
        self.state.active = Some(parts[0].into());
        self.client = None;
        println!("OK");
        Ok(())
    }

    fn add_udp(&mut self, rest: &str) -> io::Result<()> {
        let parts = rest.split_whitespace().collect::<Vec<_>>();
        if parts.len() != 3 {
            return Err(invalid_data("usage: add-udp NAME IP UDP_PORT"));
        }
        validate_name(parts[0])?;
        let udp = parse_addr(parts[1], parts[2])?;
        self.state.servers.insert(
            parts[0].into(),
            ServerTarget {
                tcp: None,
                udp: Some(udp),
            },
        );
        self.state.active = Some(parts[0].into());
        self.client = None;
        println!("OK");
        Ok(())
    }

    fn remove_server(&mut self, rest: &str) -> io::Result<()> {
        let name = rest.trim();
        if name.is_empty() {
            return Err(invalid_data("usage: remove NAME"));
        }
        self.state.servers.remove(name);
        if self.state.active.as_deref() == Some(name) {
            self.state.active = None;
            self.client = None;
        }
        println!("OK");
        Ok(())
    }

    fn select_server(&mut self, rest: &str) -> io::Result<()> {
        let name = rest.trim();
        if !self.state.servers.contains_key(name) {
            return Err(invalid_data(format!("unknown server {name}")));
        }
        self.state.active = Some(name.into());
        self.client = None;
        println!("OK");
        Ok(())
    }

    fn load_config(&mut self, rest: &str) -> io::Result<()> {
        let parts = rest.split_whitespace().collect::<Vec<_>>();
        if parts.is_empty() || parts.len() > 2 {
            return Err(invalid_data("usage: load-config PATH [NAME]"));
        }
        let (name, target) = server_from_config(Path::new(parts[0]), parts.get(1).copied())?;
        self.state.servers.insert(name.clone(), target);
        self.state.active = Some(name);
        self.client = None;
        println!("OK");
        Ok(())
    }

    fn connect_cmd(&mut self, rest: &str) -> io::Result<()> {
        if !rest.trim().is_empty() {
            self.batch_size = rest
                .trim()
                .parse::<u16>()
                .map_err(|err| invalid_data(format!("invalid batch size: {err}")))?;
            if self.batch_size == 0 {
                return Err(invalid_data("batch size must be positive"));
            }
        }
        let addr = self.tcp_addr()?;
        self.client = Some(VataClient::connect_read_writer(addr, self.batch_size)?);
        println!("connected {addr} batch={}", self.batch_size);
        Ok(())
    }

    fn write_text(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Err(invalid_data("usage: write TEXT"));
        }
        if text.len() > MAX_UDP_DATAGRAM {
            return Err(invalid_data(format!(
                "record is {} bytes; max is {MAX_UDP_DATAGRAM}; use bulk with chunks",
                text.len()
            )));
        }
        self.client()?.write_batch([text.as_bytes()])?;
        println!("OK");
        Ok(())
    }

    fn read_batch(&mut self, rest: &str) -> io::Result<()> {
        let timeout = if rest.trim().is_empty() {
            DEFAULT_READ_TIMEOUT
        } else {
            Duration::from_millis(
                rest.trim()
                    .parse()
                    .map_err(|err| invalid_data(format!("invalid timeout: {err}")))?,
            )
        };
        for (idx, record) in self
            .client()?
            .read_batch_timeout(timeout)?
            .iter()
            .enumerate()
        {
            println!("{idx}: {}", printable(record));
        }
        Ok(())
    }

    fn udp_text(&self, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Err(invalid_data("usage: udp TEXT"));
        }
        if text.len() > MAX_UDP_DATAGRAM {
            return Err(invalid_data(format!(
                "datagram is {} bytes; max is {MAX_UDP_DATAGRAM}; use udp-bulk with chunks",
                text.len()
            )));
        }
        let addr = self.udp_addr()?;
        let socket = udp_socket(addr)?;
        let sent = socket.send_to(text.as_bytes(), addr)?;
        println!("sent {sent} bytes");
        Ok(())
    }

    fn write_file(&mut self, rest: &str, udp: bool) -> io::Result<()> {
        let (path, chunk_size) = parse_file_args(rest)?;
        if udp {
            let sent = send_file_udp(self.udp_addr()?, path, chunk_size)?;
            println!("sent {sent} datagrams");
        } else {
            let sent = send_file_tcp(self.client()?, path, chunk_size)?;
            println!("sent {sent} records");
        }
        Ok(())
    }

    fn client(&mut self) -> io::Result<&mut VataClient> {
        if self.client.is_none() {
            let addr = self.tcp_addr()?;
            self.client = Some(VataClient::connect_read_writer(addr, self.batch_size)?);
        }
        Ok(self.client.as_mut().expect("client just connected"))
    }

    fn tcp_addr(&self) -> io::Result<SocketAddr> {
        self.selected()?
            .1
            .tcp
            .ok_or_else(|| invalid_data("selected server has no TCP address"))
    }

    fn udp_addr(&self) -> io::Result<SocketAddr> {
        self.selected()?
            .1
            .udp
            .ok_or_else(|| invalid_data("selected server has no UDP address"))
    }

    fn selected(&self) -> io::Result<(&str, &ServerTarget)> {
        let name = self
            .state
            .active
            .as_deref()
            .ok_or_else(|| invalid_data("no server selected; use add/select/load-config"))?;
        let target = self
            .state
            .servers
            .get(name)
            .ok_or_else(|| invalid_data(format!("active server {name} is missing")))?;
        Ok((name, target))
    }
}

fn split_cmd(line: &str) -> (&str, &str) {
    line.split_once(char::is_whitespace)
        .map(|(cmd, rest)| (cmd, rest.trim_start()))
        .unwrap_or((line, ""))
}

fn parse_addr(host: &str, port: &str) -> io::Result<SocketAddr> {
    let ip = host
        .parse()
        .map_err(|err| invalid_data(format!("invalid IP {host}: {err}")))?;
    let port = port
        .parse()
        .map_err(|err| invalid_data(format!("invalid port {port}: {err}")))?;
    Ok(SocketAddr::new(ip, port))
}

fn opt_addr(addr: Option<SocketAddr>) -> String {
    addr.map(|addr| addr.to_string())
        .unwrap_or_else(|| "-".into())
}

fn parse_file_args(rest: &str) -> io::Result<(&Path, usize)> {
    let parts = rest.split_whitespace().collect::<Vec<_>>();
    if parts.is_empty() || parts.len() > 2 {
        return Err(invalid_data("usage: bulk PATH [CHUNK_BYTES]"));
    }
    let chunk_size = if let Some(raw) = parts.get(1) {
        raw.parse()
            .map_err(|err| invalid_data(format!("invalid chunk size: {err}")))?
    } else {
        MAX_UDP_DATAGRAM
    };
    if chunk_size == 0 || chunk_size > MAX_UDP_DATAGRAM {
        return Err(invalid_data(format!(
            "chunk size must be 1..={MAX_UDP_DATAGRAM}"
        )));
    }
    Ok((Path::new(parts[0]), chunk_size))
}

fn send_file_tcp(client: &mut VataClient, path: &Path, chunk_size: usize) -> io::Result<usize> {
    let mut file = File::open(path)?;
    let mut sent = 0;
    loop {
        let mut records = Vec::with_capacity(DEFAULT_BATCH as usize);
        for _ in 0..DEFAULT_BATCH {
            let mut buf = vec![0; chunk_size];
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            buf.truncate(n);
            records.push(buf);
        }
        if records.is_empty() {
            break;
        }
        client.write_batch(records.iter().map(Vec::as_slice))?;
        sent += records.len();
    }
    Ok(sent)
}

fn send_file_udp(addr: SocketAddr, path: &Path, chunk_size: usize) -> io::Result<usize> {
    let socket = udp_socket(addr)?;
    let mut file = File::open(path)?;
    let mut sent = 0;
    loop {
        let mut buf = vec![0; chunk_size];
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        socket.send_to(&buf[..n], addr)?;
        sent += 1;
    }
    Ok(sent)
}

fn udp_socket(addr: SocketAddr) -> io::Result<UdpSocket> {
    let bind = if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    UdpSocket::bind(bind)
}

fn printable(bytes: &[u8]) -> String {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .unwrap_or_else(|_| {
            let mut out = String::from("0x");
            for byte in bytes {
                out.push_str(&format!("{byte:02x}"));
            }
            out
        })
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn state_round_trips_named_servers() {
        let path = std::env::temp_dir().join(format!(
            "vata-cli-state-{}-{}.toml",
            std::process::id(),
            "roundtrip"
        ));
        let mut state = State::default();
        state.active = Some("dev.local".into());
        state.servers.insert(
            "dev.local".into(),
            ServerTarget {
                tcp: Some("127.0.0.1:9100".parse().unwrap()),
                udp: Some("127.0.0.1:8030".parse().unwrap()),
            },
        );

        save_state(&path, &state).unwrap();
        let loaded = load_state(&path).unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(loaded.active.as_deref(), Some("dev.local"));
        assert_eq!(
            loaded.servers["dev.local"].tcp,
            Some("127.0.0.1:9100".parse::<SocketAddr>().unwrap())
        );
        assert_eq!(
            loaded.servers["dev.local"].udp,
            Some("127.0.0.1:8030".parse::<SocketAddr>().unwrap())
        );
    }

    #[test]
    fn vata_config_loads_tcp_and_udp_addresses() {
        let path = std::env::temp_dir().join(format!(
            "vata-cli-conf-{}-{}.toml",
            std::process::id(),
            "load"
        ));
        std::fs::write(
            &path,
            r#"
            [core_conf]
            max_readers = 1
            cap = 2

            [telemetry_conf]
            report_interval_ms = 1000

            [client]
            addr = "192.0.2.10:9100"

            [ingress]
            port = 8030
            receiver = 1
            processor = 1
            "#,
        )
        .unwrap();

        let loaded = server_from_config(&path, Some("prod")).unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(loaded.0, "prod");
        assert_eq!(
            loaded.1.tcp,
            Some("192.0.2.10:9100".parse::<SocketAddr>().unwrap())
        );
        assert_eq!(
            loaded.1.udp,
            Some("192.0.2.10:8030".parse::<SocketAddr>().unwrap())
        );
    }

    #[test]
    fn add_command_accepts_ip_and_port_parts() {
        assert_eq!(
            parse_addr("::1", "9100").unwrap(),
            "[::1]:9100".parse::<SocketAddr>().unwrap()
        );
    }
}
