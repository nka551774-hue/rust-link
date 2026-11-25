//! The network guard.
//!
//! A track URL arrives from a client, and following it means the node makes a
//! request on that client's behalf. Without a guard, a URL pointing at the
//! node's own network turns the node into a way to read things it should not be
//! able to reach, so every connection is checked twice: once when the URL is
//! accepted, and once for every address the name resolves to.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, NextTimeout, Transport};
use ureq::{Agent, Error};

use crate::{deadline, SourceError};

/// Parses and validates an HTTP(S) URL without credentials, control bytes or backslashes.
pub(crate) fn parse_http_url(raw: &str) -> Result<url::Url, SourceError> {
    if raw.bytes().any(|byte| byte <= b' ' || byte == 0x7f || byte == b'\\') {
        return Err(SourceError::Url("expected an HTTP(S) URL without credentials".into()));
    }
    let Ok(url) = url::Url::parse(raw) else {
        return Err(SourceError::Url("expected an HTTP(S) URL without credentials".into()));
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(SourceError::Url("expected an HTTP(S) URL without credentials".into()));
    }
    Ok(url)
}

/// Checks the shape of a URL before anything is asked of it.
///
/// Credentials are refused because they are a way to hide the real destination
/// from a log, and because nothing the node streams needs them.
pub fn validate_url(raw: &str) -> Result<(), SourceError> {
    parse_http_url(raw).map(|_| ())
}

/// Whether an address is one the node is willing to connect to.
///
/// The list of refused blocks is the set that either routes somewhere else on
/// the same host or is reserved for documentation and translation, none of which
/// a media URL legitimately points at.
pub fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => is_public_v6(address),
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    if address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
    {
        return false;
    }
    // Reject this-network (0/8), multicast/reserved (224+/3), CGNAT (100.64/10),
    // benchmarking (198.18/15), retired 6to4 relay (192.88.99/24), and 192.0.0/24.
    !matches!(
        address.octets(),
        [0 | 224.., ..]
            | [100, 64..=127, ..]
            | [198, 18 | 19, ..]
            | [192, 88, 99, _]
            | [192, 0, 0, _]
    )
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_v4(mapped);
    }
    let s = address.segments();
    // Public IPv6 unicast is 2000::/3, excluding documentation (2001:db8::/32,
    // 3fff::/20), 6to4 (2002::/16), and IETF protocol assignments (2001::/23).
    (s[0] & 0xe000 == 0x2000)
        && !matches!(
            (s[0], s[1]),
            (0x2001, 0x0db8) | (0x2002, _) | (0x2001, ..0x0200) | (0x3fff, ..0x1000)
        )
}

/// A resolver that refuses to hand back an address the node will not connect
/// to.
///
/// The check belongs here rather than at the call site because it is the last
/// place before a connection is made, which is the only point where a name that
/// resolved differently since the URL was accepted can still be caught.
#[derive(Debug)]
struct GuardedResolver {
    allow_private: bool,
    /// Exact proxy URI resolved by `ureq`'s proxy connectors, exempt because an
    /// operator's proxy commonly listens on loopback.
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
        let is_proxy = self.proxy.as_ref() == Some(uri);
        if self.allow_private || is_proxy {
            return Ok(addresses);
        }
        let mut allowed = Resolver::empty(&DefaultResolver::default());
        for address in addresses.iter() {
            if is_public(address.ip()) {
                allowed.push(*address);
            }
        }
        if allowed.is_empty() {
            return Err(Error::HostNotFound);
        }
        Ok(allowed)
    }
}

/// Blocks literal private/loopback target IPs even when a proxy (`http://`,
/// `https://`, `socks4a://`, `socks5h://`) defers target resolution to the proxy.
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
        if !self.allow_private && details.config.proxy().is_some() {
            if let Some(host) = details.uri.host() {
                let ip = match url::Host::parse(host) {
                    Ok(url::Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
                    Ok(url::Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
                    _ => None,
                };
                if ip.is_some_and(|addr| !is_public(addr)) {
                    return Err(Error::HostNotFound);
                }
            }
        }
        Ok(chained)
    }
}

/// Ureq's recv_body/global timeouts cover the entire body, not socket inactivity.
/// A stream can run for hours; cap individual blocking transport operations
/// instead, while retaining any shorter catalogue/request deadline.
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
    let idle = deadline::timeout(std::time::Duration::from_secs(30))
        .map_err(|_| Error::Timeout(ureq::Timeout::Global))?;
    if timeout.after.is_zero() {
        return Err(Error::Timeout(timeout.reason));
    }
    timeout.after = timeout.after.min(idle.into());
    Ok(timeout)
}

