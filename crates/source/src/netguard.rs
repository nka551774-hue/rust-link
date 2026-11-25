//! Outbound URL, DNS, IP, and proxy SSRF guard.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;

use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, NextTimeout, Transport};
use ureq::{Agent, Error};

use crate::{deadline, strip_ascii_prefix, SourceError};

const USER_AGENT: &str = concat!("Rustlink/", env!("CARGO_PKG_VERSION"));

/// Parses and validates an HTTP(S) URL without credentials, control bytes, or backslashes.
pub(crate) fn parse_http_url(raw: &str) -> Result<url::Url, SourceError> {
    let bad = || SourceError::Url("expected an HTTP(S) URL without credentials".into());
    if raw.bytes().any(|b| b <= b' ' || b == 0x7f || b == b'\\') {
        return Err(bad());
    }
    let after = strip_ascii_prefix(raw, "http://")
        .or_else(|| strip_ascii_prefix(raw, "https://"))
        .ok_or_else(bad)?;
    let authority = after.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(bad());
    }
    let url = url::Url::parse(raw).map_err(|_| bad())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(bad());
    }
    Ok(url)
}

pub fn validate_url(raw: &str) -> Result<(), SourceError> {
    parse_http_url(raw).map(|_| ())
}

pub fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(addr: Ipv4Addr) -> bool {
    !(addr.is_private()
        || addr.is_loopback()
        || addr.is_link_local()
        || addr.is_broadcast()
        || addr.is_documentation()
        || addr.is_unspecified()
        || matches!(
            addr.octets(),
            [0 | 224.., ..]
                | [100, 64..=127, ..]
                | [198, 18 | 19, ..]
                | [192, 88, 99, _]
                | [192, 0, 0, _]
        ))
}

fn is_public_v6(addr: Ipv6Addr) -> bool {
    if let Some(mapped) = addr.to_ipv4_mapped() {
        return is_public_v4(mapped);
    }
    let s = addr.segments();
    (s[0] & 0xe000 == 0x2000)
        && !matches!(
            (s[0], s[1]),
            (0x2001, 0x0db8) | (0x2002, _) | (0x2001, ..0x0200) | (0x3fff, ..0x1000)
        )
}

#[derive(Debug)]
struct GuardedResolver {
    allow_private: bool,
    proxy: Option<ureq::http::Uri>,
}

impl Resolver for GuardedResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, Error> {
        let addresses =
            DefaultResolver::default().resolve(uri, config, bounded_timeout(timeout)?)?;
        if self.allow_private || self.proxy.as_ref() == Some(uri) {
            return Ok(addresses);
        }
        let mut allowed = Resolver::empty(&DefaultResolver::default());
        for addr in addresses.iter().filter(|a| is_public(a.ip())) {
            allowed.push(*addr);
        }
        (!allowed.is_empty()).then_some(allowed).ok_or(Error::HostNotFound)
    }
}

fn domain_matches(domain: &str, suffix: &str) -> bool {
    domain.eq_ignore_ascii_case(suffix)
        || (domain.len() > suffix.len()
            && domain.as_bytes()[domain.len() - suffix.len() - 1] == b'.'
            && domain[domain.len() - suffix.len()..].eq_ignore_ascii_case(suffix))
}

fn check_proxied_target(host: Option<&str>, proxy_host: Option<&str>) -> Result<(), Error> {
    let bare = host
        .map(|h| h.trim_end_matches('.'))
        .filter(|h| !h.is_empty())
        .ok_or(Error::HostNotFound)?;
    let parsed = if !bare.starts_with('[') && bare.contains(':') {
        bare.parse::<Ipv6Addr>().map(url::Host::Ipv6).map_err(|_| Error::HostNotFound)?
    } else {
        url::Host::parse(bare).map_err(|_| Error::HostNotFound)?
    };
    match parsed {
        url::Host::Ipv4(v4) if is_public(IpAddr::V4(v4)) => Ok(()),
        url::Host::Ipv6(v6) if is_public(IpAddr::V6(v6)) => Ok(()),
        url::Host::Ipv4(_) | url::Host::Ipv6(_) => Err(Error::HostNotFound),
        url::Host::Domain(domain) => {
            let d = domain.trim_end_matches('.');
            if d.is_empty() {
                return Err(Error::HostNotFound);
            }
            if !matches!(url::Host::parse(d), Ok(url::Host::Domain(_))) {
                return check_proxied_target(Some(d), proxy_host);
            }
            if ["localhost", "local", "internal", "home.arpa"].iter().any(|r| domain_matches(d, r))
            {
                return Err(Error::HostNotFound);
            }
            if let Some(p) = proxy_host.map(|p| p.trim_end_matches('.')).filter(|p| !p.is_empty()) {
                let same = match url::Host::parse(p) {
                    Ok(url::Host::Domain(pd)) => d.eq_ignore_ascii_case(pd.trim_end_matches('.')),
                    _ => d.eq_ignore_ascii_case(p),
                };
                if same {
                    return Err(Error::HostNotFound);
                }
            }
            Ok(())
        }
    }
}

