use crate::{Error, Result};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use url::{Host, Url};

/// Destination classes available to trusted callers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum NetworkPolicy {
    /// Reject local and other non-public destinations.
    #[default]
    PublicInternet,
    /// Permit private/local destinations for explicit trusted use.
    AllowPrivate,
}

/// Async DNS source whose full answer is classified by the core resolver.
pub trait ResolverSource: Send + Sync {
    /// Resolve a host into socket addresses. Port values are ignored.
    fn resolve(
        &self,
        host: String,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>>;
}

#[derive(Clone)]
pub(crate) struct SystemResolver;

impl ResolverSource for SystemResolver {
    fn resolve(
        &self,
        host: String,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>> {
        Box::pin(async move {
            tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map(Iterator::collect)
        })
    }
}

#[derive(Clone)]
pub(crate) struct SafeResolver {
    source: Arc<dyn ResolverSource>,
    policy: NetworkPolicy,
}

impl SafeResolver {
    pub(crate) fn new(source: Arc<dyn ResolverSource>, policy: NetworkPolicy) -> Self {
        Self { source, policy }
    }
}

impl Resolve for SafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let source = Arc::clone(&self.source);
        let host = name.as_str().to_owned();
        let policy = self.policy;
        Box::pin(async move {
            let addresses = source.resolve(host).await.map_err(|error| {
                Box::new(ResolverDnsError { kind: error.kind() }) as Box<dyn StdError + Send + Sync>
            })?;
            if addresses.is_empty() {
                return Err(
                    Box::new(PolicyDnsError::NoAddresses) as Box<dyn StdError + Send + Sync>
                );
            }
            if addresses
                .iter()
                .any(|address| !address_is_allowed(policy, address.ip()))
            {
                return Err(
                    Box::new(PolicyDnsError::ForbiddenAddress) as Box<dyn StdError + Send + Sync>
                );
            }
            let addresses: Addrs = Box::new(
                addresses
                    .into_iter()
                    .map(|address| SocketAddr::new(address.ip(), 0)),
            );
            Ok(addresses)
        })
    }
}

#[derive(Debug)]
pub(crate) enum PolicyDnsError {
    ForbiddenAddress,
    NoAddresses,
    Redirect,
}

impl fmt::Display for PolicyDnsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::ForbiddenAddress => "DNS answer contains a forbidden destination address",
            Self::NoAddresses => "DNS answer contains no destination addresses",
            Self::Redirect => "redirect target violates network policy",
        };
        formatter.write_str(message)
    }
}

impl StdError for PolicyDnsError {}

#[derive(Debug)]
struct ResolverDnsError {
    kind: io::ErrorKind,
}

impl fmt::Display for ResolverDnsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "destination DNS resolution failed ({:?})",
            self.kind
        )
    }
}

impl StdError for ResolverDnsError {}

pub(crate) fn map_transport_error(error: reqwest::Error) -> Error {
    let error = error.without_url();
    let mut source: Option<&(dyn StdError + 'static)> = Some(&error);
    while let Some(current) = source {
        if let Some(policy) = current.downcast_ref::<PolicyDnsError>() {
            return Error::Policy(policy.to_string());
        }
        if current.downcast_ref::<ResolverDnsError>().is_some() {
            return Error::Dns("destination resolution failed".into());
        }
        source = current.source();
    }
    if error.is_timeout() {
        return Error::Timeout {
            operation: "request",
        };
    }
    Error::Http(error)
}

pub(crate) fn validate_url(url: &Url, policy: NetworkPolicy) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::Policy("only HTTP(S) URLs are permitted".into()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Policy("URL credentials are not permitted".into()));
    }
    let host = url
        .host()
        .ok_or_else(|| Error::Policy("URL host is required".into()))?;

    match host {
        Host::Ipv4(address) => validate_address(policy, IpAddr::V4(address)),
        Host::Ipv6(address) => validate_address(policy, IpAddr::V6(address)),
        Host::Domain(domain) if policy == NetworkPolicy::PublicInternet => {
            let domain = domain.trim_end_matches('.');
            if domain.eq_ignore_ascii_case("localhost")
                || domain.to_ascii_lowercase().ends_with(".localhost")
            {
                Err(Error::Policy(
                    "localhost destinations are not permitted".into(),
                ))
            } else {
                Ok(())
            }
        }
        Host::Domain(_) => Ok(()),
    }
}

fn validate_address(policy: NetworkPolicy, address: IpAddr) -> Result<()> {
    if address_is_allowed(policy, address) {
        Ok(())
    } else {
        Err(Error::Policy(
            "destination address is outside the permitted network policy".into(),
        ))
    }
}

