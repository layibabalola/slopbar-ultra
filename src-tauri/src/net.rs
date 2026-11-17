//! SlopBar's own HTTPS clients, and why a request failed.
//!
//! Every client SlopBar makes starts from [`builder`]: it trusts what Windows trusts, through
//! Windows' own certificate checks (rustls-platform-verifier, as the updater does), so a network
//! that inspects HTTPS with a company certificate installed in Windows works, while a certificate
//! Windows refuses is refused here too. Each request goes through the proxy Windows would use for
//! it (`proxy`: the HTTPS_PROXY/NO_PROXY variables first, then this user's proxy settings, proxy
//! scripts and detection included). Nothing here skips a check or goes around a proxy: a network
//! that blocks a request still blocks it, a proxy SlopBar cannot use stops it, and [`why`] says how.

/// The cryptography every client uses: ring, which the updater installs the same way when it
/// checks first. Clients cannot be built without it.
pub fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            // Fails only when another thread installed one meanwhile (the updater: also ring).
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
    });
}

/// A client builder that verifies certificates with Windows' own checks and sends each request
/// through the proxy Windows would use for it. Callers add their own timeout and redirect policy;
/// `.no_proxy()` (only for servers on this computer or its local network) goes direct.
pub fn builder() -> reqwest::ClientBuilder {
    install_crypto();
    windows_proxy(reqwest::Client::builder())
}

/// `builder` routes each request where Windows would (`proxy::Router`), for a client built
/// elsewhere too (the updater's). A route SlopBar cannot use fails the request with its reason
/// (`Why::ProxyUnsupported`, `Why::ProxyScript`, `Why::ProxyEnvironment`) before anything leaves
/// this computer.
pub fn windows_proxy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    with_router(builder, crate::proxy::system().clone())
}

/// `windows_proxy` with these routes.
pub(crate) fn with_router(builder: reqwest::ClientBuilder, router: std::sync::Arc<crate::proxy::Router>) -> reqwest::ClientBuilder {
    builder
        .proxy(reqwest::Proxy::custom(move |url| router.proxy_for(url)))
        .dns_resolver(crate::proxy::Resolver)
}

/// Why a request failed, in words a person can act on. Never carries a link, a header or a
/// server's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Why {
    /// The certificate chains to no root this computer trusts.
    Untrusted,
    /// Windows refuses this certificate on purpose: it, or its issuer, is in the Disallowed store,
    /// or its issuer revoked it (Windows reports both as revoked).
    Distrusted,
    /// The certificate is out of date (or not valid yet: the clock may be wrong).
    Expired,
    /// The certificate is for another name: what a network that redirects blocked names shows.
    WrongName,
    /// Windows refused the certificate for another reason (malformed, misused).
    Certificate,
    /// The other end refused the secure connection (an alert), or broke it off mid-handshake.
    Tls,
    /// The answer was not a secure connection at all: something on the network answered in plain
    /// text in the site's place.
    NotSecure,
    /// The network's proxy asks for a sign-in.
    ProxyAuth,
    /// The network's proxy would not connect to the host.
    ProxyRefused,
    /// Windows is set to use a proxy SlopBar cannot use (SOCKS, or a kind it does not know), so
    /// SlopBar does not connect at all.
    ProxyUnsupported,
    /// Windows' proxy script (set, or found on the network) could not be fetched or run in time,
    /// so SlopBar does not connect without it.
    ProxyScript,
    /// The HTTPS_PROXY or ALL_PROXY environment variable names a proxy SlopBar cannot use (only an
    /// http or https proxy works), so SlopBar does not connect at all.
    ProxyEnvironment,
    /// Windows' proxy script picks a proxy per site, and rclone, which takes one for a whole
    /// cloud, cannot be given one SlopBar is sure of: rclone does not run (`proxy::rclone_env`).
    /// Never a request's own failure.
    ProxyRclone,
    /// The host's name could not be found.
    Dns,
    /// The host (or something in between) refused the connection.
    Refused,
    /// The connection was cut off.
    Reset,
    /// No answer in time.
    Timeout,
    /// No connection, otherwise.
    Connect,
    /// A redirect the client does not follow.
    Redirected,
    /// An HTTP error status.
    Status,
    /// A web page (HTML) or another small answer instead of the file: a block page, typically.
    Page,
    /// Far larger than the file.
    TooLarge,
    /// The answer broke off.
    Interrupted,
    /// The file did not match its published checksum.
    Mismatch,
    /// Anything else.
    Failed,
}

