use super::*;
use crate::net::Why;
use std::io::{Read, Write};

/// A Windows answering from fixed values, noting each question and its deadline.
struct Fake {
    settings: Result<Option<Settings>, Refusal>,
    automatic: Result<Vec<Entry>, Failure>,
    manual: Result<Vec<Entry>, Failure>,
    /// Whether the WinHTTP auto-proxy service is disabled.
    off: bool,
    /// How long each automatic question takes.
    slow: Duration,
    asked: Mutex<Vec<String>>,
    deadlines: Mutex<Vec<Instant>>,
}

impl Windows for Fake {
    fn settings(&self) -> Result<Option<Settings>, Refusal> {
        self.settings.clone()
    }

    fn automatic(&self, url: &str, detect: bool, script: Option<&str>, deadline: Instant) -> Result<Vec<Entry>, Failure> {
        std::thread::sleep(self.slow);
        self.asked.lock().unwrap().push(format!("automatic {url} detect={detect} script={}", script.unwrap_or("-")));
        self.deadlines.lock().unwrap().push(deadline);
        self.automatic.clone()
    }

    fn manual(&self, url: &str, deadline: Instant) -> Result<Vec<Entry>, Failure> {
        self.asked.lock().unwrap().push(format!("manual {url}"));
        self.deadlines.lock().unwrap().push(deadline);
        self.manual.clone()
    }

    fn detection_off(&self) -> bool {
        self.off
    }
}

fn fake(settings: Option<Settings>, automatic: Result<Vec<Entry>, Failure>, manual: Result<Vec<Entry>, Failure>) -> Fake {
    Fake { settings: Ok(settings), automatic, manual, off: false, slow: Duration::ZERO, asked: Mutex::new(Vec::new()), deadlines: Mutex::new(Vec::new()) }
}

fn set(detect: bool, script: Option<&str>, manual: bool) -> Option<Settings> {
    Some(Settings { detect, script: script.map(String::from), manual })
}

fn proxy(scheme: i32, host: &str, port: u16) -> Entry {
    Entry { proxy: true, scheme, host: host.into(), port }
}

fn direct() -> Entry {
    Entry { proxy: false, scheme: 0, host: String::new(), port: 0 }
}

fn through(link: &str) -> Route {
    Route::Through(Url::parse(link).unwrap())
}

fn url(link: &str) -> Url {
    Url::parse(link).unwrap()
}

fn env_of(vars: &[(&str, &str)]) -> Box<Env> {
    let vars: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    Box::new(move |name| vars.get(name).cloned())
}

const SCRIPT_SET: &str = "http://wpad.example.test/proxy.pac";

#[test]
fn windows_first_choice_is_used_or_refused_never_skipped() {
    assert_eq!(first_choice(&[]), Route::Refused(Refusal::Unsupported), "an answer WinHTTP does not understand comes back empty");
    assert_eq!(first_choice(&[direct()]), Route::Direct);
    assert_eq!(first_choice(&[proxy(1, "proxy.corp.example", 8080)]), through("http://proxy.corp.example:8080"));
    assert_eq!(first_choice(&[proxy(2, "secure.corp.example", 443)]), through("https://secure.corp.example:443"));
    assert_eq!(first_choice(&[proxy(1, "[2001:db8::1]", 3128)]), through("http://[2001:db8::1]:3128"));
    assert_eq!(first_choice(&[proxy(1, "10.0.0.8", 80)]), through("http://10.0.0.8:80"));
    assert_eq!(first_choice(&[proxy(1, "p.example", 8080), direct()]), through("http://p.example:8080"));
    assert_eq!(first_choice(&[direct(), proxy(1, "p.example", 8080)]), Route::Direct);
    // SOCKS first: refused, and never the entry after it, direct least of all.
    for entries in [vec![proxy(4, "s.example", 1080)], vec![proxy(4, "s.example", 1080), proxy(1, "p.example", 8080)], vec![proxy(4, "s.example", 1080), direct()]] {
        assert_eq!(first_choice(&entries), Route::Refused(Refusal::Unsupported), "{entries:?}");
    }
    for host in ["", "user@proxy.example", "proxy.example/path", "proxy.example:1", "a b", "[not-v6]", "proxy.example?x", "proxy.example#x"] {
        assert_eq!(first_choice(&[proxy(1, host, 8080)]), Route::Refused(Refusal::Unsupported), "{host:?}");
    }
    assert_eq!(first_choice(&[proxy(1, "proxy.example", 0)]), Route::Refused(Refusal::Unsupported), "no port");
    assert_eq!(first_choice(&[proxy(3, "ftp.example", 21)]), Route::Refused(Refusal::Unsupported), "an FTP proxy");
}

#[test]
fn windows_order_is_followed_and_a_failed_script_never_becomes_direct() {
    let link = "https://api.example.test/";
    let at = |port| Ok(vec![proxy(1, "proxy.example.test", port)]);
    let decided = |windows: &Fake| decide(windows, link).0;

    // No settings at all, or none set: direct, and nothing asked.
    let none = fake(None, at(1), at(1));
    assert_eq!(decide(&none, link), (Route::Direct, Decided::Nothing));
    let unset = fake(set(false, None, false), at(1), at(1));
    assert_eq!(decided(&unset), Route::Direct);
    assert!(none.asked.lock().unwrap().is_empty() && unset.asked.lock().unwrap().is_empty());
    // Settings that cannot be read could name a proxy: refused.
    let unreadable = Fake { settings: Err(Refusal::Unsupported), ..fake(None, at(1), at(1)) };
    assert_eq!(decided(&unreadable), Route::Refused(Refusal::Unsupported));

    // Detection on (Windows' default) and nothing found: direct, or the manual proxy when one is
    // set, as Windows does. The same when detection cannot run (the auto-proxy service disabled).
    assert_eq!(decide(&fake(set(true, None, false), Err(Failure::Os(12180)), at(1)), link), (Route::Direct, Decided::Nothing));
    assert_eq!(decide(&fake(set(true, None, true), Err(Failure::Os(12180)), at(2)), link), (through("http://proxy.example.test:2"), Decided::Manual));
    let off = Fake { off: true, ..fake(set(true, None, false), Err(Failure::Os(12178)), at(1)) };
    assert_eq!(decided(&off), Route::Direct);
    // Anything else from detection is refused (unless a manual proxy is set, Windows' own fallback:
    // still a proxy): a script found that failed, no answer in time, the service failing while it
    // is on, an error of WinHTTP's own.
    for failed in [Failure::Os(12167), Failure::Os(12166), Failure::Os(12177), Failure::Os(12015), Failure::TimedOut,
        Failure::Os(12178), Failure::Os(12004), Failure::Os(12005), Failure::Os(0)] {
        assert_eq!(decide(&fake(set(true, None, false), Err(failed), at(1)), link), (Route::Refused(Refusal::Script), Decided::Script), "{failed:?}");
        assert_eq!(decided(&fake(set(true, None, true), Err(failed), at(3))), through("http://proxy.example.test:3"), "{failed:?}");
    }
    // A script set: its answer, the manual proxy not asked.
    let scripted = fake(set(true, Some(SCRIPT_SET), true), at(4), at(5));
    assert_eq!(decide(&scripted, link), (through("http://proxy.example.test:4"), Decided::Script));
    assert_eq!(*scripted.asked.lock().unwrap(), [format!("automatic {link} detect=true script={SCRIPT_SET}")]);
    // A script set that fails in any way is refused, never direct, the service disabled or not.
    for failed in [Failure::Os(12167), Failure::Os(12180), Failure::Os(12178), Failure::Os(5), Failure::TimedOut] {
        let alone = Fake { off: true, ..fake(set(false, Some(SCRIPT_SET), false), Err(failed), at(1)) };
        assert_eq!(decided(&alone), Route::Refused(Refusal::Script), "{failed:?}");
        assert_eq!(decided(&fake(set(true, Some(SCRIPT_SET), true), Err(failed), at(6))), through("http://proxy.example.test:6"), "{failed:?}");
    }

    // The manual proxy alone: Windows' answer for it (per-protocol servers, bypass list), or refused.
    let manual = fake(set(false, None, true), at(1), at(7));
    assert_eq!(decide(&manual, link), (through("http://proxy.example.test:7"), Decided::Manual));
    assert_eq!(*manual.asked.lock().unwrap(), [format!("manual {link}")], "no automatic question when none is set");
    assert_eq!(decided(&fake(set(false, None, true), at(1), Ok(vec![direct()]))), Route::Direct, "bypassed");
    assert_eq!(decided(&fake(set(false, None, true), at(1), Ok(vec![proxy(4, "s.example", 1080)]))), Route::Refused(Refusal::Unsupported), "socks=");
    assert_eq!(decided(&fake(set(false, None, true), at(1), Err(Failure::Os(12004)))), Route::Refused(Refusal::Unsupported));

    // Every question for one link shares one deadline, LIMIT away at most.
    let both = fake(set(true, None, true), Err(Failure::Os(12180)), at(8));
    let asked = Instant::now();
    decide(&both, link);
    let deadlines = both.deadlines.lock().unwrap().clone();
    assert_eq!(deadlines.len(), 2);
    assert!(deadlines[0] == deadlines[1] && deadlines[0] <= Instant::now() + LIMIT && deadlines[0] >= asked + LIMIT, "{deadlines:?}");
}