pub(crate) fn address_is_allowed(policy: NetworkPolicy, address: IpAddr) -> bool {
    policy == NetworkPolicy::AllowPrivate
        || match address {
            IpAddr::V4(address) => is_public_ipv4(address),
            IpAddr::V6(address) => is_public_ipv6(address),
        }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let value = u32::from(address);
    ![
        (Ipv4Addr::new(0, 0, 0, 0), 8),
        (Ipv4Addr::new(10, 0, 0, 0), 8),
        (Ipv4Addr::new(100, 64, 0, 0), 10),
        (Ipv4Addr::new(127, 0, 0, 0), 8),
        (Ipv4Addr::new(169, 254, 0, 0), 16),
        (Ipv4Addr::new(172, 16, 0, 0), 12),
        (Ipv4Addr::new(192, 0, 0, 0), 24),
        (Ipv4Addr::new(192, 0, 2, 0), 24),
        (Ipv4Addr::new(192, 88, 99, 0), 24),
        (Ipv4Addr::new(192, 168, 0, 0), 16),
        (Ipv4Addr::new(198, 18, 0, 0), 15),
        (Ipv4Addr::new(198, 51, 100, 0), 24),
        (Ipv4Addr::new(203, 0, 113, 0), 24),
        (Ipv4Addr::new(224, 0, 0, 0), 4),
        (Ipv4Addr::new(240, 0, 0, 0), 4),
    ]
    .into_iter()
    .any(|(network, prefix)| ipv4_in_prefix(value, u32::from(network), prefix))
}

fn ipv4_in_prefix(address: u32, network: u32, prefix: u32) -> bool {
    let mask = u32::MAX << (32 - prefix);
    address & mask == network & mask
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    let value = u128::from(address);
    let compatible_prefix = value >> 32;
    if compatible_prefix == 0 || compatible_prefix == u128::from(u16::MAX) {
        return is_public_ipv4(Ipv4Addr::from(value as u32));
    }
    if !ipv6_in_prefix(
        value,
        u128::from(Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0)),
        3,
    ) {
        return false;
    }

    ![
        (Ipv6Addr::new(0x0064, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
        (Ipv6Addr::new(0x0064, 0xff9b, 1, 0, 0, 0, 0, 0), 48),
        (Ipv6Addr::new(0x0100, 0, 0, 0, 0, 0, 0, 0), 64),
        (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
        (Ipv6Addr::new(0x2001, 2, 0, 0, 0, 0, 0, 0), 48),
        (Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0), 32),
        (Ipv6Addr::new(0x2001, 0x0010, 0, 0, 0, 0, 0, 0), 28),
        (Ipv6Addr::new(0x2001, 0x0020, 0, 0, 0, 0, 0, 0), 28),
        (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
        (Ipv6Addr::new(0x3ffe, 0, 0, 0, 0, 0, 0, 0), 16),
        (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
        (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
        (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
        (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10),
        (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
    ]
    .into_iter()
    .any(|(network, prefix)| ipv6_in_prefix(value, u128::from(network), prefix))
}

fn ipv6_in_prefix(address: u128, network: u128, prefix: u32) -> bool {
    let mask = u128::MAX << (128 - prefix);
    address & mask == network & mask
}

#[cfg(test)]
mod tests {
    use super::{address_is_allowed, NetworkPolicy};

    #[test]
    fn public_policy_allows_a_public_fixture_address() {
        assert!(address_is_allowed(
            NetworkPolicy::PublicInternet,
            "93.184.216.34".parse().unwrap()
        ));
        assert!(address_is_allowed(
            NetworkPolicy::PublicInternet,
            "2606:2800:220:1:248:1893:25c8:1946".parse().unwrap()
        ));
    }

    #[test]
    fn allow_private_allows_loopback_addresses() {
        assert!(address_is_allowed(
            NetworkPolicy::AllowPrivate,
            "127.0.0.1".parse().unwrap()
        ));
        assert!(address_is_allowed(
            NetworkPolicy::AllowPrivate,
            "::1".parse().unwrap()
        ));
    }

    #[test]
    fn public_policy_rejects_current_iana_non_global_ipv6_ranges() {
        for address in ["100:0:0:1::1", "3ffe::1", "3fff::1", "4000::1", "5f00::1"] {
            assert!(
                !address_is_allowed(NetworkPolicy::PublicInternet, address.parse().unwrap()),
                "{address} was accepted"
            );
        }
    }
}
