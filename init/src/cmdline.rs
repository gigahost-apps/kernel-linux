//! The kernel command line's `gh.*` parameters (interface versions 1 to
//! 5).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The interface versions this init accepts.
pub const VERSIONS: &[u32] = &[1, 2, 3, 4, 5];

/// The network and identity the agent gives the instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelArgs {
    /// The boot's interface version (`gh.v`), one of [`VERSIONS`].
    pub version: u32,
    pub hostname: String,
    pub ip4: (Ipv4Addr, u8),
    pub gw4: Ipv4Addr,
    pub ip6: Option<(Ipv6Addr, u8)>,
    pub gw6: Option<Ipv6Addr>,
    pub dns: Vec<IpAddr>,
    pub mtu: Option<u32>,
    pub vsock: u32,
}

/// The `gh.*` words of the command line, without the prefix. A key given
/// twice is refused: the agent never writes one twice.
fn words(cmdline: &str) -> Result<BTreeMap<&str, &str>, String> {
    let mut found = BTreeMap::new();
    for word in cmdline.split_ascii_whitespace() {
        let Some(rest) = word.strip_prefix("gh.") else {
            continue;
        };
        let (key, value) = rest.split_once('=').unwrap_or((rest, ""));
        if found.insert(key, value).is_some() {
            return Err(format!("gh.{key} is given twice"));
        }
    }
    Ok(found)
}

/// The interface version alone, read leniently, so a command line that
/// fails to parse is still reported in its own version when it names one
/// this init supports.
pub fn version(cmdline: &str) -> Option<u32> {
    cmdline
        .split_ascii_whitespace()
        .filter_map(|word| word.strip_prefix("gh.v="))
        .find_map(|value| value.parse().ok().filter(|v| VERSIONS.contains(v)))
}

/// The vsock port alone, read leniently, so a command line that fails to
/// parse can still be reported over vsock.
pub fn vsock_port(cmdline: &str) -> Option<u32> {
    cmdline
        .split_ascii_whitespace()
        .filter_map(|word| word.strip_prefix("gh.vsock="))
        .find_map(|value| parse_port(value).ok())
}

fn parse_port(value: &str) -> Result<u32, ()> {
    let port: u32 = value.parse().map_err(drop)?;
    // VMADDR_PORT_ANY is not a port.
    if port == u32::MAX { Err(()) } else { Ok(port) }
}

fn invalid(key: &str) -> String {
    format!("gh.{key} is invalid")
}

fn required<'a>(words: &BTreeMap<&str, &'a str>, key: &str) -> Result<&'a str, String> {
    words
        .get(key)
        .copied()
        .ok_or_else(|| format!("gh.{key} is missing"))
}

fn prefixed<T: std::str::FromStr>(value: &str, max: u8) -> Option<(T, u8)> {
    let (address, text) = value.split_once('/')?;
    let prefix: u8 = text.parse().ok()?;
    // Digits only, no leading zeros or sign.
    if prefix > max || text != prefix.to_string() {
        return None;
    }
    Some((address.parse().ok()?, prefix))
}