#[test]
fn the_sign_in_goes_only_to_a_script_set_in_settings() {
    assert!(sign_in_allowed(false, Some(SCRIPT_SET)));
    // Any network can name the script detection finds: never this user's sign-in for it.
    assert!(!sign_in_allowed(true, Some(SCRIPT_SET)));
    assert!(!sign_in_allowed(true, None));
    assert!(!sign_in_allowed(false, None));
}

#[test]
fn a_link_is_asked_about_once_at_a_time() {
    let windows = Arc::new(Fake { slow: Duration::from_millis(200), ..fake(set(true, None, false), Ok(vec![proxy(1, "proxy.example.test", 1)]), Ok(vec![])) });
    let router = Arc::new(Router::with_env(windows.clone(), env_of(&[])));
    let askers: Vec<_> = (0..4).map(|_| {
        let router = router.clone();
        std::thread::spawn(move || router.route(&url("https://same.example.test/")))
    }).collect();
    for asker in askers {
        assert_eq!(asker.join().unwrap(), through("http://proxy.example.test:1"));
    }
    assert_eq!(windows.asked.lock().unwrap().len(), 1, "the others waited for the first answer");
}

#[test]
fn every_proxy_handed_to_reqwest_is_one_it_takes() {
    // reqwest goes direct for a proxy hyper-util cannot read, so none may get through.
    for host in [".", "..", "-", "_", "1", "a-b.c_d", &"a".repeat(253), "[::ffff:10.0.0.1]", "[2001:db8::1]"] {
        if let Route::Through(proxy) = first_choice(&[proxy(1, host, 8080)]) {
            assert!(usable(&proxy), "{proxy}");
        }
    }
    for value in ["http://pro{xy:8080", "http://exa mple:80", "http://p:99999", "socks9://p:1", "http://2001:db8::1:8080"] {
        assert_eq!(env_proxy(value), None, "{value}");
    }
    assert!(env_proxy("http://someone:secret@proxy.example.test:8080").is_some_and(|p| usable(&p)));
}

#[test]
fn the_environment_comes_first_with_no_proxy() {
    let windows = Arc::new(fake(set(false, None, true), Err(Failure::Os(12180)), Ok(vec![proxy(1, "proxy.example.test", 9)])));
    let router = Router::with_env(windows.clone(), env_of(&[("HTTPS_PROXY", "http://env.example.test:3128"), ("NO_PROXY", "internal.example.test, 10.0.0.0/8")]));
    assert_eq!(router.route(&url("https://api.example.test/")), through("http://env.example.test:3128"));
    assert_eq!(router.route(&url("https://wiki.internal.example.test/")), Route::Direct);
    assert_eq!(router.route(&url("https://10.1.2.3/")), Route::Direct);
    // HTTPS_PROXY says nothing for plain http: Windows decides that.
    assert_eq!(router.route(&url("http://api.example.test/")), through("http://proxy.example.test:9"));
    assert_eq!(windows.asked.lock().unwrap().len(), 1);

    // ALL_PROXY for both, lower case read too; a value without a scheme is http; a sign-in stays.
    let router = Router::with_env(windows.clone(), env_of(&[("all_proxy", "env.example.test:8080")]));
    assert_eq!(router.route(&url("http://a.example.test/")), through("http://env.example.test:8080"));
    let router = Router::with_env(windows.clone(), env_of(&[("https_proxy", "http://someone:secret@env.example.test:1")]));
    assert_eq!(router.route(&url("https://a.example.test/")), through("http://someone:secret@env.example.test:1"));
    // Upper case first, and an empty variable is unset.
    let router = Router::with_env(windows.clone(), env_of(&[("HTTPS_PROXY", " "), ("https_proxy", "http://lower.example.test:2")]));
    assert_eq!(router.route(&url("https://a.example.test/")), through("http://lower.example.test:2"));
    // A value that is no http(s) proxy is refused, not ignored, and the reason names the
    // environment, not Windows.
    for value in ["socks5://s.example.test:1080", "ftp://f.example.test", "http://"] {
        let router = Router::with_env(windows.clone(), env_of(&[("HTTPS_PROXY", value)]));
        assert_eq!(router.decision(&url("https://a.example.test/")), (Route::Refused(Refusal::Environment), Decided::Environment), "{value}");
        let router = Router::with_env(windows.clone(), env_of(&[("ALL_PROXY", value)]));
        assert_eq!(router.route(&url("https://a.example.test/")), Route::Refused(Refusal::Environment), "{value}");
    }
}