#[derive(Debug)]
struct TargetGuardConnector {
    allow_private: bool,
}

impl Connector<()> for TargetGuardConnector {
    type Out = ();

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<()>,
    ) -> Result<Option<Self::Out>, Error> {
        if let Some(proxy) = details.config.proxy().filter(|_| !self.allow_private) {
            check_proxied_target(details.uri.host(), proxy.uri().host())?;
        }
        Ok(chained)
    }
}

#[derive(Debug)]
struct IdleConnector;

impl<In: Transport> Connector<In> for IdleConnector {
    type Out = IdleTransport<In>;

    fn connect(
        &self,
        _details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, Error> {
        Ok(chained.map(IdleTransport))
    }
}

#[derive(Debug)]
struct IdleTransport<In>(In);

impl<In: Transport> Transport for IdleTransport<In> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.0.buffers()
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        self.0.transmit_output(amount, bounded_timeout(timeout)?)
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        self.0.await_input(bounded_timeout(timeout)?)
    }
    fn is_open(&mut self) -> bool {
        self.0.is_open()
    }
    fn is_tls(&self) -> bool {
        self.0.is_tls()
    }
}

fn bounded_timeout(mut timeout: NextTimeout) -> Result<NextTimeout, Error> {
    let idle = deadline::timeout(Duration::from_secs(30))
        .map_err(|_| Error::Timeout(ureq::Timeout::Global))?;
    if timeout.after.is_zero() {
        return Err(Error::Timeout(timeout.reason));
    }
    timeout.after = timeout.after.min(idle.into());
    Ok(timeout)
}

pub fn agent(allow_private: bool) -> Agent {
    build(allow_private, None)
}

fn valid_port(port: &str) -> bool {
    port.parse::<u16>().is_ok_and(|p| p > 0)
}

fn validate_proxy_url(proxy: &str) -> Result<&str, SourceError> {
    const SCHEMES: [&str; 6] =
        ["http://", "https://", "socks4://", "socks4a://", "socks5://", "socks5h://"];
    let raw = proxy.trim();
    if raw.is_empty()
        || !raw.bytes().all(|b| (0x21..=0x7e).contains(&b) && !matches!(b, b'\\' | b'?' | b'#'))
    {
        return Err(SourceError::Url("youtubeProxy must be a valid proxy URL with a host".into()));
    }
    let after = SCHEMES
        .iter()
        .find_map(|s| strip_ascii_prefix(raw, s))
        .ok_or_else(|| SourceError::Url("youtubeProxy uses an unsupported scheme".into()))?;
    let host_port = after.split('/').next().unwrap_or("").rsplit('@').next().unwrap_or("");
    let ok = match host_port.strip_prefix('[') {
        Some(rest) => rest.split_once(']').is_some_and(|(v6, tail)| {
            v6.parse::<Ipv6Addr>().is_ok()
                && (tail.is_empty() || tail.strip_prefix(':').is_some_and(valid_port))
        }),
        None => match host_port.split_once(':') {
            None => !host_port.is_empty() && !host_port.contains(['[', ']']),
            Some((h, p)) => !h.is_empty() && !h.contains(['[', ']', ':']) && valid_port(p),
        },
    };
    ok.then_some(raw)
        .ok_or_else(|| SourceError::Url("youtubeProxy must include a valid host".into()))
}

