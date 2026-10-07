//! Every listener instance on this machine: what `--list-instances` prints, and the ports the
//! others have taken, so a new instance's setup does not offer one another will want.

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

const DEFAULT_DASHBOARD_PORT: u16 = 8080;
const DEFAULT_STREAM_PORT: u16 = 8000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub name: String,
    pub dir: PathBuf,
    /// config.json exists and holds something: setup has been finished.
    pub set_up: bool,
    pub dashboard_port: u16,
    /// Only while the alert stream is turned on.
    pub stream_port: Option<u16>,
}

fn port_value(value: Option<&Value>) -> Option<u16> {
    match value? {
        Value::Number(number) => number.as_u64().and_then(|port| u16::try_from(port).ok()),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|port| *port != 0)
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => matches!(text.trim(), "1" | "true" | "yes" | "on"),
        _ => false,
    }
}

/// The ports an instance's config.json asks for, read from the JSON itself: loading it as a
/// `Config` would resolve its paths against this process's instance, not that one.
pub(crate) fn read_instance(name: &str, dir: &Path) -> Instance {
    let config: Option<Value> = std::fs::read_to_string(dir.join("config.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let set_up = config
        .as_ref()
        .and_then(Value::as_object)
        .is_some_and(|object| !object.is_empty());
    let field = |key: &str| config.as_ref().and_then(|config| config.get(key));
    let dashboard_port = port_value(field("MONITORING_BIND_PORT"))
        .or_else(|| {
            field("MONITORING_BIND_ADDR")
                .and_then(Value::as_str)
                .and_then(|addr| addr.rsplit_once(':'))
                .and_then(|(_, port)| port.parse().ok())
        })
        .unwrap_or(DEFAULT_DASHBOARD_PORT);
    let stream_port = truthy(field("ICECAST_ALERT_STREAM_ENABLED"))
        .then(|| port_value(field("ICECAST_ALERT_PORT")).unwrap_or(DEFAULT_STREAM_PORT));
    Instance {
        name: name.to_string(),
        dir: dir.to_path_buf(),
        set_up,
        dashboard_port,
        stream_port,
    }
}

/// Every instance directory, plus a default instance still kept beside the binary.
pub fn all() -> Vec<Instance> {
    let mut found = Vec::new();
    let base = crate::paths::instances_dir();
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let dir = entry.path();
            let Some(raw) = dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            // What an uninstall leaves -- a kept alert archive -- is not an instance: only a
            // folder holding a configuration, or one being set up, is.
            if !dir.join("config.json").exists() && !dir.join("setup-token.txt").exists() {
                continue;
            }
            if let Ok(name) = crate::paths::parse_instance_name(raw) {
                let label = name.as_deref().unwrap_or(crate::paths::DEFAULT_INSTANCE);
                found.push(read_instance(label, &dir));
            }
        }
    }
    let beside_binary = crate::paths::install_root();
    if beside_binary.join("config.json").is_file()
        && !found.iter().any(|instance| instance.dir == beside_binary)
    {
        found.push(read_instance(crate::paths::DEFAULT_INSTANCE, beside_binary));
    }
    found.sort_by(|a, b| a.name.cmp(&b.name).then(a.dir.cmp(&b.dir)));
    found
}

/// Ports every other set-up instance will bind: its dashboard, and its alert stream when on.
pub fn ports_taken_by_others() -> Vec<u16> {
    let own = crate::paths::app_root();
    all()
        .into_iter()
        .filter(|instance| instance.set_up && instance.dir != own)
        .flat_map(|instance| std::iter::once(instance.dashboard_port).chain(instance.stream_port))
        .collect()
}

/// The first port from `start` that no other instance claims and nothing is listening on now.
pub fn free_port(start: u16, taken: &[u16]) -> u16 {
    free_port_by(start, taken, in_use)
}