#[test]
fn this_computer_is_never_behind_a_proxy() {
    let windows = Arc::new(fake(set(false, None, true), Err(Failure::Os(12180)), Ok(vec![proxy(1, "proxy.example.test", 9)])));
    let router = Router::with_env(windows.clone(), env_of(&[("HTTPS_PROXY", "http://env.example.test:1"), ("HTTP_PROXY", "http://env.example.test:1")]));
    for link in ["https://127.0.0.1:8443/", "http://localhost/", "https://LOCALHOST:1/", "https://[::1]/", "http://127.9.9.9/x"] {
        assert_eq!(router.route(&url(link)), Route::Direct, "{link}");
    }
    assert!(windows.asked.lock().unwrap().is_empty());
}

#[test]
fn routes_are_kept_a_while_per_scheme_host_and_port() {
    let windows = Arc::new(fake(set(true, None, false), Ok(vec![proxy(1, "proxy.example.test", 1)]), Ok(vec![])));
    let router = Router::with_env(windows.clone(), env_of(&[]));
    for link in ["https://a.example.test/x?secret=1", "https://a.example.test/other", "http://a.example.test/", "https://a.example.test:8443/", "https://a.example.test:443/y"] {
        assert_eq!(router.route(&url(link)), through("http://proxy.example.test:1"), "{link}");
    }
    // Windows sees scheme, host and a port that is not the scheme's own: never a path or query.
    assert_eq!(*windows.asked.lock().unwrap(), [
        "automatic https://a.example.test/ detect=true script=-",
        "automatic http://a.example.test/ detect=true script=-",
        "automatic https://a.example.test:8443/ detect=true script=-",
    ]);
}

/// A server on this computer that counts the connections it gets, for a name a client resolves to
/// it: a request that went direct shows here.
struct Target {
    addr: std::net::SocketAddr,
    connections: Arc<std::sync::atomic::AtomicUsize>,
}

