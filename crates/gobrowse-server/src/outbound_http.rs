//! Validated, pinned transport for outbound webhook attempts.
//!
//! This module deliberately keeps URL policy separate from delivery persistence:
//! every attempt resolves and classifies its target immediately before opening a
//! connection, and the approved addresses are pinned into the request client.

use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use gobrowse_core::sandbox::is_public_destination;
use reqwest::redirect::Policy;
use tokio::net::lookup_host;
use url::{Host, Url};

const MAX_TARGET_URL_LENGTH: usize = 2_048;
const MAX_DNS_ANSWERS: usize = 16;
const RESOLUTION_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

fn has_userinfo(raw: &str, url: &Url) -> bool {
    if url.username() != "" || url.password().is_some() {
        return true;
    }
    raw.split_once("://")
        .and_then(|(_, remainder)| remainder.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetPolicyError {
    InvalidUrl,
    UnsupportedScheme,
    HostRequired,
    UserInfoForbidden,
    FragmentForbidden,
    TooManyAddresses,
    EmptyAddresses,
    ResolutionFailed,
    PrivateAddress,
}

impl TargetPolicyError {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::InvalidUrl => "target_policy_denied",
            Self::UnsupportedScheme => "target_policy_denied",
            Self::HostRequired => "target_policy_denied",
            Self::UserInfoForbidden => "target_policy_denied",
            Self::FragmentForbidden => "target_policy_denied",
            Self::TooManyAddresses => "target_policy_denied",
            Self::EmptyAddresses => "target_policy_denied",
            Self::ResolutionFailed => "target_policy_denied",
            Self::PrivateAddress => "target_policy_denied",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedOutboundTarget {
    pub(crate) url: Url,
    pub(crate) host: String,
    pub(crate) addresses: Vec<SocketAddr>,
}

/// Parse and validate an outbound target against already-resolved addresses.
///
/// This pure entry point is used by deterministic tests and by the async DNS
/// entry point below. Every address must be public; mixed DNS answers are
/// rejected rather than partially accepted.
pub(crate) fn validate_resolved_target(
    raw: &str,
    addresses: &[SocketAddr],
) -> Result<ValidatedOutboundTarget, TargetPolicyError> {
    if raw.len() > MAX_TARGET_URL_LENGTH {
        return Err(TargetPolicyError::InvalidUrl);
    }
    let url = Url::parse(raw).map_err(|_| TargetPolicyError::InvalidUrl)?;
    if url.scheme() != "https" {
        return Err(TargetPolicyError::UnsupportedScheme);
    }
    if has_userinfo(raw, &url) {
        return Err(TargetPolicyError::UserInfoForbidden);
    }
    if url.fragment().is_some() {
        return Err(TargetPolicyError::FragmentForbidden);
    }
    let host = url
        .host_str()
        .ok_or(TargetPolicyError::HostRequired)?
        .to_owned();
    if host.is_empty() {
        return Err(TargetPolicyError::HostRequired);
    }
    if addresses.is_empty() {
        return Err(TargetPolicyError::EmptyAddresses);
    }
    if addresses.len() > MAX_DNS_ANSWERS {
        return Err(TargetPolicyError::TooManyAddresses);
    }
    if addresses
        .iter()
        .any(|address| !is_public_destination(address.ip()))
    {
        return Err(TargetPolicyError::PrivateAddress);
    }
    Ok(ValidatedOutboundTarget {
        url,
        host,
        addresses: addresses.to_vec(),
    })
}

/// Resolve and validate one target immediately before a delivery attempt.
pub(crate) async fn resolve_target(
    raw: &str,
) -> Result<ValidatedOutboundTarget, TargetPolicyError> {
    if raw.len() > MAX_TARGET_URL_LENGTH {
        return Err(TargetPolicyError::InvalidUrl);
    }
    let url = Url::parse(raw).map_err(|_| TargetPolicyError::InvalidUrl)?;
    if url.scheme() != "https" {
        return Err(TargetPolicyError::UnsupportedScheme);
    }
    if has_userinfo(raw, &url) {
        return Err(TargetPolicyError::UserInfoForbidden);
    }
    if url.fragment().is_some() {
        return Err(TargetPolicyError::FragmentForbidden);
    }
    if url.host_str().is_none() {
        return Err(TargetPolicyError::HostRequired);
    }
    let port = url
        .port_or_known_default()
        .ok_or(TargetPolicyError::InvalidUrl)?;
    let addresses = match url.host() {
        Some(Host::Ipv4(ip)) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
        Some(Host::Ipv6(ip)) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
        Some(Host::Domain(domain)) => {
            let resolved = tokio::time::timeout(RESOLUTION_TIMEOUT, lookup_host((domain, port)))
                .await
                .map_err(|_| TargetPolicyError::ResolutionFailed)?
                .map_err(|_| TargetPolicyError::ResolutionFailed)?;
            let addresses: Vec<_> = resolved.take(MAX_DNS_ANSWERS + 1).collect();
            addresses
        }
        None => return Err(TargetPolicyError::HostRequired),
    };
    validate_resolved_target(raw, &addresses)
}

/// Build a no-proxy, no-redirect client pinned to this attempt's answers.
pub(crate) fn pinned_client(
    target: &ValidatedOutboundTarget,
) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(Policy::none())
        .no_proxy()
        .resolve_to_addrs(&target.host, &target.addresses)
        .user_agent(concat!("gobrowse-os/", env!("CARGO_PKG_VERSION")))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public() -> SocketAddr {
        "1.1.1.1:443".parse().expect("public address")
    }

    #[test]
    fn rejects_non_https_and_url_credential_components() {
        for target in [
            "http://example.com/hook",
            "file:///tmp/hook",
            "https://user:password@example.com/hook",
            "https://@example.com/hook",
            "https://example.com/hook#fragment",
            "https://",
            "https://example.com:not-a-port/hook",
        ] {
            assert!(
                validate_resolved_target(target, &[public()]).is_err(),
                "{target}"
            );
        }
    }

    #[test]
    fn rejects_private_mapped_and_mixed_answers() {
        for address in [
            "127.0.0.1:443",
            "10.0.0.1:443",
            "169.254.169.254:443",
            "[::1]:443",
            "[::ffff:127.0.0.1]:443",
        ] {
            let address = address.parse().expect("test address");
            assert!(validate_resolved_target("https://example.com/hook", &[address]).is_err());
        }
        let private = "192.168.1.1:443".parse().expect("private address");
        assert!(
            validate_resolved_target("https://example.com/hook", &[public(), private]).is_err()
        );
    }

    #[test]
    fn accepts_public_answers_and_retains_hostname_for_pinning() {
        let target = validate_resolved_target(
            "https://Example.com:8443/hook?event=one",
            &["1.1.1.1:8443".parse().expect("public address")],
        )
        .expect("public target");
        assert_eq!(target.host, "example.com");
        assert_eq!(target.addresses.len(), 1);
        assert_eq!(target.url.scheme(), "https");
        assert_eq!(target.url.port(), Some(8443));
        assert_eq!(
            TargetPolicyError::PrivateAddress.code(),
            "target_policy_denied"
        );
    }

    #[test]
    fn rejects_empty_and_excessive_answer_sets() {
        assert!(
            validate_resolved_target(
                &format!("https://example.com/{}", "x".repeat(MAX_TARGET_URL_LENGTH)),
                &[public()],
            )
            .is_err()
        );
        assert_eq!(
            validate_resolved_target("https://example.com/hook", &[]),
            Err(TargetPolicyError::EmptyAddresses)
        );
        let answers: Vec<_> = (0..=MAX_DNS_ANSWERS)
            .map(|octet| SocketAddr::new(IpAddr::V4([1, 1, 1, octet as u8].into()), 443))
            .collect();
        assert_eq!(
            validate_resolved_target("https://example.com/hook", &answers),
            Err(TargetPolicyError::TooManyAddresses)
        );
    }
}