/// With nothing free in the next 200 -- a block Windows has reserved for Hyper-V, say -- `start`
/// is kept, and binding it reports the problem as it always has.
fn free_port_by(start: u16, taken: &[u16], in_use: impl Fn(u16) -> bool) -> u16 {
    (start..=u16::MAX)
        .take(200)
        .find(|port| !taken.contains(port) && !in_use(*port))
        .unwrap_or(start)
}

/// A bind alone is not enough on Windows: a listener that set SO_REUSEADDR -- Docker Desktop's
/// published ports, WSL's relay -- lets a second socket bind the same port, and then which of
/// them a browser reaches depends on the address it resolved. Anything answering on loopback
/// counts as in use.
fn in_use(port: u16) -> bool {
    std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port)).is_err() || answers(port)
}

/// Something accepts connections on this port on loopback.
pub(crate) fn answers(port: u16) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
    let connects = |addr: SocketAddr| {
        TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(150)).is_ok()
    };
    connects(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        || connects(SocketAddr::from((Ipv6Addr::LOCALHOST, port)))
}

pub fn print_list() -> Result<()> {
    let instances = all();
    println!(
        "Instances are kept in {}",
        crate::paths::instances_dir().display()
    );
    if instances.is_empty() {
        println!("  (none yet)");
    }
    for instance in &instances {
        let stream = instance
            .stream_port
            .map(|port| format!(", alert stream {port}"))
            .unwrap_or_default();
        println!(
            "  {:<16} {:<11} dashboard {:<5}{}  {}",
            instance.name,
            if instance.set_up {
                "set up"
            } else {
                "not set up"
            },
            instance.dashboard_port,
            stream,
            instance.dir.display()
        );
    }
    println!();
    println!("Start one with:     eas_listener --instance <name>");
    println!("Start it at boot:   eas_listener --instance <name> --install-service");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instance_is_read_from_its_own_config_json() {
        let dir = tempfile::tempdir().expect("temp dir");
        let north = dir.path().join("north");
        std::fs::create_dir(&north).expect("mkdir");
        assert_eq!(
            read_instance("north", &north),
            Instance {
                name: "north".into(),
                dir: north.clone(),
                set_up: false,
                dashboard_port: 8080,
                stream_port: None,
            }
        );

        std::fs::write(
            north.join("config.json"),
            r#"{"MONITORING_BIND_ADDR": "0.0.0.0:9000", "ICECAST_ALERT_STREAM_ENABLED": true,
                "ICECAST_ALERT_PORT": "8100"}"#,
        )
        .expect("config");
        let read = read_instance("north", &north);
        assert!(read.set_up);
        assert_eq!(read.dashboard_port, 9000);
        assert_eq!(read.stream_port, Some(8100));

        // The explicit port wins over the address's, the way the listener reads them.
        std::fs::write(
            north.join("config.json"),
            r#"{"MONITORING_BIND_ADDR": "0.0.0.0:9000", "MONITORING_BIND_PORT": 9100}"#,
        )
        .expect("config");
        let read = read_instance("north", &north);
        assert_eq!(read.dashboard_port, 9100);
        assert_eq!(read.stream_port, None);
    }

    #[test]
    fn a_free_port_skips_what_other_instances_claim_and_what_is_in_use() {
        let busy = [8081, 8083];
        let in_use = |port: u16| busy.contains(&port);
        assert_eq!(free_port_by(8080, &[], in_use), 8080);
        assert_eq!(free_port_by(8080, &[8080], in_use), 8082);
        assert_eq!(free_port_by(8080, &[8080, 8082], in_use), 8084);
        assert_eq!(free_port_by(8080, &[], |_| true), 8080);
    }

    #[test]
    fn a_port_something_listens_on_is_in_use() {
        let held = std::net::TcpListener::bind(("0.0.0.0", 0)).expect("bind");
        let port = held.local_addr().expect("addr").port();
        assert!(in_use(port));
    }
}