impl Target {
    fn start() -> Target {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = connections.clone();
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(socket);
            }
        });
        Target { addr, connections }
    }

    fn count(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// SlopBar's client with these routes, `api.example.test` resolving to `target`.
fn client(router: Arc<Router>, target: &Target) -> reqwest::Client {
    crate::net::with_router(crate::net::builder().no_proxy(), router)
        .resolve("api.example.test", target.addr)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

#[tokio::test]
async fn a_refused_proxy_fails_the_request_with_its_reason_and_nothing_goes_direct() {
    let target = Target::start();
    for (answer, why) in [
        (Ok(vec![proxy(4, "socks.example.test", 1080)]), Why::ProxyUnsupported),
        (Ok(vec![]), Why::ProxyUnsupported),
        (Err(Failure::Os(12167)), Why::ProxyScript),
        (Err(Failure::TimedOut), Why::ProxyScript),
    ] {
        let windows = Arc::new(fake(set(false, Some(SCRIPT_SET), false), answer.clone(), Ok(vec![])));
        let failed = client(Arc::new(Router::with_env(windows, env_of(&[]))), &target).get("https://api.example.test/x").send().await.unwrap_err();
        assert_eq!(crate::net::why(&failed), why, "{answer:?}");
        assert!(failed.is_connect());
        // The flyout's usage error is the reason's code, which it words.
        let code = if why == Why::ProxyUnsupported { "proxy_unsupported" } else { "proxy_script" };
        assert_eq!(crate::net::failure_text(&failed), code);
    }
    // An environment variable SlopBar cannot use, whatever Windows would say: its own reason.
    let windows = Arc::new(fake(set(false, None, true), Ok(vec![direct()]), Ok(vec![direct()])));
    let env = env_of(&[("HTTPS_PROXY", "socks5://socks.example.test:1080")]);
    let failed = client(Arc::new(Router::with_env(windows, env)), &target).get("https://api.example.test/x").send().await.unwrap_err();
    assert_eq!((crate::net::why(&failed), crate::net::failure_text(&failed)), (Why::ProxyEnvironment, "proxy_environment".to_string()));
    assert!(failed.is_connect(), "nothing was sent, so monitoring retries it as a connection failure");
    // Every refusal's stand-in, through the client: its own reason and code.
    for refusal in Refusal::ALL {
        let windows = Arc::new(Fake { settings: Err(refusal), ..fake(None, Ok(vec![]), Ok(vec![])) });
        let failed = client(Arc::new(Router::with_env(windows, env_of(&[]))), &target).get("https://api.example.test/x").send().await.unwrap_err();
        assert_eq!((crate::net::why(&failed), crate::net::proxy_code(&failed)), (refusal.why(), Some(refusal.code())), "{refusal:?}");
        assert!(failed.is_connect(), "{refusal:?}");
    }
    assert_eq!(target.count(), 0, "nothing went direct");

    // The control: a direct answer does reach the target.
    let windows = Arc::new(fake(set(false, Some(SCRIPT_SET), false), Ok(vec![direct()]), Ok(vec![])));
    let _ = client(Arc::new(Router::with_env(windows, env_of(&[]))), &target).get("https://api.example.test/x").send().await;
    assert_eq!(target.count(), 1);
}

/// Windows whose answer for every link changes when told to, as it does when a VPN comes up or a
/// setting changes: a proxy script is set, and answers what the test put here last.
struct Changing(Mutex<Result<Vec<Entry>, Failure>>);

impl Windows for Changing {
    fn settings(&self) -> Result<Option<Settings>, Refusal> {
        Ok(set(false, Some(SCRIPT_SET), false))
    }

    fn automatic(&self, _: &str, _: bool, _: Option<&str>, _: Instant) -> Result<Vec<Entry>, Failure> {
        self.0.lock().unwrap().clone()
    }

    fn manual(&self, _: &str, _: Instant) -> Result<Vec<Entry>, Failure> {
        unreachable!("no manual proxy is set")
    }

    fn detection_off(&self) -> bool {
        false
    }
}

/// An HTTPS server on this computer for `api.example.test` that answers every request on a
/// connection and keeps the connection open, as the providers' servers do, so that a client can
/// send its next request on it. Notes each request's path.
struct KeepsOpen {
    addr: std::net::SocketAddr,
    asked: Arc<Mutex<Vec<String>>>,
}

impl KeepsOpen {
    fn start(authority: &crate::test_tls::Authority) -> KeepsOpen {
        let (cert, key) = authority.issue_for("api.example.test");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions().unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], rustls::pki_types::PrivateKeyDer::Pkcs8(key)).unwrap();
        let config = Arc::new(config);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let noted = asked.clone();
        std::thread::spawn(move || {
            for socket in listener.incoming().map_while(Result::ok) {
                let (config, noted) = (config.clone(), noted.clone());
                std::thread::spawn(move || {
                    let _ = socket.set_read_timeout(Some(Duration::from_secs(10)));
                    let Ok(connection) = rustls::ServerConnection::new(config) else { return };
                    let mut stream = rustls::StreamOwned::new(connection, socket);
                    loop {
                        let mut head = Vec::new();
                        let mut byte = [0u8; 1];
                        while !head.ends_with(b"\r\n\r\n") {
                            match stream.read(&mut byte) {
                                Ok(1) if head.len() < 16 * 1024 => head.push(byte[0]),
                                _ => return,
                            }
                        }
                        let path = String::from_utf8_lossy(&head).split_whitespace().nth(1).unwrap_or_default().to_string();
                        noted.lock().unwrap().push(path);
                        if stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").and_then(|_| stream.flush()).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        KeepsOpen { addr, asked }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

/// `KeepsOpen` answers a second request on the same connection. If it did not, the test below would
/// pass on code that keeps connections for later requests too.
#[test]
fn the_target_keeps_its_connections_open() {
    let authority = crate::test_tls::Authority::new();
    let server = KeepsOpen::start(&authority);
    let mut roots = rustls::RootCertStore::empty();
    roots.add(authority.root.clone()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions().unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from("api.example.test").unwrap();
    let socket = std::net::TcpStream::connect(server.addr).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut stream = rustls::StreamOwned::new(rustls::ClientConnection::new(Arc::new(config), name).unwrap(), socket);
    for path in ["/one", "/two"] {
        stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: api.example.test\r\n\r\n").as_bytes()).unwrap();
        let mut answer = Vec::new();
        let mut buf = [0u8; 256];
        while !answer.ends_with(b"\r\n\r\nok") {
            let n = stream.read(&mut buf).unwrap();
            assert!(n > 0, "{path}: the connection was closed");
            answer.extend_from_slice(&buf[..n]);
        }
    }
    assert_eq!(server.asked(), ["/one", "/two"]);
}

/// A connection is never used past the route it was opened for: no request starts on a connection
/// opened for an earlier route. reqwest picks a connection's route only when it opens it, so one it
/// kept for the next request to the same host would go on going direct after Windows names a proxy
/// (a VPN coming up, a setting changed) or refuses, for as long as requests kept coming.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_is_never_used_past_its_route() {
    let fake_proxy = FakeProxy::start();
    let changed = tokio::join!(
        after_the_route_changes(Ok(vec![proxy(1, "127.0.0.1", fake_proxy.port)]), Why::ProxyRefused),
        after_the_route_changes(Err(Failure::Os(12167)), Why::ProxyScript),
    );
    assert_eq!(changed, (Ok(()), Ok(())));
    let lines = fake_proxy.lines();
    assert!(lines.contains(&"CONNECT api.example.test:443 HTTP/1.1".to_string()), "the proxy Windows names now was asked: {lines:?}");
}

/// Requests to one host on one client while Windows' answer changes from direct to `now`. The first
/// goes direct, and those sent while that route is kept may go direct too. They come under a second
/// apart, as a refresh's do, so a connection kept open never sits idle for long. The first request
/// after that route has expired must fail for `why`, and never reach the host.
async fn after_the_route_changes(now: Result<Vec<Entry>, Failure>, why: Why) -> Result<(), String> {
    let authority = crate::test_tls::Authority::new();
    let server = KeepsOpen::start(&authority);
    let windows = Arc::new(Changing(Mutex::new(Ok(vec![direct()]))));
    let router = Arc::new(Router::with_env(windows.clone(), env_of(&[])));
    let client = crate::net::with_router(crate::net::builder().no_proxy().tls_certs_merge([authority.reqwest_root()]), router)
        .resolve("api.example.test", server.addr)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let get = |path: &'static str| {
        let client = &client;
        // Read to the end, so that the connection is free for the next request.
        async move { client.get(format!("https://api.example.test{path}")).send().await?.text().await }
    };
    assert_eq!(get("/first").await.unwrap(), "ok", "{why:?}");
    *windows.0.lock().unwrap() = now;
    let expired = Instant::now() + KEPT + Duration::from_millis(200);
    while Instant::now() + Duration::from_secs(1) < expired {
        tokio::time::sleep(Duration::from_millis(800)).await;
        let _ = get("/meanwhile").await;
    }
    tokio::time::sleep(expired.saturating_duration_since(Instant::now())).await;
    match get("/after").await {
        Ok(_) => return Err(format!("{why:?}: the request went out on a connection opened for the route before")),
        Err(failed) if crate::net::why(&failed) != why => return Err(format!("{why:?}: failed for {:?}", crate::net::why(&failed))),
        Err(_) => {}
    }
    let asked = server.asked();
    if asked.first().is_none_or(|p| p != "/first") || asked.iter().any(|p| p == "/after") {
        return Err(format!("{why:?}: the host was asked {asked:?}"));
    }
    Ok(())
}

/// `Refusal::ALL` is every refusal: a refusal added to the enum does not compile here until it is
/// matched, and then this fails until it is in ALL too, where `refusal_named` finds it.
fn listed(refusal: Refusal) -> bool {
    match refusal {
        Refusal::Unsupported | Refusal::Script | Refusal::Environment | Refusal::Rclone => Refusal::ALL.contains(&refusal),
    }
}

#[test]
fn a_stand_in_names_only_its_refusal() {
    for refusal in [Refusal::Unsupported, Refusal::Script, Refusal::Environment, Refusal::Rclone] {
        assert!(listed(refusal), "{refusal:?} is missing from Refusal::ALL");
    }
    for refusal in Refusal::ALL {
        let name = stand_in(refusal).host_str().unwrap().to_string();
        assert!(name.ends_with(".invalid"), "{name}");
        assert_eq!(refusal_named(&name), Some(refusal));
    }
    for other in ["slopbar.invalid", "x.slopbar.invalid", "proxy-script.slopbar.invalid.example", "api.example.test"] {
        assert_eq!(refusal_named(other), None, "{other}");
    }
    // The resolver looks up no name under the stand-ins' domain: one that is no refusal's (a
    // refusal left out of ALL, say) is refused too.
    for refusal in Refusal::ALL {
        let name = stand_in(refusal).host_str().unwrap().to_ascii_uppercase();
        assert_eq!(stand_in_refusal(&name), Some(refusal), "{name}");
    }
    for unknown in ["x.slopbar.invalid", crate::rclone::ANY_HOST, "proxy-new.SLOPBAR.invalid"] {
        assert_eq!(stand_in_refusal(unknown), Some(Refusal::Unsupported), "{unknown}");
    }
    for other in ["slopbar.invalid", "proxy-script.slopbar.invalid.example", "api.example.test", "example.invalid"] {
        assert_eq!(stand_in_refusal(other), None, "{other}");
    }
}

/// Each refusal's code is the one its `Why` gives, and is a code everything that words the
/// network's proxy knows: diagnostics, Health, the native widget, i18n.js's `errorText` and its
/// English text, and the rclone download reasons Settings words (`sharingWhy_…`).
#[test]
fn every_proxy_code_is_known_wherever_one_is_worded() {
    let i18n = include_str!("../../../src/i18n.js");
    // The list errorText and accountErrorText use.
    let listed = i18n.split("const PROXY_CODES = [").nth(1).and_then(|l| l.split("];").next()).expect("i18n.js's PROXY_CODES");
    assert!(i18n.contains("if (PROXY_CODES.includes(code)) return t(code);"), "errorText words them");
    let mut codes: Vec<&str> = Refusal::ALL.iter().map(|r| r.code()).collect();
    codes.extend([Why::ProxyAuth, Why::ProxyRefused].map(|w| w.proxy_code().unwrap()));
    for refusal in Refusal::ALL {
        assert_eq!(refusal.why().proxy_code(), Some(refusal.code()), "{refusal:?}");
    }
    assert_eq!(codes.len(), crate::net::PROXY_CODES.len(), "every code SlopBar gives, and no other: {codes:?}");
    for code in codes {
        assert!(crate::net::PROXY_CODES.contains(&code), "{code}");
        assert_eq!(crate::diagnostics::Code::from_error(Some(code)), crate::diagnostics::Code::Proxy, "{code}");
        let english = crate::i18n::t("en", code, &[]);
        assert!(english != code && english.contains("proxy"), "{code}: {english}");
        assert!(listed.contains(&format!("\"{code}\"")), "errorText (i18n.js) words {code}");
        assert_eq!(crate::widget::error_text(&crate::widget::Lang("en".into(), None), "", code), english, "the widget words {code}");
        // rclone's own refusal is never a download's.
        if code != "proxy_rclone" {
            assert!(crate::i18n::t("en", &format!("sharingWhy_{code}"), &[("host", "x")]).starts_with("x: "), "sharingWhy_{code}");
        }
    }
}

#[test]
fn rclone_gets_the_proxy_windows_names_for_its_cloud_or_does_not_run() {
    let at = |host: &str, port| Ok(vec![proxy(1, host, port)]);
    let router = |automatic| Router::with_env(Arc::new(fake(set(true, None, false), automatic, Ok(vec![]))), env_of(&[]));
    let env = rclone_env(&router(at("proxy.example.test", 8080)), &["api.dropboxapi.com", "content.dropboxapi.com"], true).unwrap();
    assert_eq!(env, [
        ("HTTPS_PROXY", Some("http://proxy.example.test:8080".to_string())),
        ("HTTP_PROXY", Some("http://proxy.example.test:8080".to_string())),
        // Nothing inherited overrides it.
        ("NO_PROXY", None),
        ("RCLONE_HTTP_PROXY", None),
    ]);
    // Direct: the variables are removed, whatever SlopBar's own environment had.
    let env = rclone_env(&router(Ok(vec![direct()])), &["api.dropboxapi.com"], true).unwrap();
    assert_eq!(env, [("HTTPS_PROXY", None), ("HTTP_PROXY", None), ("NO_PROXY", None), ("RCLONE_HTTP_PROXY", None)]);

    // On a real command: the variables rclone starts with, whatever this process's own are (on
    // Windows, where names are not case-sensitive).
    #[cfg(windows)]
    {
        let mut command = std::process::Command::new("rclone");
        command.env("https_proxy", "http://inherited.example.test:1").env("NO_PROXY", ".dropboxapi.com");
        crate::rclone::apply_env(&mut command, rclone_env(&router(at("proxy.example.test", 8080)), &["api.dropboxapi.com"], true).unwrap());
        let set: HashMap<String, Option<String>> = command.get_envs()
            .map(|(k, v)| (k.to_string_lossy().to_ascii_uppercase(), v.map(|v| v.to_string_lossy().into_owned()))).collect();
        assert_eq!(set.get("HTTPS_PROXY"), Some(&Some("http://proxy.example.test:8080".to_string())), "{set:?}");
        assert_eq!(set.get("NO_PROXY"), Some(&None), "{set:?}");
    }
    // A proxy SlopBar cannot use, or a script that failed: rclone does not run.
    assert_eq!(rclone_env(&router(at("s.example.test", 0)), &["api.dropboxapi.com"], true), Err(Refusal::Unsupported));
    assert_eq!(rclone_env(&router(Ok(vec![proxy(4, "s.example.test", 1080)])), &["api.dropboxapi.com"], true), Err(Refusal::Unsupported));
    assert_eq!(rclone_env(&router(Err(Failure::Os(12167))), &["api.dropboxapi.com"], true), Err(Refusal::Script));

    // A script that answers each host its own way.
    struct ByHost(Vec<(&'static str, Vec<Entry>)>);
    impl Windows for ByHost {
        fn settings(&self) -> Result<Option<Settings>, Refusal> {
            Ok(set(false, Some(SCRIPT_SET), false))
        }
        fn automatic(&self, url: &str, _: bool, _: Option<&str>, _: Instant) -> Result<Vec<Entry>, Failure> {
            Ok(self.0.iter().find(|(part, _)| url.contains(part)).map(|(_, entries)| entries.clone()).unwrap_or_else(|| vec![direct()]))
        }
        fn manual(&self, _: &str, _: Instant) -> Result<Vec<Entry>, Failure> {
            Ok(vec![])
        }
        fn detection_off(&self) -> bool {
            false
        }
    }
    let by_host = |answers: Vec<(&'static str, Vec<Entry>)>| Router::with_env(Arc::new(ByHost(answers)), env_of(&[]));
    // One proxy for some hosts and direct for others: rclone takes that one proxy for all of them,
    // through a proxy and never around one.
    let some = by_host(vec![("//api.", vec![proxy(1, "proxy.example.test", 8080)]), ("//content.", vec![direct()])]);
    assert_eq!(rclone_env(&some, &["api.dropboxapi.com", "content.dropboxapi.com"], true).unwrap()[0], ("HTTPS_PROXY", Some("http://proxy.example.test:8080".to_string())));
    // Two different proxies: rclone takes only one, so it does not run, and says that is why.
    let two = by_host(vec![("//api.", vec![proxy(1, "a.example.test", 8080)]), ("//content.", vec![proxy(1, "b.example.test", 8080)])]);
    assert_eq!(rclone_env(&two, &["api.dropboxapi.com", "content.dropboxapi.com"], true), Err(Refusal::Rclone));

    // A cloud SlopBar does not know: the remote's own host is not known here, so where a script
    // decides (which may answer it its own way) rclone does not run; where the manual proxy or the
    // environment decides, it gets the proxy they give any host they do not bypass.
    let unknown = crate::rclone::cloud("s3");
    assert_eq!(unknown, crate::rclone::Cloud { hosts: &[crate::rclone::ANY_HOST], known: false });
    assert!(crate::rclone::cloud("dropbox").known && crate::rclone::cloud("crypt").hosts == unknown.hosts);
    assert_eq!(rclone_env(&router(at("proxy.example.test", 8080)), unknown.hosts, false), Err(Refusal::Rclone));
    assert_eq!(rclone_env(&router(Ok(vec![direct()])), unknown.hosts, false), Err(Refusal::Rclone), "even where the script says direct");
    // A script that failed is the reason, whatever the cloud.
    assert_eq!(rclone_env(&router(Err(Failure::Os(12167))), unknown.hosts, false), Err(Refusal::Script));
    let from_env = Router::with_env(Arc::new(fake(set(true, None, false), at("proxy.example.test", 1), Ok(vec![]))), env_of(&[("ALL_PROXY", "http://env.example.test:2"), ("NO_PROXY", ".dropboxapi.com,.googleapis.com")]));
    assert_eq!(rclone_env(&from_env, unknown.hosts, false).unwrap()[0], ("HTTPS_PROXY", Some("http://env.example.test:2".to_string())));
    // A manual proxy whose bypass list covers the well-known clouds still names its proxy for the
    // remote: never direct because those happen to be bypassed.
    struct Bypassing;
    impl Windows for Bypassing {
        fn settings(&self) -> Result<Option<Settings>, Refusal> {
            Ok(set(true, None, true))
        }
        fn automatic(&self, _: &str, _: bool, _: Option<&str>, _: Instant) -> Result<Vec<Entry>, Failure> {
            Err(Failure::Os(12180))
        }
        fn manual(&self, url: &str, _: Instant) -> Result<Vec<Entry>, Failure> {
            let bypassed = ["dropboxapi.com", "googleapis.com", "microsoft.com", "box.com"].iter().any(|d| url.contains(d));
            Ok(if bypassed { vec![direct()] } else { vec![proxy(1, "proxy.example.test", 3128)] })
        }
        fn detection_off(&self) -> bool {
            false
        }
    }
    let bypassing = Router::with_env(Arc::new(Bypassing), env_of(&[]));
    assert_eq!(rclone_env(&bypassing, unknown.hosts, false).unwrap()[0], ("HTTPS_PROXY", Some("http://proxy.example.test:3128".to_string())));
    assert_eq!(rclone_env(&bypassing, crate::rclone::cloud("dropbox").hosts, true).unwrap()[0], ("HTTPS_PROXY", None), "a known cloud bypassed: direct, as Windows");
}

/// What a child process actually starts with: Windows' variable names are not case-sensitive, and
/// an inherited lower-case variable, NO_PROXY or RCLONE_HTTP_PROXY must not survive.
#[cfg(windows)]
#[test]
fn rclone_starts_with_the_proxy_whatever_this_process_has() {
    let router = Router::with_env(Arc::new(fake(set(true, None, false), Ok(vec![proxy(1, "proxy.example.test", 8080)]), Ok(vec![]))), env_of(&[]));
    let mut command = crate::sources::command("cmd");
    command.args(["/d", "/c", "set"]).env("https_proxy", "http://inherited.example.test:1").env("no_proxy", ".dropboxapi.com")
        .env("RCLONE_HTTP_PROXY", "http://inherited.example.test:2");
    crate::rclone::apply_env(&mut command, rclone_env(&router, &["api.dropboxapi.com"], true).unwrap());
    let out = String::from_utf8_lossy(&command.output().unwrap().stdout).to_string();
    let seen = |name: &str| out.lines().find_map(|line| line.split_once('=').filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.to_string()));
    assert_eq!(seen("HTTPS_PROXY").as_deref(), Some("http://proxy.example.test:8080"));
    assert_eq!(seen("HTTP_PROXY").as_deref(), Some("http://proxy.example.test:8080"));
    assert_eq!(seen("NO_PROXY"), None, "no inherited exception");
    assert_eq!(seen("RCLONE_HTTP_PROXY"), None, "nor rclone's own setting");
}

// Against WinHTTP itself: a proxy script served on this computer, given to WinHTTP directly (the
// user's own settings are never read or changed), and a proxy on this computer that notes what it
// is asked and refuses it.

/// Serves `script` as a proxy script on this computer; its address.
fn serve_script(script: String) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut socket in listener.incoming().map_while(Result::ok) {
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf);
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/x-ns-proxy-autoconfig\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", script.len());
            let _ = socket.write_all(head.as_bytes());
            let _ = socket.write_all(script.as_bytes());
        }
    });
    // A fresh path each time: WinHTTP's service keeps scripts by address.
    format!("http://127.0.0.1:{port}/{}.pac", uuid::Uuid::new_v4())
}

/// A proxy on this computer: notes each request's first line and answers 403.
struct FakeProxy {
    port: u16,
    lines: Arc<Mutex<Vec<String>>>,
}

impl FakeProxy {
    fn start() -> FakeProxy {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let noted = lines.clone();
        std::thread::spawn(move || {
            for mut socket in listener.incoming().map_while(Result::ok) {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 && socket.read(&mut byte).is_ok_and(|n| n == 1) {
                    head.push(byte[0]);
                }
                let first = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().to_string();
                noted.lock().unwrap().push(first);
                let _ = socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        });
        FakeProxy { port, lines }
    }

    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}

/// Windows with this proxy script set and nothing else: WinHTTP fetches and runs it.
#[cfg(windows)]
struct Scripted(String);

#[cfg(windows)]
impl Windows for Scripted {
    fn settings(&self) -> Result<Option<Settings>, Refusal> {
        Ok(set(false, Some(&self.0), false))
    }

    fn automatic(&self, url: &str, detect: bool, script: Option<&str>, deadline: Instant) -> Result<Vec<Entry>, Failure> {
        super::win::automatic(url, detect, script, deadline)
    }

    fn manual(&self, url: &str, deadline: Instant) -> Result<Vec<Entry>, Failure> {
        super::win::manual(url, deadline)
    }

    fn detection_off(&self) -> bool {
        false
    }
}

#[cfg(windows)]
fn scripted(script: String) -> Arc<Router> {
    Arc::new(Router::with_env(Arc::new(Scripted(script)), env_of(&[])))
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_go_through_the_proxy_a_proxy_script_names() {
    let target = Target::start();
    let proxy = FakeProxy::start();
    let script = serve_script(format!(
        "function FindProxyForURL(url, host) {{ if (isPlainHostName(host) || dnsDomainIs(host, '.direct.test')) return 'DIRECT'; return 'PROXY 127.0.0.1:{}'; }}",
        proxy.port
    ));
    let router = scripted(script);
    let client = client(router.clone(), &target);

    // HTTPS: a tunnel through the script's proxy, which refuses it here.
    let failed = client.get("https://api.example.test/usage?secret=1").send().await.unwrap_err();
    assert_eq!(crate::net::why(&failed), Why::ProxyRefused, "the proxy refused the tunnel");
    // Plain http: the whole link, to the proxy.
    let answer = client.get("http://api.example.test/plain").send().await.unwrap();
    assert_eq!(answer.status(), 403);
    assert_eq!(proxy.lines(), ["CONNECT api.example.test:443 HTTP/1.1", "GET http://api.example.test/plain HTTP/1.1"]);
    assert_eq!(target.count(), 0, "nothing went direct");

    // Where the script says direct, direct.
    assert_eq!(router.route(&url("https://wiki.direct.test/")), Route::Direct);
    assert_eq!(router.route(&url("https://intranet/")), Route::Direct);
    assert_eq!(router.route(&url("https://api.example.test/")), Route::Through(url(&format!("http://127.0.0.1:{}", proxy.port))));

    // rclone gets the same proxy for Dropbox.
    let env = rclone_env(&router, &["api.dropboxapi.com", "content.dropboxapi.com"], true).unwrap();
    let expected = Some(format!("http://127.0.0.1:{}", proxy.port));
    assert_eq!(env, [("HTTPS_PROXY", expected.clone()), ("HTTP_PROXY", expected), ("NO_PROXY", None), ("RCLONE_HTTP_PROXY", None)]);
}

/// rclone itself through the proxy a script names, with the environment SlopBar gives it over an
/// inherited one that would send it elsewhere: it asks the fake proxy for its own download site,
/// which refuses. Needs rclone: installed (SlopBar's own copy, WinGet's, or on PATH), or with
/// SLOPBAR_TEST_RCLONE=download SlopBar's pinned one, downloaded and checked as the setup does (the
/// Tests workflow): `cargo test --lib rclone_itself -- --ignored`.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn rclone_itself_goes_through_the_proxy_a_script_names() {
    let dir = crate::test_support::TempDir::new();
    let exe = match crate::rclone::find() {
        Some(exe) => exe,
        None if std::env::var("SLOPBAR_TEST_RCLONE").as_deref() == Ok("download") => {
            let dest = dir.0.join("rclone").join("rclone.exe");
            let mut reasons = Vec::new();
            crate::sharing_setup::install_rclone(&dest, &std::sync::atomic::AtomicBool::new(false), &|_| {}, &mut reasons).await
                .unwrap_or_else(|e| panic!("{e}: {reasons:?}"));
            dest
        }
        None => panic!("rclone installed, or SLOPBAR_TEST_RCLONE=download"),
    };
    let proxy = FakeProxy::start();
    let router = scripted(serve_script(format!("function FindProxyForURL(url, host) {{ return 'PROXY 127.0.0.1:{}'; }}", proxy.port)));
    let mut command = crate::sources::command(&exe.to_string_lossy());
    // What SlopBar's own environment might hold: none of it may win.
    command.env("NO_PROXY", "*").env("RCLONE_HTTP_PROXY", "http://127.0.0.1:1").env("https_proxy", "http://127.0.0.1:1");
    crate::rclone::apply_env(&mut command, rclone_env(&router, &["downloads.rclone.org"], true).unwrap());
    command.args(["lsjson", ":http:", "--http-url", "https://downloads.rclone.org/", "--contimeout", "5s", "--timeout", "5s", "--low-level-retries", "1", "--retries", "1"]);
    let out = tokio::task::spawn_blocking(move || command.output()).await.unwrap().unwrap();
    assert!(!out.status.success(), "the proxy refused it");
    assert!(proxy.lines().iter().any(|line| line == "CONNECT downloads.rclone.org:443 HTTP/1.1"), "{:?}", proxy.lines());
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proxy_the_script_names_that_cannot_be_used_fails_with_its_reason() {
    let target = Target::start();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let returns = |answer: &str| serve_script(format!("function FindProxyForURL(url, host) {{ return '{answer}'; }}"));
    for (script, why) in [
        // WinHTTP's own WinHttpGetProxyForUrl would answer these direct.
        (returns("SOCKS 127.0.0.1:1080"), Why::ProxyUnsupported),
        (returns("SOCKS 127.0.0.1:1080; DIRECT"), Why::ProxyUnsupported),
        (returns("SOCKS5 127.0.0.1:1080"), Why::ProxyUnsupported),
        (returns("BANANA 127.0.0.1:1"), Why::ProxyUnsupported),
        // Not a script, and a script that cannot be fetched.
        (serve_script("function FindProxyForURL(url, host) { return 'PROXY a:1' ".into()), Why::ProxyScript),
        (format!("http://127.0.0.1:{closed}/proxy.pac"), Why::ProxyScript),
    ] {
        let failed = client(scripted(script.clone()), &target).get("https://api.example.test/").send().await.unwrap_err();
        assert_eq!(crate::net::why(&failed), why, "{script}");
    }
    assert_eq!(target.count(), 0, "nothing went direct");
    let socks = scripted(returns("SOCKS 127.0.0.1:1080"));
    assert_eq!(rclone_env(&socks, &["api.dropboxapi.com"], true), Err(Refusal::Unsupported), "nor does rclone run");
}

/// This user's Windows proxy settings, set through WinINet as Windows' own Settings sets them (the
/// LAN connection) and announced, so WinHTTP reads them: for the disposable runner's test only.
#[cfg(windows)]
fn set_user_proxy(flags: u32, server: Option<&str>, bypass: Option<&str>, script: Option<&str>) {
    use windows_sys::Win32::Networking::WinInet::*;
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (server, bypass, script) = (server.map(wide), bypass.map(wide), script.map(wide));
    let text = |w: &Option<Vec<u16>>| w.as_ref().map_or(std::ptr::null_mut(), |w| w.as_ptr() as *mut u16);
    let mut options = [
        INTERNET_PER_CONN_OPTIONW { dwOption: INTERNET_PER_CONN_FLAGS, Value: INTERNET_PER_CONN_OPTIONW_0 { dwValue: flags } },
        INTERNET_PER_CONN_OPTIONW { dwOption: INTERNET_PER_CONN_PROXY_SERVER, Value: INTERNET_PER_CONN_OPTIONW_0 { pszValue: text(&server) } },
        INTERNET_PER_CONN_OPTIONW { dwOption: INTERNET_PER_CONN_PROXY_BYPASS, Value: INTERNET_PER_CONN_OPTIONW_0 { pszValue: text(&bypass) } },
        INTERNET_PER_CONN_OPTIONW { dwOption: INTERNET_PER_CONN_AUTOCONFIG_URL, Value: INTERNET_PER_CONN_OPTIONW_0 { pszValue: text(&script) } },
    ];
    let mut list = INTERNET_PER_CONN_OPTION_LISTW {
        dwSize: std::mem::size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32,
        pszConnection: std::ptr::null_mut(),
        dwOptionCount: options.len() as u32,
        dwOptionError: 0,
        pOptions: options.as_mut_ptr(),
    };
    // SAFETY: the list and every string it points to outlive the calls.
    unsafe {
        let set = InternetSetOptionW(std::ptr::null(), INTERNET_OPTION_PER_CONNECTION_OPTION, &mut list as *mut _ as *const std::ffi::c_void, list.dwSize);
        assert!(set != 0, "InternetSetOptionW failed: {}", windows_sys::Win32::Foundation::GetLastError());
        InternetSetOptionW(std::ptr::null(), INTERNET_OPTION_SETTINGS_CHANGED, std::ptr::null(), 0);
        InternetSetOptionW(std::ptr::null(), INTERNET_OPTION_REFRESH, std::ptr::null(), 0);
    }
}

/// The Tests workflow's proof (`.github/workflows/tests.yml`, "Proxy settings come from Windows"):
/// this user's own proxy settings, changed for real, and every client the app makes follows them:
/// a proxy script set, a manual proxy per protocol with a bypass list, one manual proxy for all, a
/// SOCKS proxy (refused), a script that cannot be fetched (refused, or the manual proxy beside it).
/// It changes this user's proxy settings, so it runs only on a disposable runner, with
/// SLOPBAR_TEST_PROXY_SETTINGS=disposable, and puts Windows' default back as it ends. Never run on a
/// computer people use.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn slopbar_follows_this_users_windows_proxy_settings() {
    use windows_sys::Win32::Networking::WinInet::{PROXY_TYPE_AUTO_DETECT, PROXY_TYPE_AUTO_PROXY_URL, PROXY_TYPE_DIRECT, PROXY_TYPE_PROXY};
    assert_eq!(std::env::var("SLOPBAR_TEST_PROXY_SETTINGS").as_deref(), Ok("disposable"), "this changes this user's proxy settings: a disposable runner only");
    /// Windows' default again, however the test ends.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            set_user_proxy(PROXY_TYPE_DIRECT | PROXY_TYPE_AUTO_DETECT, None, None, None);
        }
    }
    let _restore = Restore;
    // A router of its own for each step (nothing kept from the step before); the app's own clients
    // are asked for hosts no step shares.
    let fresh = || Router::new(Arc::new(System));
    let at = |port: u16| through(&format!("http://127.0.0.1:{port}"));

    // 1. A proxy script set ("Use setup script").
    let scripted = FakeProxy::start();
    let script = serve_script(format!(
        "function FindProxyForURL(url, host) {{ if (host == 'p1-direct.example.test') return 'DIRECT'; return 'PROXY 127.0.0.1:{}'; }}",
        scripted.port
    ));
    set_user_proxy(PROXY_TYPE_DIRECT | PROXY_TYPE_AUTO_PROXY_URL, None, None, Some(&script));
    assert_eq!(super::win::settings(), Ok(Some(Settings { detect: false, script: Some(script.clone()), manual: false })), "WinHTTP reads the change");
    #[allow(unused_mut)] // Only the desktop build adds the usage client.
    let mut clients = vec![
        ("builder", crate::net::builder().build().unwrap()),
        ("download", crate::sharing_setup::download_client()),
        ("phone", crate::phone::client_for(&url("https://p1-phone.example.test/"))),
    ];
    #[cfg(feature = "desktop")]
    clients.push(("usage", crate::app::http_client()));
    for (name, client) in &clients {
        let failed = client.get(format!("https://p1-{name}.example.test/x")).send().await.unwrap_err();
        assert_eq!(crate::net::why(&failed), Why::ProxyRefused, "{name}: the script's proxy refused it");
    }
    let lines = scripted.lines();
    for (name, _) in &clients {
        assert!(lines.contains(&format!("CONNECT p1-{name}.example.test:443 HTTP/1.1")), "{name}: {lines:?}");
    }
    assert_eq!(fresh().route(&url("https://p1-direct.example.test/")), Route::Direct);
    let env = rclone_env(&fresh(), &["p1-cloud.example.test"], true).unwrap();
    assert_eq!(env[0], ("HTTPS_PROXY", Some(format!("http://127.0.0.1:{}", scripted.port))), "rclone gets it too");

    // 2. A manual proxy per protocol, with a bypass list.
    let (plain, secure) = (FakeProxy::start(), FakeProxy::start());
    set_user_proxy(PROXY_TYPE_DIRECT | PROXY_TYPE_PROXY, Some(&format!("http=127.0.0.1:{};https=127.0.0.1:{}", plain.port, secure.port)), Some("<local>;*.p2-bypass.example.test"), None);
    let router = fresh();
    assert_eq!(router.route(&url("https://p2.example.test/")), at(secure.port));
    assert_eq!(router.route(&url("http://p2.example.test/")), at(plain.port));
    assert_eq!(router.route(&url("https://p2intranet/")), Route::Direct, "<local>: a name without a dot");
    assert_eq!(router.route(&url("https://x.p2-bypass.example.test/")), Route::Direct);
    let client = crate::net::builder().build().unwrap();
    let _ = client.get("https://p2-api.example.test/").send().await;
    assert_eq!(client.get("http://p2-api.example.test/plain").send().await.unwrap().status(), 403);
    assert_eq!(secure.lines(), ["CONNECT p2-api.example.test:443 HTTP/1.1"]);
    assert_eq!(plain.lines(), ["GET http://p2-api.example.test/plain HTTP/1.1"]);

    // 3. One manual proxy for every protocol, and no bypass list: a name without a dot too.
    set_user_proxy(PROXY_TYPE_DIRECT | PROXY_TYPE_PROXY, Some(&format!("127.0.0.1:{}", plain.port)), None, None);
    let router = fresh();
    assert_eq!(router.route(&url("https://p3.example.test/")), at(plain.port));
    assert_eq!(router.route(&url("http://p3.example.test/")), at(plain.port));
    assert_eq!(router.route(&url("https://p3intranet/")), at(plain.port));
    // The same proxy saved but turned off: direct.
    set_user_proxy(PROXY_TYPE_DIRECT, Some(&format!("127.0.0.1:{}", plain.port)), None, None);
    assert_eq!(fresh().route(&url("https://p3off.example.test/")), Route::Direct);

    // 4. A SOCKS proxy: refused, never direct; and rclone is not even started (an rclone that
    // does not exist would say so otherwise).
    set_user_proxy(PROXY_TYPE_DIRECT | PROXY_TYPE_PROXY, Some("socks=127.0.0.1:1080"), None, None);
    assert_eq!(fresh().route(&url("https://p4.example.test/")), Route::Refused(Refusal::Unsupported));
    let failed = crate::net::builder().build().unwrap().get("https://p4-api.example.test/").send().await.unwrap_err();
    assert_eq!(crate::net::why(&failed), Why::ProxyUnsupported);
    assert_eq!(crate::net::failure_text(&failed), "proxy_unsupported");
    let missing = std::path::Path::new(r"C:\nonexistent-slopbar-test\rclone.exe");
    let connected = crate::sharing_setup::connect_dropbox(missing, "dropbox", crate::sharing_setup::Connect::Create, &|_| {},
        &std::sync::atomic::AtomicBool::new(false), Duration::from_secs(5));
    assert_eq!(connected, Err("proxy_unsupported"));
    let store = crate::rclone::Rclone::with("dropbox", crate::rclone::Cli::for_remote(missing.to_path_buf(), "dropbox"));
    let listed = crate::presence::PresenceStore::list(&store).unwrap_err();
    assert_eq!(refusal_in(&listed), Some(Refusal::Unsupported), "{listed}");

    // 5. A script that cannot be fetched: refused; with a manual proxy beside it, that proxy.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let gone = format!("http://127.0.0.1:{closed}/gone.pac");
    set_user_proxy(PROXY_TYPE_DIRECT | PROXY_TYPE_AUTO_PROXY_URL, None, None, Some(&gone));
    assert_eq!(fresh().route(&url("https://p5.example.test/")), Route::Refused(Refusal::Script));
    set_user_proxy(PROXY_TYPE_DIRECT | PROXY_TYPE_AUTO_PROXY_URL | PROXY_TYPE_PROXY, Some(&format!("127.0.0.1:{}", plain.port)), None, Some(&gone));
    assert_eq!(fresh().route(&url("https://p5.example.test/")), at(plain.port));

    // 6. Windows' default (detection on): direct where nothing is found.
    set_user_proxy(PROXY_TYPE_DIRECT | PROXY_TYPE_AUTO_DETECT, None, None, None);
    assert_eq!(fresh().route(&url("https://p6.example.test/")), Route::Direct);
}
