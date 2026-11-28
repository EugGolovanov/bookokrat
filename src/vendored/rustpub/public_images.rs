use std::net::IpAddr;

use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};
use ureq::{Agent, Error};

pub(super) fn agent() -> Agent {
    // A proxy could resolve the hostname again, bypassing our address check.
    let config = Agent::config_builder().https_only(true).proxy(None).build();
    Agent::with_parts(config, DefaultConnector::default(), PublicResolver)
}

#[derive(Debug)]
struct PublicResolver;

impl Resolver for PublicResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, Error> {
        let addrs = DefaultResolver::default().resolve(uri, config, timeout)?;
        if addrs.iter().all(|a| a.port() == 443 && is_public(a.ip())) {
            Ok(addrs)
        } else {
            Err(Error::HostNotFound)
        }
    }
}

// Exclude IANA special-purpose space, including tunnels and translation prefixes.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_documentation()
                && !matches!(
                    ip.octets(),
                    [0 | 224..=255, ..]
                        | [100, 64..=127, ..]
                        | [192, 0, 0, _]
                        | [192, 31, 196, _]
                        | [192, 52, 193, _]
                        | [192, 88, 99, _]
                        | [192, 175, 48, _]
                        | [198, 18..=19, ..]
                )
        }
        IpAddr::V6(ip) => {
            let [first, second, third, _, iid, marker, _, _] = ip.segments();
            (0x2000..=0x3fff).contains(&first)
                // ISATAP can tunnel to an embedded IPv4 address under a global prefix.
                && !matches!((iid, marker), (0 | 0x0200, 0x5efe))
                && !matches!(
                    (first, second, third),
                    (0x2001, 0..=0x1ff | 0xdb8, _)
                        | (0x2002 | 0x3ffe, _, _)
                        | (0x2620, 0x4f, 0x8000)
                        | (0x3fff, 0..=0xfff, _)
                )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_public;

    #[test]
    fn only_ordinary_public_unicast_addresses_are_eligible() {
        for ip in [
            "0.1.2.3",
            "10.1.2.3",
            "100.64.0.0",
            "100.127.255.255",
            "127.0.0.2",
            "169.254.169.254",
            "172.31.255.255",
            "192.0.0.170",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "100::1",
            "2001::1",
            "2001:2::1",
            "2001:db8::1",
            "2002:7f00:1::",
            "3fff:fff::1",
            "2600::5efe:7f00:1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "1.1.1.1",
            "100.128.0.1",
            "172.32.0.1",
            "2606:4700:4700::1111",
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }
}
