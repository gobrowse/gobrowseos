//! Validated, pinned transport for outbound webhook attempts.
//!
//! Every attempt resolves and classifies its target immediately before opening
//! a connection, and the approved addresses are pinned into that connection.

use std::{
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
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

pub type ResolverFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<SocketAddr>, TargetPolicyError>> + Send + 'a>>;
pub type TransportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + 'a>>;

fn has_userinfo(raw: &str, url: &Url) -> bool {
    if url.username() != "" || url.password().is_some() {
        return true;
    }
    raw.split_once("://")
        .and_then(|(_, remainder)| remainder.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetPolicyError {
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
    pub const fn code(self) -> &'static str {
        "target_policy_denied"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedOutboundTarget {
    pub(crate) url: Url,
    pub(crate) host: String,
    pub(crate) addresses: Vec<SocketAddr>,
}

/// A resolver used immediately before each outbound attempt.
pub trait OutboundResolver: Send + Sync {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolverFuture<'a>;
}

/// A transport that can send only an already validated and pinned target.
pub trait OutboundTransport: Send + Sync {
    fn send<'a>(
        &'a self,
        target: &'a ValidatedOutboundTarget,
        delivery_id: &'a str,
        payload: &'a str,
        signature: Option<&'a str>,
    ) -> TransportFuture<'a>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportResponse {
    pub status: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    Timeout,
    Connection,
    Other,
}

impl TransportError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Timeout => "target_timeout",
            Self::Connection => "target_connection_failed",
            Self::Other => "target_transport_failed",
        }
    }
}

/// Resolver/transport pair passed through every delivery task.
#[derive(Clone)]
pub struct WebhookDeliveryDeps {
    pub resolver: Arc<dyn OutboundResolver>,
    pub transport: Arc<dyn OutboundTransport>,
}

impl WebhookDeliveryDeps {
    pub fn production() -> Self {
        Self {
            resolver: Arc::new(SystemResolver),
            transport: Arc::new(PinnedHttpsTransport),
        }
    }
}

/// Tokio system resolver with a bounded lookup deadline and answer count.
pub struct SystemResolver;

impl OutboundResolver for SystemResolver {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolverFuture<'a> {
        Box::pin(async move {
            let resolved = tokio::time::timeout(RESOLUTION_TIMEOUT, lookup_host((host, port)))
                .await
                .map_err(|_| TargetPolicyError::ResolutionFailed)?
                .map_err(|_| TargetPolicyError::ResolutionFailed)?;
            Ok(resolved.take(MAX_DNS_ANSWERS + 1).collect())
        })
    }
}

/// Reqwest transport with no proxy/redirect behavior and static address pins.
pub struct PinnedHttpsTransport;

impl OutboundTransport for PinnedHttpsTransport {
    fn send<'a>(
        &'a self,
        target: &'a ValidatedOutboundTarget,
        delivery_id: &'a str,
        payload: &'a str,
        signature: Option<&'a str>,
    ) -> TransportFuture<'a> {
        Box::pin(async move {
            let client = pinned_client(target).map_err(|_| TransportError::Other)?;
            let mut request = client
                .post(target.url.clone())
                .header("X-Gobrowse-Delivery", delivery_id)
                .body(payload.to_owned());
            if let Some(signature) = signature {
                request = request.header("X-Gobrowse-Signature", signature);
            }
            let response = request.send().await.map_err(|error| {
                if error.is_timeout() {
                    TransportError::Timeout
                } else if error.is_connect() {
                    TransportError::Connection
                } else {
                    TransportError::Other
                }
            })?;
            Ok(TransportResponse {
                status: response.status().as_u16(),
            })
        })
    }
}

/// Parse and validate an outbound target against already-resolved addresses.
pub fn validate_resolved_target(
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
pub async fn resolve_target_with<R: OutboundResolver + ?Sized>(
    resolver: &R,
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
        Some(Host::Domain(domain)) => resolver.resolve(domain, port).await?,
        None => return Err(TargetPolicyError::HostRequired),
    };
    validate_resolved_target(raw, &addresses)
}

fn pinned_client(target: &ValidatedOutboundTarget) -> Result<reqwest::Client, reqwest::Error> {
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
            "[64:ff9b::a9fe:a9fe]:443",
            "[64:ff9b::7f00:1]:443",
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