impl Why {
    /// Whether this looks like the network (or Windows) refusing on purpose, rather than the
    /// source being down or broken. After such a failure, SlopBar tries no other source for the
    /// same file: a download a company blocks stays blocked, and says why.
    pub fn blocks(self, status: Option<u16>) -> bool {
        match self {
            Why::Status => !status.is_some_and(|s| matches!(s, 404 | 410 | 429 | 500..=599)),
            Why::Mismatch | Why::Interrupted | Why::TooLarge => false,
            _ => true,
        }
    }

    /// The code shown when the network's proxy stopped the request (`Why`'s own name), which
    /// `errorText` (i18n.js) and `widget::error_text` word: a proxy SlopBar does not go around
    /// (`proxy_unsupported`, `proxy_script`, `proxy_environment`, and for rclone `proxy_rclone`),
    /// one that wants a sign-in SlopBar cannot give (`proxy_auth`), one that refuses
    /// (`proxy_refused`). None for any other failure.
    pub fn proxy_code(self) -> Option<&'static str> {
        match self {
            Why::ProxyUnsupported => Some("proxy_unsupported"),
            Why::ProxyScript => Some("proxy_script"),
            Why::ProxyEnvironment => Some("proxy_environment"),
            Why::ProxyRclone => Some("proxy_rclone"),
            Why::ProxyAuth => Some("proxy_auth"),
            Why::ProxyRefused => Some("proxy_refused"),
            _ => None,
        }
    }
}

/// Every code `Why::proxy_code` gives: what Settings, the flyout, the widget, Health and
/// diagnostics treat as the network's proxy stopping SlopBar.
pub const PROXY_CODES: &[&str] = &["proxy_unsupported", "proxy_script", "proxy_environment", "proxy_rclone", "proxy_auth", "proxy_refused"];

/// Windows' errors that the verifier passes on as its OS error text (the text is translated, the
/// number is not): CERT_E_CHAINING, no chain to a trusted root (a missing intermediate, say);
/// TRUST_E_EXPLICIT_DISTRUST, a certificate marked untrusted; and CERT_E_UNTRUSTEDCA, an
/// authority the policy does not trust.
const CERT_E_CHAINING: &str = "(os error -2146762486)";
const TRUST_E_EXPLICIT_DISTRUST: &str = "(os error -2146762479)";
const CERT_E_UNTRUSTEDCA: &str = "(os error -2146762478)";

/// Why `error` happened: the certificate check's verdict when there was one, else the proxy's,
/// the connection's or reqwest's own kind. Reads the errors under reqwest's own, whose text has
/// the request's link in it; keeps no text.
pub fn why(error: &reqwest::Error) -> Why {
    let mut found = None;
    let mut text = String::new();
    let mut cause = std::error::Error::source(error);
    while let Some(e) = cause {
        if let Some(tls) = inspect(e, &mut found) {
            return tls;
        }
        text.push_str(&e.to_string());
        text.push('\n');
        cause = e.source();
    }
    // hyper-util's tunnel errors are private: their words are what is left of them.
    if text.contains("tunnel error: proxy authorization required") {
        return Why::ProxyAuth;
    }
    if text.contains("tunnel error: unsuccessful") {
        return Why::ProxyRefused;
    }
    if let Some(why) = found {
        return why;
    }
    if error.is_dns() || text.contains("dns error") {
        Why::Dns
    } else if error.is_timeout() {
        Why::Timeout
    } else if error.is_connect() {
        Why::Connect
    } else if error.is_body() || error.is_decode() {
        Why::Interrupted
    } else {
        Why::Failed
    }
}

/// The code of the network's proxy stopping `error`'s request (`Why::proxy_code`), if it did.
pub fn proxy_code(error: &reqwest::Error) -> Option<&'static str> {
    why(error).proxy_code()
}

/// What a usage request's failure shows: the code of the network's proxy stopping it
/// (`proxy_code`: the flyout words it), else reqwest's own text, as before.
pub fn failure_text(error: &reqwest::Error) -> String {
    proxy_code(error).map_or_else(|| error.to_string(), str::to_string)
}