pub fn proxied_agent(allow_private: bool, proxy: &str) -> Result<Agent, SourceError> {
    let proxy = ureq::Proxy::new(validate_proxy_url(proxy)?)
        .map_err(|e| SourceError::Url(format!("youtubeProxy is not usable: {e}")))?;
    if proxy.uri().host().is_none_or(str::is_empty) {
        return Err(SourceError::Url("youtubeProxy must include a valid host".into()));
    }
    Ok(build(allow_private, Some(proxy)))
}

fn build(allow_private: bool, proxy: Option<ureq::Proxy>) -> Agent {
    let exempt = proxy.as_ref().map(|p| p.uri().clone());
    let config = Agent::config_builder()
        .user_agent(USER_AGENT)
        .accept_encoding("identity")
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(30)))
        .max_redirects(0)
        .proxy(proxy)
        .build();
    Agent::with_parts(
        config,
        TargetGuardConnector { allow_private }
            .chain(ureq::unversioned::transport::DefaultConnector::default())
            .chain(IdleConnector),
        GuardedResolver { allow_private, proxy: exempt },
    )
}

pub fn resolve_all_public(host: &str, port: u16, allow_private: bool) -> Result<(), SourceError> {
    if allow_private {
        return Ok(());
    }
    let mut addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| SourceError::Url(format!("{host} could not be resolved: {e}")))?
        .peekable();
    if addrs.peek().is_none() {
        return Err(SourceError::Url(format!("{host} has no addresses")));
    }
    if addrs.any(|a| !is_public(a.ip())) {
        return Err(SourceError::Url("private or special-use network address is disabled".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(raw: &str) -> IpAddr {
        raw.parse().unwrap()
    }

    #[test]
    fn a_local_proxy_is_reached_while_private_networks_stay_refused() {
        let (proxy, server) = crate::test_http::serve(1, |_, head, _| {
            assert!(head.starts_with("CONNECT youtube.test:80 HTTP/1.1"), "{head}");
            serde_json::json!({})
        });
        let proxied = proxied_agent(false, &proxy).unwrap();
        let _ = proxied.get("http://youtube.test/probe").call();
        server.join().unwrap();
        assert!(agent(false).get(&proxy).call().is_err());
        assert!(proxied_agent(false, "not a proxy").is_err());
    }

    #[test]
    fn blocking_io_has_an_idle_limit_without_a_whole_stream_lifetime_limit() {
        use ureq::unversioned::transport::time::Duration as UreqDur;
        let unbounded = NextTimeout { after: UreqDur::NotHappening, reason: ureq::Timeout::Global };
        assert_eq!(bounded_timeout(unbounded).unwrap().after, UreqDur::from_secs(30));
        let _scope = deadline::enter(std::time::Instant::now() + Duration::from_millis(50));
        assert!(bounded_timeout(unbounded).unwrap().after <= UreqDur::from_millis(50));
        let sooner =
            NextTimeout { after: UreqDur::from_millis(1), reason: ureq::Timeout::RecvBody };
        assert_eq!(bounded_timeout(sooner).unwrap().after, UreqDur::from_millis(1));
    }

    #[test]
    fn a_url_without_credentials_and_with_an_http_scheme_is_accepted() {
        assert!(validate_url("https://example.com/a.mp3").is_ok());
        assert!(validate_url("http://example.com").is_ok());
    }

    #[test]
    fn a_url_with_credentials_is_refused() {
        assert!(validate_url("https://user:pass@example.com/a.mp3").is_err());
    }

    #[test]
    fn a_url_that_is_not_http_is_refused() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("not a url").is_err());
    }

    #[test]
    fn addresses_that_are_not_routable_on_the_public_internet_are_refused() {
        for raw in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.1.1",
            "0.0.0.0",
            "100.64.0.1",
            "198.18.0.1",
            "192.0.2.5",
            "198.51.100.7",
            "203.0.113.9",
            "240.0.0.1",
            "255.255.255.255",
            "::1",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
            "2002::1",
        ] {
            assert!(!is_public(ip(raw)), "{raw}");
        }
    }

    #[test]
    fn an_address_on_the_public_internet_is_allowed() {
        for raw in ["1.1.1.1", "8.8.8.8", "142.250.0.1", "2606:4700::1111"] {
            assert!(is_public(ip(raw)), "{raw}");
        }
    }

    #[test]
    fn special_use_and_multicast_addresses_are_never_media_destinations() {
        for raw in [
            "0.1.2.3",
            "224.0.0.1",
            "239.255.255.250",
            "192.88.99.1",
            "::ffff:0.1.2.3",
            "::ffff:224.0.0.1",
            "::127.0.0.1",
            "100::1",
            "64:ff9b:1::a00:1",
            "fec0::1",
            "ff02::1",
            "ff0e::1",
            "2001:2::1",
            "3fff::1",
            "3fff:fff::1",
        ] {
            assert!(!is_public(ip(raw)), "{raw}");
        }
    }

    #[test]
    fn ordinary_global_ipv6_is_not_confused_with_teredo() {
        for raw in ["2001:4860:4860::8888", "2001:4860::1", "2001:200::1", "2606:4700::1111"] {
            assert!(is_public(ip(raw)), "{raw}");
        }
    }

    #[test]
    fn a_mapped_v4_address_is_judged_by_the_v4_rules() {
        assert!(!is_public(ip("::ffff:127.0.0.1")) && is_public(ip("::ffff:1.1.1.1")));
    }

    #[test]
    fn a_name_that_resolves_to_a_private_address_is_refused() {
        assert!(resolve_all_public("localhost", 80, false).is_err());
        assert!(resolve_all_public("localhost", 80, true).is_ok());
    }

    #[test]
    fn empty_userinfo_markers_in_url_authority_are_refused() {
        for bad in [
            "https://@example.com/a.mp3",
            "https://:@example.com/a.mp3",
            "https://user@example.com/a.mp3",
        ] {
            assert!(validate_url(bad).is_err());
        }
        assert!(validate_url("https://example.com/@artist/a.mp3").is_ok());
    }

    #[test]
    fn proxied_target_guard_blocks_noncanonical_ips_internal_domains_and_proxy_self() {
        for blocked in [
            "",
            ".",
            "127.0.0.1",
            "127.0.0.1.",
            "0177.0.0.1",
            "2130706433",
            "127.1",
            "127.0.0.1%2e",
            "0177.0.0.999",
            "[::1]",
            "::1",
            "[::ffff:127.0.0.1]",
            "[fe80::1%25eth0]",
            "localhost",
            "localhost.",
            "sub.localhost.",
            "printer.local",
            "metadata.google.internal",
            "router.home.arpa",
            "proxy.corp.example",
            "PROXY.CORP.EXAMPLE.",
        ] {
            assert!(
                check_proxied_target(Some(blocked), Some("proxy.corp.example")).is_err(),
                "{blocked}"
            );
        }
        assert!(check_proxied_target(None, Some("127.0.0.1")).is_err());
        for allowed in [
            "youtube.test",
            "rr1---sn-x.googlevideo.com.",
            "1.1.1.1",
            "1.1.1.1.",
            "[2606:4700::1111]",
            "2606:4700::1111",
        ] {
            assert!(check_proxied_target(Some(allowed), Some("127.0.0.1")).is_ok(), "{allowed}");
        }
    }

    #[test]
    fn proxied_agent_validates_scheme_characters_and_host_and_agent_sets_rustlink_ua() {
        for good in [
            "http://127.0.0.1:8080",
            "https://user:pass@proxy.example:8443",
            "socks4://127.0.0.1:1080",
            "socks4a://proxy.example:1080",
            "socks5://[::1]:1080",
            "socks5h://proxy.example",
        ] {
            assert!(proxied_agent(false, good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "127.0.0.1:8080",
            "ftp://proxy.example:8080",
            "socks5://",
            "http://user@:8080",
            "http://proxy.example:8080?x=1",
            "http://proxy.example:8080#frag",
            "http://proxy.example\\@evil",
            "http://proxy.example:0",
        ] {
            assert!(proxied_agent(false, bad).is_err(), "{bad}");
        }
        let (base, server) = crate::test_http::serve(1, |_, head, _| {
            assert!(
                head.to_ascii_lowercase()
                    .contains(&format!("\r\nuser-agent: {}\r\n", USER_AGENT.to_ascii_lowercase())),
                "{head}"
            );
            serde_json::json!({})
        });
        agent(true).get(&base).call().unwrap();
        server.join().unwrap();
    }
}
