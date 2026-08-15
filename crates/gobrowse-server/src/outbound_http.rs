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

fn classify_transport_error(error: &reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else if error.is_connect() {
        TransportError::Connection
    } else {
        TransportError::Other
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
            transport: Arc::new(PinnedHttpsTransport::default()),
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
#[derive(Default)]
pub struct PinnedHttpsTransport {
    #[cfg(test)]
    root: Option<reqwest::Certificate>,
    #[cfg(test)]
    timeout: Option<Duration>,
}

#[cfg(test)]
impl PinnedHttpsTransport {
    fn for_test(root: reqwest::Certificate, timeout: Duration) -> Self {
        Self {
            root: Some(root),
            timeout: Some(timeout),
        }
    }
}

impl OutboundTransport for PinnedHttpsTransport {
    fn send<'a>(
        &'a self,
        target: &'a ValidatedOutboundTarget,
        delivery_id: &'a str,
        payload: &'a str,
        signature: Option<&'a str>,
    ) -> TransportFuture<'a> {
        Box::pin(async move {
            let client = self.client(target).map_err(|_| TransportError::Other)?;
            let mut request = client
                .post(target.url.clone())
                .header("X-Gobrowse-Delivery", delivery_id)
                .body(payload.to_owned());
            if let Some(signature) = signature {
                request = request.header("X-Gobrowse-Signature", signature);
            }
            let response = request
                .send()
                .await
                .map_err(|error| classify_transport_error(&error))?;
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

impl PinnedHttpsTransport {
    fn client(&self, target: &ValidatedOutboundTarget) -> Result<reqwest::Client, reqwest::Error> {
        let timeout = {
            #[cfg(test)]
            {
                self.timeout.unwrap_or(REQUEST_TIMEOUT)
            }
            #[cfg(not(test))]
            {
                REQUEST_TIMEOUT
            }
        };
        let builder = pinned_client_builder(target, CONNECT_TIMEOUT.min(timeout), timeout);
        #[cfg(test)]
        let builder = if let Some(root) = &self.root {
            builder.add_root_certificate(root.clone())
        } else {
            builder
        };
        builder.build()
    }
}

fn pinned_client_builder(
    target: &ValidatedOutboundTarget,
    connect_timeout: Duration,
    request_timeout: Duration,
) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .redirect(Policy::none())
        .no_proxy()
        .resolve_to_addrs(&target.host, &target.addresses)
        .user_agent(concat!("gobrowse-os/", env!("CARGO_PKG_VERSION")))
}

#[cfg(test)]
mod tests {
    use std::{
        process::Stdio,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use base64::Engine;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        process::Command,
    };
    use tokio_rustls::{
        TlsAcceptor,
        rustls::{
            self,
            pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
            time_provider::TimeProvider,
        },
    };

    use super::*;

    const TEST_CERT: &[u8] = include_bytes!("../tests/fixtures/webhook-test-cert.pem");
    const TEST_CA: &[u8] = include_bytes!("../tests/fixtures/webhook-test-ca.pem");
    const TEST_KEY: &[u8] = include_bytes!("../tests/fixtures/webhook-test-key.pem");

    #[derive(Debug)]
    struct FixedTime(UnixTime);

    impl TimeProvider for FixedTime {
        fn current_time(&self) -> Option<UnixTime> {
            Some(self.0)
        }
    }

    fn public() -> SocketAddr {
        "1.1.1.1:443".parse().expect("public address")
    }

    fn pem_der(pem: &[u8]) -> Vec<u8> {
        let mut lines = std::str::from_utf8(pem)
            .expect("PEM UTF-8")
            .lines()
            .filter(|line| !line.starts_with('-'));
        let body: String = lines.by_ref().collect();
        base64::engine::general_purpose::STANDARD
            .decode(body)
            .expect("decode PEM")
    }

    async fn tls_server(
        response: Vec<u8>,
    ) -> (
        SocketAddr,
        Arc<Mutex<Option<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let cert = CertificateDer::from(pem_der(TEST_CERT));
        let key = PrivateKeyDer::Pkcs8(pem_der(TEST_KEY).into());
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .expect("TLS test config");
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("TLS listener");
        let address = listener.local_addr().expect("TLS address");
        let seen = Arc::new(Mutex::new(None));
        let seen_by_server = seen.clone();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("TLS connection");
            let Ok(mut stream) = acceptor.accept(stream).await else {
                return;
            };
            let mut request = vec![0u8; 8192];
            let Ok(count) = stream.read(&mut request).await else {
                return;
            };
            let request = String::from_utf8_lossy(&request[..count]);
            let sni = stream
                .get_ref()
                .1
                .server_name()
                .unwrap_or_default()
                .to_owned();
            *seen_by_server.lock().expect("record TLS metadata") =
                Some(format!("{sni}\n{request}"));
            if response.is_empty() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            } else {
                stream.write_all(&response).await.expect("write response");
            }
        });
        (address, seen, task)
    }

    fn target(address: SocketAddr) -> ValidatedOutboundTarget {
        ValidatedOutboundTarget {
            url: Url::parse("https://hooks.example.test/deliver").expect("test URL"),
            host: "hooks.example.test".to_owned(),
            addresses: vec![address],
        }
    }

    async fn sentinel_listener() -> (SocketAddr, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        sentinel_listener_for(Duration::from_millis(200)).await
    }

    async fn sentinel_listener_for(
        wait: Duration,
    ) -> (SocketAddr, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("sentinel listener");
        let address = listener.local_addr().expect("sentinel address");
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_by_server = calls.clone();
        let task = tokio::spawn(async move {
            if tokio::time::timeout(wait, listener.accept())
                .await
                .ok()
                .and_then(Result::ok)
                .is_some()
            {
                calls_by_server.fetch_add(1, Ordering::SeqCst);
            }
        });
        (address, calls, task)
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

    #[tokio::test]
    async fn trust_material_verifies_now_and_five_years_forward() {
        let now = UnixTime::now();
        let now_datetime = time::OffsetDateTime::from_unix_timestamp(now.as_secs() as i64)
            .expect("current certificate time");
        let future_year = now_datetime.year() + 5;
        let future_date =
            time::Date::from_calendar_date(future_year, now_datetime.month(), now_datetime.day())
                .or_else(|_| {
                    time::Date::from_calendar_date(
                        future_year,
                        now_datetime.month(),
                        now_datetime.day() - 1,
                    )
                })
                .expect("five-year calendar date");
        let future_datetime = future_date.with_time(now_datetime.time()).assume_utc();
        assert_eq!(future_datetime.year(), future_year);
        let future = UnixTime::since_unix_epoch(std::time::Duration::from_secs(
            future_datetime.unix_timestamp() as u64,
        ));
        for verification_time in [now, future] {
            let response = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_vec();
            let (address, _seen, server_task) = tls_server(response).await;
            let mut roots = rustls::RootCertStore::empty();
            roots
                .add(CertificateDer::from(pem_der(TEST_CA)))
                .expect("test CA");
            let config = rustls::ClientConfig::builder_with_details(
                Arc::new(rustls::crypto::ring::default_provider()),
                Arc::new(FixedTime(verification_time)),
            )
            .with_safe_default_protocol_versions()
            .expect("TLS protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
            let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
            let stream = tokio::net::TcpStream::connect(address)
                .await
                .expect("future TLS connection");
            let server_name = ServerName::try_from("hooks.example.test")
                .expect("server name")
                .to_owned();
            connector
                .connect(server_name, stream)
                .await
                .expect("certificate valid at verification time");
            server_task.await.expect("TLS server");
        }
    }

    #[tokio::test]
    async fn pinned_transport_preserves_hostname_and_sni() {
        let response = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_vec();
        let (address, seen, task) = tls_server(response).await;
        let target = target(SocketAddr::new(address.ip(), address.port()));
        let root = reqwest::Certificate::from_pem(TEST_CA).expect("test root");
        let transport = PinnedHttpsTransport::for_test(root, Duration::from_secs(2));
        let response = transport
            .send(&target, "delivery-123", "body", Some("sha256=fixture"))
            .await
            .expect("TLS request");
        assert_eq!(response.status, 204);
        task.await.expect("TLS server");
        let seen = seen
            .lock()
            .expect("read metadata")
            .clone()
            .expect("metadata");
        assert!(seen.starts_with("hooks.example.test\n"));
        let request = seen.to_ascii_lowercase();
        assert!(request.starts_with("hooks.example.test\npost /deliver http/1.1"));
        assert!(request.contains("host: hooks.example.test"));
        assert!(request.contains("x-gobrowse-delivery: delivery-123"));
        assert!(request.contains("x-gobrowse-signature: sha256=fixture"));
        assert!(request.ends_with("body"));
    }

    #[tokio::test]
    async fn wrong_hostname_certificate_is_a_redacted_transport_failure() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec();
        let (address, _seen, task) = tls_server(response).await;
        let target = ValidatedOutboundTarget {
            url: Url::parse("https://wrong.example.test/deliver").expect("wrong URL"),
            host: "wrong.example.test".to_owned(),
            addresses: vec![SocketAddr::new(address.ip(), address.port())],
        };
        let root = reqwest::Certificate::from_pem(TEST_CA).expect("test root");
        let transport = PinnedHttpsTransport::for_test(root, Duration::from_secs(2));
        let error = transport
            .send(&target, "delivery", "body", None)
            .await
            .expect_err("hostname mismatch");
        assert_eq!(error, TransportError::Connection);
        assert_eq!(error.code(), "target_connection_failed");
        assert!(!error.code().contains("wrong.example.test"));
        task.await.expect("TLS server");
    }

    #[tokio::test]
    async fn redirects_are_not_followed_to_302_or_307_sentinels() {
        for status in [302, 307] {
            let (sentinel, calls, sentinel_task) = sentinel_listener().await;
            let response = format!(
                "HTTP/1.1 {status} Redirect\r\nLocation: https://127.0.0.1:{}/hit\r\nContent-Length: 0\r\n\r\n",
                sentinel.port()
            )
            .into_bytes();
            let (address, _seen, task) = tls_server(response).await;
            let target = target(SocketAddr::new(address.ip(), address.port()));
            let root = reqwest::Certificate::from_pem(TEST_CA).expect("test root");
            let transport = PinnedHttpsTransport::for_test(root, Duration::from_secs(2));
            let response = transport
                .send(&target, "delivery", "body", None)
                .await
                .expect("redirect response");
            assert_eq!(response.status, status);
            task.await.expect("TLS server");
            sentinel_task.await.expect("redirect sentinel");
            assert_eq!(calls.load(Ordering::SeqCst), 0, "redirect target contacted");
        }
    }

    #[tokio::test]
    async fn refused_connection_is_a_stable_redacted_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("refused listener");
        let address = listener.local_addr().expect("refused address");
        drop(listener);
        let target = target(address);
        let root = reqwest::Certificate::from_pem(TEST_CA).expect("test root");
        let transport = PinnedHttpsTransport::for_test(root, Duration::from_millis(100));
        let error = transport
            .send(&target, "delivery", "body", None)
            .await
            .expect_err("refused connection");
        assert_eq!(error, TransportError::Connection);
        assert_eq!(error.code(), "target_connection_failed");
    }

    #[tokio::test]
    async fn stalled_tls_handshake_is_a_bounded_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("stalled listener");
        let address = listener.local_addr().expect("stalled address");
        let task = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("stalled connection");
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let target = target(address);
        let root = reqwest::Certificate::from_pem(TEST_CA).expect("test root");
        let transport = PinnedHttpsTransport::for_test(root, Duration::from_millis(50));
        let error = transport
            .send(&target, "delivery", "body", None)
            .await
            .expect_err("stalled handshake");
        assert_eq!(error, TransportError::Timeout);
        task.await.expect("stalled server");
    }

    #[tokio::test]
    async fn proxy_environment_is_ignored_in_an_isolated_child() {
        if std::env::var_os("GOBROWSE_PROXY_CHILD").is_some() {
            let port: u16 = std::env::var("GOBROWSE_PROXY_TARGET_PORT")
                .expect("child target port")
                .parse()
                .expect("child target port number");
            let target = target(SocketAddr::from(([127, 0, 0, 1], port)));
            let root = reqwest::Certificate::from_pem(TEST_CA).expect("test root");
            let transport = PinnedHttpsTransport::for_test(root, Duration::from_secs(2));
            let response = transport
                .send(&target, "delivery", "body", None)
                .await
                .expect("direct child request");
            assert_eq!(response.status, 204);
            return;
        }

        let response = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_vec();
        let (address, _seen, server_task) = tls_server(response).await;
        let (proxy, calls, proxy_task) = sentinel_listener_for(Duration::from_secs(5)).await;
        let proxy_url = format!("http://127.0.0.1:{}/proxy", proxy.port());
        let mut child = Command::new(std::env::current_exe().expect("test executable"));
        child
            .arg("--exact")
            .arg("outbound_http::tests::proxy_environment_is_ignored_in_an_isolated_child")
            .arg("--nocapture")
            .env("GOBROWSE_PROXY_CHILD", "1")
            .env("GOBROWSE_PROXY_TARGET_PORT", address.port().to_string())
            .env("HTTPS_PROXY", &proxy_url)
            .env("https_proxy", &proxy_url)
            .env("ALL_PROXY", &proxy_url)
            .env("all_proxy", &proxy_url)
            .env("NO_PROXY", "")
            .env("no_proxy", "")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let status = tokio::time::timeout(Duration::from_secs(5), child.status())
            .await
            .expect("proxy child timeout")
            .expect("proxy child status");
        assert!(status.success(), "proxy child failed: {status}");
        server_task.await.expect("TLS server");
        proxy_task.await.expect("proxy sentinel");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "proxy sentinel contacted");
    }

    #[tokio::test]
    async fn request_timeout_is_bounded_and_redacted() {
        let (address, _seen, task) = tls_server(Vec::new()).await;
        let target = target(SocketAddr::new(address.ip(), address.port()));
        let root = reqwest::Certificate::from_pem(TEST_CA).expect("test root");
        let transport = PinnedHttpsTransport::for_test(root, Duration::from_millis(50));
        let error = transport
            .send(&target, "delivery", "body", None)
            .await
            .expect_err("request timeout");
        assert_eq!(error, TransportError::Timeout);
        assert!(!error.code().contains("hooks.example.test"));
        task.await.expect("TLS server");
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