/// A host name as RFC 1123 allows it: labels of letters, digits and
/// hyphens, 1 to 63 bytes each, 64 bytes in all (the kernel's limit).
fn valid_hostname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// Parses the command line. Unknown `gh.*` keys are ignored; a `gh.v`
/// other than the versions in [`VERSIONS`] is refused.
pub fn parse(cmdline: &str) -> Result<KernelArgs, String> {
    let words = words(cmdline)?;
    let version = required(&words, "v")?
        .parse()
        .ok()
        .filter(|v: &u32| VERSIONS.contains(v))
        .ok_or("gh.v names an interface version this init does not support")?;
    let hostname = required(&words, "hostname")?;
    if !valid_hostname(hostname) {
        return Err(invalid("hostname"));
    }
    let ip4 = prefixed(required(&words, "ip4")?, 32).ok_or_else(|| invalid("ip4"))?;
    let gw4 = required(&words, "gw4")?
        .parse()
        .map_err(|_| invalid("gw4"))?;
    let ip6 = words
        .get("ip6")
        .map(|value| prefixed(value, 128).ok_or_else(|| invalid("ip6")))
        .transpose()?;
    let gw6 = words
        .get("gw6")
        .map(|value| value.parse().map_err(|_| invalid("gw6")))
        .transpose()?;
    if ip6.is_some() != gw6.is_some() {
        return Err("gh.ip6 and gh.gw6 come together".into());
    }
    let dns = required(&words, "dns")?
        .split(',')
        .map(|value| value.parse::<IpAddr>().map_err(|_| invalid("dns")))
        .collect::<Result<Vec<_>, _>>()?;
    let mtu = words
        .get("mtu")
        .map(|value| {
            value
                .parse::<u32>()
                .ok()
                .filter(|mtu| (68..=65535).contains(mtu))
                .ok_or_else(|| invalid("mtu"))
        })
        .transpose()?;
    let vsock = parse_port(required(&words, "vsock")?).map_err(|()| invalid("vsock"))?;
    Ok(KernelArgs {
        version,
        hostname: hostname.to_owned(),
        ip4,
        gw4,
        ip6,
        gw6,
        dns,
        mtu,
        vsock,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "console=ttyS0 quiet gh.v=1 gh.hostname=web-1 gh.ip4=10.0.2.15/24 \
        gh.gw4=10.0.2.2 gh.ip6=fd00::15/64 gh.gw6=fe80::1 gh.dns=10.0.2.3,fd00::3 gh.mtu=1400 gh.vsock=1024";

    #[test]
    fn every_key() {
        let args = parse(FULL).unwrap();
        assert_eq!(args.version, 1);
        assert_eq!(args.hostname, "web-1");
        assert_eq!(args.ip4, (Ipv4Addr::new(10, 0, 2, 15), 24));
        assert_eq!(args.gw4, Ipv4Addr::new(10, 0, 2, 2));
        assert_eq!(args.ip6, Some(("fd00::15".parse().unwrap(), 64)));
        assert_eq!(args.gw6, Some("fe80::1".parse().unwrap()));
        assert_eq!(
            args.dns,
            vec![
                "10.0.2.3".parse::<IpAddr>().unwrap(),
                "fd00::3".parse().unwrap()
            ]
        );
        assert_eq!(args.mtu, Some(1400));
        assert_eq!(args.vsock, 1024);
    }

    #[test]
    fn optional_keys_absent() {
        let args = parse("gh.v=1 gh.hostname=a gh.ip4=192.0.2.5/32 gh.gw4=192.0.2.1 gh.dns=192.0.2.53 gh.vsock=7").unwrap();
        assert_eq!(args.ip6, None);
        assert_eq!(args.gw6, None);
        assert_eq!(args.mtu, None);
        assert_eq!(args.ip4.1, 32);
    }

    #[test]
    fn each_required_key_missing() {
        for key in ["v", "hostname", "ip4", "gw4", "dns", "vsock"] {
            let line: String = FULL
                .split(' ')
                .filter(|word| !word.starts_with(&format!("gh.{key}=")))
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(parse(&line), Err(format!("gh.{key} is missing")), "{key}");
        }
    }

    #[test]
    fn other_versions_refused() {
        for version in ["6", "0", "", "1.0", "one"] {
            let line = FULL.replace("gh.v=1", &format!("gh.v={version}"));
            assert!(parse(&line).unwrap_err().contains("version"), "{version}");
        }
    }

    #[test]
    fn version_two_accepted() {
        let args = parse(&FULL.replace("gh.v=1", "gh.v=2")).unwrap();
        assert_eq!(args.version, 2);
        let args = parse(&FULL.replace("gh.v=1", "gh.v=3")).unwrap();
        assert_eq!(args.version, 3);
        let args = parse(&FULL.replace("gh.v=1", "gh.v=4")).unwrap();
        assert_eq!(args.version, 4);
        let args = parse(&FULL.replace("gh.v=1", "gh.v=5")).unwrap();
        assert_eq!(args.version, 5);
    }

    #[test]
    fn lenient_version() {
        assert_eq!(version("gh.v=2 gh.hostname=-bad"), Some(2));
        assert_eq!(version("gh.v=1"), Some(1));
        assert_eq!(version("gh.v=3"), Some(3));
        assert_eq!(version("gh.v=4"), Some(4));
        assert_eq!(version("gh.v=5"), Some(5));
        assert_eq!(version("gh.v=6"), None);
        assert_eq!(version("quiet"), None);
    }

    #[test]
    fn unknown_keys_ignored() {
        let line = format!("{FULL} gh.future=yes gh.flag ghost=1 gh");
        assert!(parse(&line).is_ok());
    }

    #[test]
    fn duplicate_keys_refused() {
        let line = format!("{FULL} gh.hostname=other");
        assert_eq!(parse(&line), Err("gh.hostname is given twice".into()));
    }

    #[test]
    fn ip6_needs_gw6() {
        let line = FULL.replace(" gh.gw6=fe80::1", "");
        assert!(parse(&line).is_err());
        let line = FULL.replace(" gh.ip6=fd00::15/64", "");
        assert!(parse(&line).is_err());
    }

    #[test]
    fn invalid_values() {
        for (from, to) in [
            ("gh.ip4=10.0.2.15/24", "gh.ip4=10.0.2.15"),
            ("gh.ip4=10.0.2.15/24", "gh.ip4=10.0.2.15/33"),
            ("gh.ip4=10.0.2.15/24", "gh.ip4=10.0.2.15/024"),
            ("gh.ip4=10.0.2.15/24", "gh.ip4=fd00::1/24"),
            ("gh.gw4=10.0.2.2", "gh.gw4=10.0.2"),
            ("gh.ip6=fd00::15/64", "gh.ip6=fd00::15/129"),
            ("gh.gw6=fe80::1", "gh.gw6=10.0.2.2"),
            ("gh.dns=10.0.2.3,fd00::3", "gh.dns=10.0.2.3,"),
            ("gh.mtu=1400", "gh.mtu=67"),
            ("gh.mtu=1400", "gh.mtu=65536"),
            ("gh.vsock=1024", "gh.vsock=4294967295"),
            ("gh.vsock=1024", "gh.vsock=-1"),
            ("gh.hostname=web-1", "gh.hostname=-web"),
            ("gh.hostname=web-1", "gh.hostname=web_1"),
            ("gh.hostname=web-1", "gh.hostname=a..b"),
        ] {
            let line = FULL.replace(from, to);
            assert!(parse(&line).is_err(), "{to}");
        }
        let long = format!("gh.hostname={}", "a".repeat(65));
        assert!(parse(&FULL.replace("gh.hostname=web-1", &long)).is_err());
    }

    #[test]
    fn lenient_port() {
        assert_eq!(vsock_port("gh.v=9 gh.vsock=55"), Some(55));
        assert_eq!(vsock_port("gh.vsock=x"), None);
        assert_eq!(vsock_port("quiet"), None);
    }
}