/// Whether `error` is an answer cut short: the other end closed before the whole body came.
pub fn cut_short(error: &reqwest::Error) -> bool {
    fn eof(e: &(dyn std::error::Error + 'static)) -> bool {
        e.downcast_ref::<std::io::Error>().is_some_and(|io| {
            io.kind() == std::io::ErrorKind::UnexpectedEof || io.get_ref().is_some_and(|inner| eof(inner))
        })
    }
    let mut cause = std::error::Error::source(error);
    while let Some(e) = cause {
        if eof(e) {
            return true;
        }
        cause = e.source();
    }
    false
}

/// The certificate check's (or TLS's) verdict in `error`, looking inside every io::Error it wraps,
/// however deep: io::Error's source() skips the error it carries, and on a direct connection
/// rustls's error sits two io::Errors down (hyper-rustls's, then tokio-rustls's). Notes in `found`
/// the first connection failure an io::Error's kind names.
fn inspect(error: &(dyn std::error::Error + 'static), found: &mut Option<Why>) -> Option<Why> {
    // Refused before anything left this computer (`proxy::Resolver`): under a "dns error".
    if let Some(refused) = error.downcast_ref::<crate::proxy::ProxyRefused>() {
        return Some(refused.0.why());
    }
    if let Some(tls) = error.downcast_ref::<rustls::Error>() {
        return Some(tls_why(tls));
    }
    let io = error.downcast_ref::<std::io::Error>()?;
    if let Some(inner) = io.get_ref() {
        if let Some(tls) = inspect(inner, found) {
            return Some(tls);
        }
    }
    if found.is_none() {
        *found = io_why(io);
    }
    None
}

fn tls_why(error: &rustls::Error) -> Why {
    use rustls::CertificateError as C;
    match error {
        rustls::Error::InvalidCertificate(C::UnknownIssuer) => Why::Untrusted,
        rustls::Error::InvalidCertificate(C::Other(other)) => {
            let text = other.to_string();
            if text.contains(TRUST_E_EXPLICIT_DISTRUST) || text.contains(CERT_E_UNTRUSTEDCA) {
                Why::Distrusted
            } else if text.contains(CERT_E_CHAINING) {
                Why::Untrusted
            } else {
                Why::Certificate
            }
        }
        rustls::Error::InvalidCertificate(C::Expired | C::ExpiredContext { .. } | C::NotValidYet | C::NotValidYetContext { .. }) => Why::Expired,
        rustls::Error::InvalidCertificate(C::NotValidForName | C::NotValidForNameContext { .. }) => Why::WrongName,
        // Windows reports a certificate in its Disallowed store as revoked (CRYPT_E_REVOKED), as
        // it does one its issuer revoked: refused on purpose, either way.
        rustls::Error::InvalidCertificate(C::Revoked) => Why::Distrusted,
        rustls::Error::InvalidCertificate(_) => Why::Certificate,
        // Bytes that are no TLS record: a plain-text answer on the secure port.
        rustls::Error::InvalidMessage(_) => Why::NotSecure,
        _ => Why::Tls,
    }
}

fn io_why(error: &std::io::Error) -> Option<Why> {
    use std::io::ErrorKind as K;
    // Windows Sockets' name lookup failures: WSAHOST_NOT_FOUND, WSATRY_AGAIN, WSANO_RECOVERY,
    // WSANO_DATA.
    if cfg!(windows) && matches!(error.raw_os_error(), Some(11001..=11004)) {
        return Some(Why::Dns);
    }
    match error.kind() {
        K::ConnectionRefused => Some(Why::Refused),
        // The other end hung up mid-handshake: what a firewall that blocks by name does.
        K::ConnectionReset | K::ConnectionAborted | K::UnexpectedEof => Some(Why::Reset),
        K::TimedOut => Some(Why::Timeout),
        _ => None,
    }
}

/// The host part of `url`, for showing where a request went: never its path or query (a signed
/// download link carries its signature there). A link that is not http(s) shows its scheme alone.
pub fn shown_host(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or_default().chars().take(253).collect::<String>();
    match url.scheme() {
        "https" => host,
        "http" => format!("http://{host}"),
        other => format!("{other}:"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificate_verdicts_become_plain_reasons() {
        use rustls::CertificateError as C;
        let cert = |e| tls_why(&rustls::Error::InvalidCertificate(e));
        assert_eq!(cert(C::UnknownIssuer), Why::Untrusted);
        let chaining = rustls::OtherError(std::sync::Arc::from(Box::<dyn std::error::Error + Send + Sync>::from(
            std::io::Error::from_raw_os_error(-2146762486_i32).to_string())));
        assert_eq!(cert(C::Other(chaining)), Why::Untrusted, "no chain to a trusted root (a missing intermediate)");
        assert_eq!(cert(C::Expired), Why::Expired);
        assert_eq!(cert(C::NotValidYet), Why::Expired);
        assert_eq!(cert(C::NotValidForName), Why::WrongName);
        assert_eq!(cert(C::Revoked), Why::Distrusted, "Windows reports a certificate in the Disallowed store as revoked");
        assert_eq!(cert(C::InvalidPurpose), Why::Certificate);
        let distrusted = rustls::OtherError(std::sync::Arc::from(Box::<dyn std::error::Error + Send + Sync>::from(
            std::io::Error::from_raw_os_error(-2146762479_i32).to_string())));
        assert_eq!(cert(C::Other(distrusted)), Why::Distrusted, "explicitly distrusted is refused, not merely unknown");
        let other = rustls::OtherError(std::sync::Arc::from(Box::<dyn std::error::Error + Send + Sync>::from("anything else")));
        assert_eq!(cert(C::Other(other)), Why::Certificate);
        assert_eq!(tls_why(&rustls::Error::AlertReceived(rustls::AlertDescription::HandshakeFailure)), Why::Tls);
        assert_eq!(tls_why(&rustls::Error::InvalidMessage(rustls::InvalidMessage::InvalidContentType)), Why::NotSecure);
    }

    #[test]
    fn only_a_source_that_is_down_or_broken_lets_the_next_one_be_tried() {
        for why in [Why::Untrusted, Why::Distrusted, Why::Expired, Why::WrongName, Why::Certificate, Why::Tls, Why::NotSecure,
            Why::ProxyAuth, Why::ProxyRefused, Why::ProxyUnsupported, Why::ProxyScript, Why::ProxyEnvironment, Why::ProxyRclone,
            Why::Dns, Why::Refused, Why::Reset, Why::Timeout, Why::Connect, Why::Redirected, Why::Page, Why::Failed] {
            assert!(why.blocks(None), "{why:?} may be a block");
        }
        for status in [401, 403, 407, 451, 400] {
            assert!(Why::Status.blocks(Some(status)), "{status}");
        }
        for status in [404, 410, 429, 500, 502, 503] {
            assert!(!Why::Status.blocks(Some(status)), "{status}");
        }
        for why in [Why::Mismatch, Why::Interrupted, Why::TooLarge] {
            assert!(!why.blocks(Some(200)), "{why:?}");
        }
    }

    #[test]
    fn connection_failures_become_plain_reasons() {
        use std::io::{Error, ErrorKind};
        assert_eq!(io_why(&Error::from(ErrorKind::ConnectionRefused)), Some(Why::Refused));
        assert_eq!(io_why(&Error::from(ErrorKind::ConnectionReset)), Some(Why::Reset));
        assert_eq!(io_why(&Error::from(ErrorKind::UnexpectedEof)), Some(Why::Reset));
        assert_eq!(io_why(&Error::from(ErrorKind::TimedOut)), Some(Why::Timeout));
        #[cfg(windows)]
        assert_eq!(io_why(&Error::from_raw_os_error(11001)), Some(Why::Dns));
        assert_eq!(io_why(&Error::from(ErrorKind::Other)), None);
    }

    /// A server on this machine that reads one request (or a TLS hello) and answers `reply` in
    /// plain text, then hangs up.
    async fn plain(reply: &'static str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        port
    }

    async fn failure(client: &reqwest::Client, url: &str) -> Why {
        why(&client.get(url).send().await.expect_err("the request fails"))
    }

    #[tokio::test]
    async fn a_proxy_that_wants_a_sign_in_or_refuses_is_named() {
        for (reply, expected) in [
            ("HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Negotiate\r\nContent-Length: 0\r\n\r\n", Why::ProxyAuth),
            ("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n", Why::ProxyRefused),
        ] {
            let port = plain(reply).await;
            let proxy = reqwest::Proxy::https(format!("http://127.0.0.1:{port}")).unwrap();
            // This proxy alone, whatever this computer's own settings name.
            let client = builder().no_proxy().proxy(proxy).build().unwrap();
            let failed = client.get("https://downloads.rclone.org/x").send().await.expect_err("the request fails");
            assert_eq!(why(&failed), expected, "{reply}");
            // The tunnel failed, so nothing reached the host: what monitoring retries as a
            // connection failure, never as an uncertain send.
            assert!(failed.is_connect(), "{reply}");
            assert_eq!(proxy_code(&failed), expected.proxy_code(), "{reply}");
        }
    }

    #[tokio::test]
    async fn an_answer_cut_short_is_told_from_a_refusal() {
        let client = builder().no_proxy().build().unwrap();
        // Promises 100 bytes, sends 2, and hangs up.
        let port = plain("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nPK").await;
        let mut response = client.get(format!("http://127.0.0.1:{port}/x")).send().await.unwrap();
        let mut failure = None;
        while failure.is_none() {
            match response.chunk().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("the whole body came"),
                Err(e) => failure = Some(e),
            }
        }
        assert!(cut_short(&failure.unwrap()));
        let port = plain("HTTP/1.1 403 Forbidden\r\n\r\n").await;
        let refused = client.get(format!("https://127.0.0.1:{port}/x")).send().await.unwrap_err();
        assert!(!cut_short(&refused), "a plain answer on the secure port is no cut-short body");
    }

    #[tokio::test]
    async fn a_plain_answer_on_the_secure_port_or_a_refusal_is_named() {
        let client = builder().no_proxy().build().unwrap();
        let port = plain("HTTP/1.1 403 Forbidden\r\nContent-Type: text/html\r\n\r\n<html>Blocked</html>").await;
        assert_eq!(failure(&client, &format!("https://127.0.0.1:{port}/x")).await, Why::NotSecure);
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert_eq!(failure(&client, &format!("https://127.0.0.1:{closed}/x")).await, Why::Refused);
    }

    /// The CI step's proof (`.github/workflows/tests.yml`): SlopBar's own clients decide what to
    /// trust as Windows does. The step makes a throwaway authority (`test_tls`), and then, on a
    /// disposable runner only, runs this with SLOPBAR_TEST_OS_STORE_EXPECT saying what Windows'
    /// store holds: `untrusted` (the authority's root in no store), `trusted` (the root in the
    /// machine's Root store), `distrusted` (the root in its Disallowed store too). Never run on a
    /// computer people use.
    #[tokio::test]
    #[ignore]
    async fn slopbar_trusts_what_windows_trusts() {
        use crate::test_tls::{os_store_folder, Answer, Server};
        let folder = os_store_folder().expect("SLOPBAR_TEST_OS_STORE names the folder");
        let expect = std::env::var("SLOPBAR_TEST_OS_STORE_EXPECT").expect("SLOPBAR_TEST_OS_STORE_EXPECT says what the store holds");
        let read = |name: &str| std::fs::read(folder.join(name)).unwrap();
        let server = Server::start(read("server.cer").into(), read("server.key").into(),
            |_, _| Answer::Body { status: 200, content_type: "text/plain", body: b"ok".to_vec() });
        let topic = reqwest::Url::parse(&server.url("/")).unwrap();
        #[allow(unused_mut)] // Only the desktop build adds the usage client.
        let mut clients = vec![("download", crate::sharing_setup::download_client()), ("phone", crate::phone::client_for(&topic))];
        #[cfg(feature = "desktop")]
        clients.push(("usage", crate::app::http_client()));
        for (name, client) in clients {
            let got = client.get(server.url("/")).send().await;
            match expect.as_str() {
                "trusted" => {
                    let response = got.unwrap_or_else(|e| panic!("{name}: {:?}", why(&e)));
                    assert_eq!(response.status(), 200, "{name}");
                    assert_eq!(response.text().await.unwrap(), "ok", "{name}");
                }
                "untrusted" => { let e = got.expect_err(name); assert_eq!(why(&e), Why::Untrusted, "{name}: {}", detail(&e)) }
                "distrusted" => { let e = got.expect_err(name); assert_eq!(why(&e), Why::Distrusted, "{name}: {}", detail(&e)) }
                other => panic!("SLOPBAR_TEST_OS_STORE_EXPECT={other}"),
            }
        }
    }

    /// The TLS error under `error`, for a test's failure message: rustls's own, never a link.
    fn detail(error: &reqwest::Error) -> String {
        fn find(e: &(dyn std::error::Error + 'static)) -> Option<String> {
            if let Some(tls) = e.downcast_ref::<rustls::Error>() {
                return Some(format!("{tls:?}"));
            }
            e.downcast_ref::<std::io::Error>().and_then(|io| io.get_ref()).and_then(|inner| find(inner))
        }
        let mut cause = std::error::Error::source(error);
        while let Some(e) = cause {
            if let Some(found) = find(e) {
                return found;
            }
            cause = e.source();
        }
        "no TLS error".into()
    }

    /// reqwest 0.13 panics building a client before any TLS cryptography is installed, so every
    /// client is built from `builder`, which installs it and Windows' proxy rules: tests included.
    #[test]
    fn every_client_is_built_here() {
        fn walk(dir: &std::path::Path, found: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap().map(Result::unwrap) {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, found);
                } else if path.extension().is_some_and(|e| e == "rs") && path.file_name().is_some_and(|n| n != "net.rs") {
                    let code = crate::source_guard::tokens(&std::fs::read_to_string(&path).unwrap());
                    // Nor may a client replace the proxy rules or the resolver that refuses a proxy
                    // SlopBar cannot use (`windows_proxy`).
                    for needle in ["Client::new(", "Client::builder(", "ClientBuilder::new(", "Client::default(", "ClientBuilder::default(",
                        "reqwest::get(", ".build().unwrap_or_default()", "reqwest::Client as", "ClientBuilder as", "Client as Default",
                        ".dns_resolver(", "Proxy::custom(", "Proxy::all(", ".danger_accept_invalid"] {
                        if code.contains(needle) {
                            found.push(format!("{}: {needle}", path.display()));
                        }
                    }
                }
            }
        }
        let mut found = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut found);
        assert!(found.is_empty(), "build clients with net::builder(): {found:#?}");
    }

    /// Outside its tests, nothing sets a proxy of its own, looks names up its own way, goes
    /// direct, or routes by routes of its own (only proxy.rs makes a `Router`: this user's): each
    /// would be a way around the proxy Windows names. The exceptions: phone.rs reaches a server on
    /// this computer or its local network directly, on purpose; test_support.rs, which only tests
    /// compile, makes tests' own routes.
    #[test]
    fn nothing_goes_around_the_proxy() {
        fn walk(dir: &std::path::Path, found: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap().map(Result::unwrap) {
                let path = entry.path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if path.is_dir() {
                    walk(&path, found);
                } else if name.ends_with(".rs") && !matches!(name.as_str(), "net.rs" | "tests.rs") {
                    let code = crate::source_guard::tokens(&std::fs::read_to_string(&path).unwrap());
                    // Only the code before the file's own tests.
                    let code = code.split("#[cfg(test)]mod tests").next().unwrap_or_default();
                    for needle in [".no_proxy(", ".proxy(", ".resolve(", ".resolve_to_addrs(", "with_router(", "Router::with_env(", "Router::new("] {
                        let allowed = (needle == ".no_proxy(" && name == "phone.rs")
                            || (needle.starts_with("Router::") && name == "proxy.rs")
                            || (needle == "Router::with_env(" && name == "test_support.rs");
                        if code.contains(needle) && !allowed {
                            found.push(format!("{}: {needle}", path.display()));
                        }
                    }
                }
            }
        }
        let mut found = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut found);
        assert!(found.is_empty(), "{found:#?}");
        // test_support.rs's own routes reach no build people run.
        let lib = crate::source_guard::tokens(include_str!("lib.rs"));
        assert!(lib.contains("#[cfg(test)]mod test_support;"), "test_support is compiled for tests only");
    }

    #[test]
    fn a_shown_host_never_carries_a_path_or_query() {
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        assert_eq!(shown_host(&url("https://release-assets.githubusercontent.com/a/b?sig=SYNTHETIC&jwt=x")), "release-assets.githubusercontent.com");
        assert_eq!(shown_host(&url("http://block.example.invalid/page?user=someone")), "http://block.example.invalid");
        assert_eq!(shown_host(&url("ftp://files.example.invalid/x")), "ftp:");
        assert_eq!(shown_host(&url("https://user:secret@proxy.example.invalid/")), "proxy.example.invalid");
    }
}