/// Builds the client every outbound request goes through.
///
/// Redirects are not followed: a redirect is a second URL from a source the node
/// does not control, and it would have to be checked and re-fetched anyway, so
/// the caller follows it deliberately instead.
pub fn agent(allow_private: bool) -> Agent {
    build(allow_private, None)
}

/// The same client, connecting through `proxy` (`http://`, `socks4://`,
/// `socks5://` or `socks5h://`).
pub fn proxied_agent(allow_private: bool, proxy: &str) -> Result<Agent, SourceError> {
    let proxy = ureq::Proxy::new(proxy.trim())
        .map_err(|error| SourceError::Url(format!("youtubeProxy is not usable: {error}")))?;
    Ok(build(allow_private, Some(proxy)))
}

fn build(allow_private: bool, proxy: Option<ureq::Proxy>) -> Agent {
    let exempt = proxy.as_ref().map(|p| p.uri().clone());
    let config = Agent::config_builder()
        .accept_encoding("identity")
        .timeout_connect(Some(std::time::Duration::from_secs(10)))
        .timeout_global(Some(std::time::Duration::from_secs(30)))
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

/// Resolves a name and reports whether every address it has is one the node may
/// use, which is what lets a test check the guard without a socket.
pub fn resolve_all_public(host: &str, port: u16, allow_private: bool) -> Result<(), SourceError> {
    if allow_private {
        return Ok(());
    }
    let mut addresses = (host, port)
        .to_socket_addrs()
        .map_err(|error| SourceError::Url(format!("{host} could not be resolved: {error}")))?
        .peekable();
    if addresses.peek().is_none() {
        return Err(SourceError::Url(format!("{host} has no addresses")));
    }
    if addresses.any(|address| !is_public(address.ip())) {
        return Err(SourceError::Url("private or special-use network address is disabled".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_proxy_is_reached_while_private_networks_stay_refused() {
        let (proxy, server) = crate::test_http::serve(1, |_, head, _| {
            // The tunnel is asked of the proxy, which resolves the target itself.
            assert!(head.starts_with("CONNECT youtube.test:80 HTTP/1.1"), "{head}");
            serde_json::json!({})
        });
        let proxied = proxied_agent(false, &proxy).unwrap();
        // The fixture is no real proxy, so the tunnel itself fails afterwards.
        let _ = proxied.get("http://youtube.test/probe").call();
        server.join().expect("the local proxy was reached");
        assert!(agent(false).get(&proxy).call().is_err(), "the direct client still refuses it");
        assert!(proxied_agent(false, "not a proxy").is_err());
    }

    #[test]
    fn blocking_io_has_an_idle_limit_without_a_whole_stream_lifetime_limit() {
        use ureq::unversioned::transport::time::Duration;
        let unbounded =
            NextTimeout { after: Duration::NotHappening, reason: ureq::Timeout::Global };
        assert_eq!(bounded_timeout(unbounded).unwrap().after, Duration::from_secs(30));
        let _scope =
            deadline::enter(std::time::Instant::now() + std::time::Duration::from_millis(50));
        assert!(bounded_timeout(unbounded).unwrap().after <= Duration::from_millis(50));
        let sooner =
            NextTimeout { after: Duration::from_millis(1), reason: ureq::Timeout::RecvBody };
        assert_eq!(bounded_timeout(sooner).unwrap().after, Duration::from_millis(1));
    }

    fn ip(raw: &str) -> IpAddr {
        raw.parse().unwrap()
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
            assert!(!is_public(ip(raw)), "{raw} should not be reachable");
        }
    }

    #[test]
    fn an_address_on_the_public_internet_is_allowed() {
        for raw in ["1.1.1.1", "8.8.8.8", "142.250.0.1", "2606:4700::1111"] {
            assert!(is_public(ip(raw)), "{raw} should be reachable");
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
            assert!(!is_public(ip(raw)), "{raw} should not be reachable");
        }
    }

    #[test]
    fn ordinary_global_ipv6_is_not_confused_with_teredo() {
        for raw in ["2001:4860:4860::8888", "2001:4860::1", "2001:200::1", "2606:4700::1111"] {
            assert!(is_public(ip(raw)), "{raw} should be reachable");
        }
    }

    #[test]
    fn a_mapped_v4_address_is_judged_by_the_v4_rules() {
        assert!(!is_public(ip("::ffff:127.0.0.1")));
        assert!(is_public(ip("::ffff:1.1.1.1")));
    }

    #[test]
    fn a_name_that_resolves_to_a_private_address_is_refused() {
        assert!(resolve_all_public("localhost", 80, false).is_err());
        assert!(resolve_all_public("localhost", 80, true).is_ok());
    }
}
