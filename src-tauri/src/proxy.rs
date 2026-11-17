//! The proxy Windows would use for a link: SlopBar's own HTTPS (`net::builder`), the updater, and
//! rclone all go where Windows' own clients would send the same request.
//!
//! The environment comes first, as it always has: HTTPS_PROXY (https) or HTTP_PROXY (http), else
//! ALL_PROXY, with NO_PROXY. Otherwise Windows decides, from this user's proxy settings read the way
//! Windows' own clients read them (WinHTTP): a proxy script (PAC), one found on the network (WPAD,
//! "Automatically detect settings"), and the manual proxy with its per-protocol servers and bypass
//! list. Nothing here goes around a proxy: when Windows names one SlopBar cannot use (SOCKS, or a
//! kind WinHTTP does not know), or a proxy script that is set, or found, cannot be fetched or run in
//! time, the request fails with that reason instead of going direct. A request to this computer
//! itself never goes through a proxy, as Windows does too.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::Url;

/// How a request goes out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Direct,
    /// Through this proxy: `http://host:port`, or `https://host:port` (a secure connection to the
    /// proxy itself).
    Through(Url),
    /// Not at all: the proxy Windows names cannot be used, for this reason.
    Refused(Refusal),
}

/// Why the proxy Windows names cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Windows names a proxy SlopBar cannot use: SOCKS, or a kind WinHTTP does not know.
    Unsupported,
    /// Windows' proxy script (set, or found on the network) could not be fetched or run in time.
    Script,
}

impl Refusal {
    pub fn why(self) -> crate::net::Why {
        match self {
            Refusal::Unsupported => crate::net::Why::ProxyUnsupported,
            Refusal::Script => crate::net::Why::ProxyScript,
        }
    }

    /// The code Settings, the flyout and agents show: `net::Why`'s own name.
    pub fn code(self) -> &'static str {
        match self {
            Refusal::Unsupported => "proxy_unsupported",
            Refusal::Script => "proxy_script",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Refusal::Unsupported => "proxy-unsupported",
            Refusal::Script => "proxy-script",
        }
    }
}

/// The refusal an rclone run was stopped for (`rclone::proxy_env`), if that is why.
pub fn refusal_in(error: &std::io::Error) -> Option<Refusal> {
    error.get_ref().and_then(|inner| inner.downcast_ref::<ProxyRefused>()).map(|refused| refused.0)
}

/// A request refused before it left this computer: what `net::why` finds in its error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxyRefused(pub Refusal);

impl std::fmt::Display for ProxyRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            Refusal::Unsupported => "Windows is set to use a proxy SlopBar can't use",
            Refusal::Script => "Windows' proxy script couldn't be fetched or run",
        })
    }
}

impl std::error::Error for ProxyRefused {}

/// One of Windows' answers for a link, in its order (WinHttpGetProxyResult).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// A proxy; false is direct.
    pub proxy: bool,
    /// WINHTTP_INTERNET_SCHEME: 1 HTTP, 2 HTTPS, 4 SOCKS.
    pub scheme: i32,
    pub host: String,
    pub port: u16,
}

/// This user's proxy settings, as far as the order of Windows' decision needs them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// "Automatically detect settings" (WPAD).
    pub detect: bool,
    /// "Use setup script": its address.
    pub script: Option<String>,
    /// "Use a proxy server": a manual proxy is set.
    pub manual: bool,
}

/// Why Windows gave no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// A WinHTTP error code.
    Os(u32),
    /// No answer within `LIMIT`.
    TimedOut,
}

/// No proxy script found on the network (ERROR_WINHTTP_AUTODETECTION_FAILED).
const NOT_DETECTED: u32 = 12180;
/// WinHTTP's auto-proxy service failed (ERROR_WINHTTP_AUTO_PROXY_SERVICE_ERROR).
const SERVICE_FAILED: u32 = 12178;

/// The longest SlopBar waits for Windows' answer for one link, every question included: a proxy
/// script's download, or looking for one.
pub const LIMIT: Duration = Duration::from_secs(10);

/// Windows' proxy decisions, for `decide`; tests substitute their own. Each question ends by
/// `deadline`.
pub trait Windows: Send + Sync {
    /// This user's proxy settings; None when there are none.
    fn settings(&self) -> Result<Option<Settings>, Refusal>;
    /// Windows' answer for `url` from automatic configuration: detection, the script, or both.
    fn automatic(&self, url: &str, detect: bool, script: Option<&str>, deadline: Instant) -> Result<Vec<Entry>, Failure>;
    /// Windows' answer for `url` from the manual proxy: its per-protocol servers and bypass list.
    fn manual(&self, url: &str, deadline: Instant) -> Result<Vec<Entry>, Failure>;
    /// Whether WinHTTP's auto-proxy service is turned off (disabled), so that detection cannot run.
    fn detection_off(&self) -> bool;
}

/// What decided a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decided {
    /// A link on this computer, or nothing set: direct.
    Nothing,
    /// The proxy environment variables.
    Environment,
    /// Windows' automatic configuration: a proxy script, which may answer each host differently.
    Script,
    /// Windows' manual proxy, with its bypass list.
    Manual,
}

/// Where Windows sends a request to `url`, in Windows' own order: automatic configuration first (a
/// script found on the network, then the script set), then the manual proxy, then direct. Where
/// SlopBar parts from Windows, it is never toward direct: a script that is set or found but fails
/// leaves the manual proxy if there is one (as Windows does) and otherwise refuses, where Windows
/// would go direct. Only "nothing found on the network", or detection turned off, lets detection
/// alone go on as if it were not set.
pub fn decide(windows: &dyn Windows, url: &str) -> (Route, Decided) {
    let deadline = Instant::now() + LIMIT;
    let settings = match windows.settings() {
        Ok(Some(settings)) => settings,
        Ok(None) => return (Route::Direct, Decided::Nothing),
        Err(refusal) => return (Route::Refused(refusal), Decided::Nothing),
    };
    let mut script_failed = false;
    if settings.detect || settings.script.is_some() {
        match windows.automatic(url, settings.detect, settings.script.as_deref(), deadline) {
            Ok(entries) => return (first_choice(&entries), Decided::Script),
            // Detection alone found nothing on the network, or cannot run at all: Windows goes on
            // to the manual proxy, else direct.
            Err(Failure::Os(NOT_DETECTED)) if settings.script.is_none() => {}
            Err(Failure::Os(SERVICE_FAILED)) if settings.script.is_none() && windows.detection_off() => {}
            // A script set or found that failed, or anything else that went wrong.
            Err(_) => script_failed = true,
        }
    }
    if settings.manual {
        let route = match windows.manual(url, deadline) {
            Ok(entries) => first_choice(&entries),
            Err(_) => Route::Refused(Refusal::Unsupported),
        };
        return (route, Decided::Manual);
    }
    if script_failed { (Route::Refused(Refusal::Script), Decided::Script) } else { (Route::Direct, Decided::Nothing) }
}

/// Whether a failed question about this script may be asked again with this user's Windows sign-in:
/// only for a script set in Settings (an address the user or their company chose), never one found
/// on the network, which any network can name.
pub fn sign_in_allowed(detect: bool, script: Option<&str>) -> bool {
    !detect && script.is_some()
}

/// Windows' first choice among `entries`, or why it cannot be used. Never a later entry: a SOCKS
/// proxy first and direct second is no permission to go direct. WinHTTP itself drops the kinds it
/// does not know ("SOCKS5 …", "QUIC …"): an answer of only those comes back empty, and is refused;
/// one where DIRECT follows them comes back DIRECT, the script's own fallback for a client that
/// cannot use them, and Windows' answer.
pub fn first_choice(entries: &[Entry]) -> Route {
    let Some(first) = entries.first() else {
        return Route::Refused(Refusal::Unsupported);
    };
    if !first.proxy {
        return Route::Direct;
    }
    let scheme = match first.scheme {
        1 => "http",
        2 => "https",
        _ => return Route::Refused(Refusal::Unsupported),
    };
    proxy_url(scheme, &first.host, first.port).map_or(Route::Refused(Refusal::Unsupported), Route::Through)
}

/// `scheme://host:port` for a proxy Windows named: a host name, an IPv4 address or a bracketed IPv6
/// one, and a port; nothing that could read as a user name, a path or another port.
fn proxy_url(scheme: &str, host: &str, port: u16) -> Option<Url> {
    let name = !host.is_empty() && host.len() <= 253 && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    let v6 = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).is_some_and(|h| h.parse::<std::net::Ipv6Addr>().is_ok());
    if port == 0 || !(name || v6) {
        return None;
    }
    Url::parse(&format!("{scheme}://{host}:{port}")).ok().filter(usable)
}

/// Whether reqwest takes `proxy` as the proxy: it reads what `Proxy::custom` returns with
/// hyper-util's own rules, and goes direct for anything they reject (a host `http::Uri` refuses, a
/// port out of range), so such a proxy must be refused here instead.
fn usable(proxy: &Url) -> bool {
    matches!(proxy.scheme(), "http" | "https")
        && hyper_util::client::proxy::matcher::Matcher::builder()
            .all(String::from(proxy.clone()))
            .build()
            .intercept(&http::Uri::from_static("https://slopbar.invalid/"))
            .is_some()
}

/// Whether `url` is on this computer: `localhost`, 127.0.0.0/8 or ::1. Windows never sends those
/// through a proxy, whatever its settings say.
fn on_this_computer(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Reads an environment variable: the process's own, or a test's.
pub type Env = dyn Fn(&str) -> Option<String> + Send + Sync;

/// The first of `names` set to something in `env`: a blank one counts as unset, as Go (rclone)
/// reads them.
fn env_var(env: &Env, names: &[&str]) -> Option<String> {
    names.iter().filter_map(|name| env(name)).find(|v| !v.trim().is_empty())
}

/// The route the environment sets for `url`, when it sets one: HTTPS_PROXY (https) or HTTP_PROXY
/// (http), upper case first, else ALL_PROXY; NO_PROXY as reqwest and curl read it. A value that is
/// no http(s) proxy is refused, not ignored.
fn from_env(env: &Env, url: &Url) -> Option<Route> {
    let own = match url.scheme() {
        "https" => env_var(env, &["HTTPS_PROXY", "https_proxy"]),
        "http" => env_var(env, &["HTTP_PROXY", "http_proxy"]),
        _ => None,
    };
    let value = own.or_else(|| env_var(env, &["ALL_PROXY", "all_proxy"]))?;
    let bypass = env_var(env, &["NO_PROXY", "no_proxy"]).unwrap_or_default();
    // hyper-util's own NO_PROXY rules (the ones reqwest applied): a placeholder proxy that
    // NO_PROXY either lets through or not.
    let rules = hyper_util::client::proxy::matcher::Matcher::builder().all("http://placeholder.invalid:1").no(bypass).build();
    let uri = url.as_str().parse::<http::Uri>().ok()?;
    if rules.intercept(&uri).is_none() {
        return Some(Route::Direct);
    }
    Some(env_proxy(&value).map_or(Route::Refused(Refusal::Unsupported), Route::Through))
}

/// An environment variable's proxy: `http://` or `https://`, or `host:port` (http), as curl and
/// hyper-util read it, and as reqwest will take it (`usable`).
fn env_proxy(value: &str) -> Option<Url> {
    let value = value.trim();
    let url = Url::parse(&if value.contains("://") { value.to_string() } else { format!("http://{value}") }).ok()?;
    (url.host_str().is_some_and(|h| !h.is_empty()) && usable(&url)).then_some(url)
}

/// How long a route is kept: briefly, since Windows' answers change with the network (a VPN coming
/// up, a setting changed) and WinHTTP answers again in milliseconds; a refusal, which may have taken
/// `LIMIT` to reach, a little longer.
const KEPT: Duration = Duration::from_secs(5);
const KEPT_REFUSED: Duration = Duration::from_secs(15);
/// More links than SlopBar ever asks for: past this the cache starts again.
const MOST_KEPT: usize = 256;

/// Routes for links, from the environment and a `Windows`, kept a short while per scheme, host and
/// port, and asked once at a time per link.
pub struct Router {
    windows: Arc<dyn Windows>,
    env: Box<Env>,
    kept: Mutex<HashMap<String, (Route, Decided, Instant)>>,
    asking: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Router {
    /// Routes from this process's environment and `windows`.
    pub fn new(windows: Arc<dyn Windows>) -> Router {
        Router::with_env(windows, Box::new(|name| std::env::var(name).ok()))
    }

    pub fn with_env(windows: Arc<dyn Windows>, env: Box<Env>) -> Router {
        Router { windows, env, kept: Mutex::new(HashMap::new()), asking: Mutex::new(HashMap::new()) }
    }

    /// The route for `url` (only its scheme, host and port count).
    pub fn route(&self, url: &Url) -> Route {
        self.decision(url).0
    }

    /// The route for `url`, and what decided it.
    pub fn decision(&self, url: &Url) -> (Route, Decided) {
        if on_this_computer(url) {
            return (Route::Direct, Decided::Nothing);
        }
        if let Some(route) = from_env(&*self.env, url) {
            return (route, Decided::Environment);
        }
        let Some(host) = url.host_str() else { return (Route::Direct, Decided::Nothing) };
        // What a proxy script sees, as a browser hands it over: scheme, host, and a port only
        // when it is not the scheme's own; never a path or query.
        let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
        let link = format!("{}://{host}{port}/", url.scheme());
        if let Some(kept) = self.kept(&link) {
            return kept;
        }
        let gate = self.asking.lock().unwrap_or_else(|e| e.into_inner()).entry(link.clone()).or_default().clone();
        // WinHTTP's script and detection run on its own threads; this one only waits (for them, or
        // for a question about the same link already asked), and on a runtime's worker it says so,
        // so the worker's other tasks move elsewhere meanwhile.
        blocking(|| {
            let _one_at_a_time = gate.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(kept) = self.kept(&link) {
                return kept;
            }
            let decided = decide(&*self.windows, &link);
            let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
            if kept.len() >= MOST_KEPT {
                kept.clear();
                self.asking.lock().unwrap_or_else(|e| e.into_inner()).clear();
            }
            kept.insert(link.clone(), (decided.0.clone(), decided.1, Instant::now()));
            decided
        })
    }

    /// `link`'s route, if one is kept and still fresh.
    fn kept(&self, link: &str) -> Option<(Route, Decided)> {
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        let (route, decided, at) = kept.get(link)?;
        let fresh = if matches!(route, Route::Refused(_)) { KEPT_REFUSED } else { KEPT };
        (at.elapsed() < fresh).then(|| (route.clone(), *decided))
    }

    /// What `Proxy::custom` needs: the proxy, None for direct, or for a refusal a proxy whose name
    /// SlopBar's own resolver (`Resolver`) answers with the refusal, so the request fails with it.
    pub fn proxy_for(&self, url: &Url) -> Option<Url> {
        match self.route(url) {
            Route::Direct => None,
            Route::Through(proxy) => Some(proxy),
            Route::Refused(refusal) => Some(stand_in(refusal)),
        }
    }
}

/// Runs `f`, which blocks, telling a multi-thread runtime's worker first (`block_in_place`); on a
/// current-thread runtime or none, it just runs.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// The names that stand in for a refusal: `.invalid` never resolves anywhere (RFC 6761), and
/// `Resolver` answers these before any lookup.
const STAND_IN: &str = ".slopbar.invalid";

fn stand_in(refusal: Refusal) -> Url {
    Url::parse(&format!("http://{}{STAND_IN}:1", refusal.label())).expect("a valid stand-in")
}

fn refusal_named(host: &str) -> Option<Refusal> {
    match host.strip_suffix(STAND_IN)? {
        "proxy-unsupported" => Some(Refusal::Unsupported),
        "proxy-script" => Some(Refusal::Script),
        _ => None,
    }
}

/// SlopBar's name lookups: a stand-in for a refusal fails with it, without a lookup; every other
/// name goes to the system's resolver (getaddrinfo) on a blocking thread, as reqwest's own does.
pub struct Resolver;

impl reqwest::dns::Resolve for Resolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        type Failed = Box<dyn std::error::Error + Send + Sync>;
        if let Some(refusal) = refusal_named(name.as_str()) {
            return Box::pin(std::future::ready(Err(Box::new(ProxyRefused(refusal)) as Failed)));
        }
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found = tokio::task::spawn_blocking(move || {
                std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), 0)).map(|addrs| addrs.collect::<Vec<_>>())
            })
            .await
            .map_err(|e| Box::new(std::io::Error::new(std::io::ErrorKind::Interrupted, e)) as Failed)?
            .map_err(|e| Box::new(e) as Failed)?;
            Ok(Box::new(found.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// The router SlopBar's own requests use: this user's Windows settings.
pub fn system() -> &'static Arc<Router> {
    static ROUTER: std::sync::OnceLock<Arc<Router>> = std::sync::OnceLock::new();
    ROUTER.get_or_init(|| Arc::new(Router::new(Arc::new(System))))
}

/// The proxy environment rclone (Go: HTTPS_PROXY, HTTP_PROXY, NO_PROXY) needs to go where Windows
/// would send SlopBar's own requests to `hosts`, the cloud's hosts: each proxy variable set to the
/// proxy, or removed for direct, and NO_PROXY removed, so that no inherited exception sends rclone
/// around it (Go never sends this computer's own addresses through a proxy anyway). All of `hosts`
/// must go the same way, since rclone takes one proxy per scheme; when they do not, or the proxy is
/// refused, rclone must not run, for this reason. `known`: whether `hosts` are the remote's own; for
/// a cloud SlopBar does not know they only stand in for it, and where a proxy script decides (which
/// may answer each host its own way) rclone does not run either. Windows' variable names are not
/// case-sensitive: `rclone::apply_env` removes the lower-case ones elsewhere.
pub fn rclone_env(router: &Router, hosts: &[&str], known: bool) -> Result<Vec<(&'static str, Option<String>)>, Refusal> {
    let mut env = Vec::new();
    for (scheme, var) in [("https", "HTTPS_PROXY"), ("http", "HTTP_PROXY")] {
        let mut chosen: Option<Route> = None;
        for host in hosts {
            let Ok(url) = Url::parse(&format!("{scheme}://{host}/")) else { return Err(Refusal::Unsupported) };
            let (route, decided) = router.decision(&url);
            if let Route::Refused(refusal) = route {
                return Err(refusal);
            }
            if !known && decided == Decided::Script {
                return Err(Refusal::Unsupported);
            }
            if chosen.as_ref().is_some_and(|c| *c != route) {
                return Err(Refusal::Unsupported);
            }
            chosen = Some(route);
        }
        match chosen {
            Some(Route::Through(proxy)) => env.push((var, Some(proxy.as_str().trim_end_matches('/').to_string()))),
            _ => env.push((var, None)),
        }
    }
    env.push(("NO_PROXY", None));
    Ok(env)
}

/// This user's Windows, through WinHTTP.
pub struct System;

#[cfg(windows)]
impl Windows for System {
    fn settings(&self) -> Result<Option<Settings>, Refusal> {
        win::settings()
    }

    fn automatic(&self, url: &str, detect: bool, script: Option<&str>, deadline: Instant) -> Result<Vec<Entry>, Failure> {
        win::automatic(url, detect, script, deadline)
    }

    fn manual(&self, url: &str, deadline: Instant) -> Result<Vec<Entry>, Failure> {
        win::manual(url, deadline)
    }

    fn detection_off(&self) -> bool {
        win::detection_off()
    }
}

/// Elsewhere there are no Windows settings: the environment alone decides.
#[cfg(not(windows))]
impl Windows for System {
    fn settings(&self) -> Result<Option<Settings>, Refusal> {
        Ok(None)
    }

    fn automatic(&self, _: &str, _: bool, _: Option<&str>, _: Instant) -> Result<Vec<Entry>, Failure> {
        Err(Failure::Os(NOT_DETECTED))
    }

    fn manual(&self, _: &str, _: Instant) -> Result<Vec<Entry>, Failure> {
        Ok(vec![Entry { proxy: false, scheme: 0, host: String::new(), port: 0 }])
    }

    fn detection_off(&self) -> bool {
        true
    }
}

#[cfg(windows)]
mod win {
    //! WinHTTP's proxy resolution: WinHttpGetIEProxyConfigForCurrentUser for the settings, and
    //! WinHttpGetProxyForUrlEx (asynchronous, on WinHTTP's own threads) for the answer, which
    //! keeps each proxy's kind and order, where WinHttpGetProxyForUrl turns a SOCKS proxy into
    //! direct.

    use super::{Entry, Failure, Refusal, Settings};
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex, OnceLock};
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING};
    use windows_sys::Win32::Networking::WinHttp::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    /// The text at `p`, a NUL-terminated UTF-16 string WinHTTP gave.
    unsafe fn text(p: *const u16) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut len = 0;
        while len < 4096 && *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }

    /// A string WinHttpGetIEProxyConfigForCurrentUser gave, freed (the caller's to free).
    unsafe fn take(p: *mut u16) -> Option<String> {
        if p.is_null() {
            return None;
        }
        let s = text(p);
        GlobalFree(p as _);
        Some(s).filter(|s| !s.trim().is_empty())
    }

    pub fn settings() -> Result<Option<Settings>, Refusal> {
        let mut config = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG {
            fAutoDetect: 0,
            lpszAutoConfigUrl: std::ptr::null_mut(),
            lpszProxy: std::ptr::null_mut(),
            lpszProxyBypass: std::ptr::null_mut(),
        };
        // SAFETY: a zeroed out-structure; each string it fills is freed by `take`, read or not.
        let (ok, error, script, proxy) = unsafe {
            let ok = WinHttpGetIEProxyConfigForCurrentUser(&mut config) != 0;
            let error = if ok { 0 } else { GetLastError() };
            let script = take(config.lpszAutoConfigUrl);
            let proxy = take(config.lpszProxy);
            let _bypass = take(config.lpszProxyBypass);
            (ok, error, script, proxy)
        };
        if !ok {
            // No settings at all is direct; settings that cannot be read could name a proxy.
            return if error == ERROR_FILE_NOT_FOUND { Ok(None) } else { Err(Refusal::Unsupported) };
        }
        Ok(Some(Settings { detect: config.fAutoDetect != 0, script, manual: proxy.is_some() }))
    }

    pub fn automatic(url: &str, detect: bool, script: Option<&str>, deadline: Instant) -> Result<Vec<Entry>, Failure> {
        let flags = if detect { WINHTTP_AUTOPROXY_AUTO_DETECT } else { 0 } | if script.is_some() { WINHTTP_AUTOPROXY_CONFIG_URL } else { 0 };
        // As Microsoft advises: without the user's sign-in first, with it when the script's server
        // asks for one, and only for a script set in Settings (`sign_in_allowed`).
        match ask(url, flags, script, false, deadline) {
            Err(Failure::Os(ERROR_WINHTTP_LOGIN_FAILURE)) if super::sign_in_allowed(detect, script) => ask(url, flags, script, true, deadline),
            other => other,
        }
    }

    pub fn manual(url: &str, deadline: Instant) -> Result<Vec<Entry>, Failure> {
        ask(url, WINHTTP_AUTOPROXY_ALLOW_STATIC, None, false, deadline)
    }

    /// Whether WinHTTP's auto-proxy service is disabled (its Start value 4), so that nothing can
    /// be detected; a service that is merely stopped starts when asked.
    pub fn detection_off() -> bool {
        use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
        let key = wide(r"SYSTEM\CurrentControlSet\Services\WinHttpAutoProxySvc");
        let name = wide("Start");
        let (mut start, mut size) = (0u32, std::mem::size_of::<u32>() as u32);
        // SAFETY: a DWORD read into a DWORD.
        let read = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut start as *mut u32 as *mut c_void, &mut size) };
        read == 0 && start == 4
    }

    /// A session for one question: automatic proxy (this user's settings as they are now, Windows
    /// 8.1 and later), asynchronous, with a script's download held to WinHTTP's timeouts. None
    /// when WinHTTP cannot open one. WinHTTP's own service keeps scripts and what detection found,
    /// so a new session costs milliseconds.
    fn session() -> Option<Handle> {
        // SAFETY: plain WinHTTP calls; the handle is closed by `Handle`'s drop.
        unsafe {
            let agent = wide("SlopBar");
            let session = Handle(WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, std::ptr::null(), std::ptr::null(), WINHTTP_FLAG_ASYNC));
            if session.0.is_null() {
                return None;
            }
            // A script's download: name lookup, connection, request and answer, 5 s each.
            WinHttpSetTimeouts(session.0, 5000, 5000, 5000, 5000);
            let previous = WinHttpSetStatusCallback(session.0, Some(answered), WINHTTP_CALLBACK_FLAG_REQUEST_ERROR | WINHTTP_CALLBACK_FLAG_GETPROXYFORURL_COMPLETE, 0);
            // WINHTTP_INVALID_STATUS_CALLBACK: no answer would ever arrive.
            if previous.map_or(0, |f| f as usize) == usize::MAX {
                return None;
            }
            Some(session)
        }
    }

    /// A WinHTTP handle, closed when dropped: closing a resolver cancels its question.
    struct Handle(*mut c_void);

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: a handle WinHTTP gave and nothing else closes.
                unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    /// A question in flight: what WinHTTP may read until it answers, and where the answer goes.
    struct Pending {
        _url: Vec<u16>,
        _script: Option<Vec<u16>>,
        _options: Box<WINHTTP_AUTOPROXY_OPTIONS>,
        answer: Arc<Answer>,
    }

    // SAFETY: the raw pointers in the options point into the Vecs beside them, which move with it.
    unsafe impl Send for Pending {}

    #[derive(Default)]
    struct Answer {
        got: Mutex<Option<Result<Vec<Entry>, u32>>>,
        ready: Condvar,
    }

    /// Questions in flight, by the context number WinHTTP hands back: an answer for a number no
    /// longer here (a late or repeated one) is dropped.
    fn pending() -> &'static Mutex<HashMap<usize, Pending>> {
        static PENDING: OnceLock<Mutex<HashMap<usize, Pending>>> = OnceLock::new();
        PENDING.get_or_init(Default::default)
    }

    /// WinHTTP's callback, on its own thread (or, for a question cancelled by closing, inside the
    /// close). Panics must not cross it: nothing here unwraps. The question is claimed first, so
    /// an answer that races its own cancellation is dropped unread.
    unsafe extern "system" fn answered(resolver: *mut c_void, context: usize, status: u32, info: *mut c_void, length: u32) {
        if !matches!(status, WINHTTP_CALLBACK_STATUS_GETPROXYFORURL_COMPLETE | WINHTTP_CALLBACK_STATUS_REQUEST_ERROR) {
            return;
        }
        let Some(pending) = pending().lock().unwrap_or_else(|e| e.into_inner()).remove(&context) else { return };
        let result = if status == WINHTTP_CALLBACK_STATUS_GETPROXYFORURL_COMPLETE {
            entries(resolver)
        } else if !info.is_null() && length as usize >= std::mem::size_of::<WINHTTP_ASYNC_RESULT>() {
            Err((*(info as *const WINHTTP_ASYNC_RESULT)).dwError)
        } else {
            Err(0)
        };
        *pending.answer.got.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        pending.answer.ready.notify_all();
    }

    /// The entries of `resolver`'s answer, copied, and WinHTTP's copy freed.
    unsafe fn entries(resolver: *mut c_void) -> Result<Vec<Entry>, u32> {
        let mut result = WINHTTP_PROXY_RESULT { cEntries: 0, pEntries: std::ptr::null_mut() };
        let error = WinHttpGetProxyResult(resolver, &mut result);
        if error != 0 {
            return Err(error);
        }
        let list = (0..result.cEntries as usize)
            .map(|i| {
                let entry = &*result.pEntries.add(i);
                Entry { proxy: entry.fProxy != 0, scheme: entry.ProxyScheme, host: text(entry.pwszProxy), port: entry.ProxyPort }
            })
            .collect();
        WinHttpFreeProxyResult(&mut result);
        Ok(list)
    }

    /// Asks WinHTTP for `url`'s proxies with `flags`, waiting until `deadline` at most; one still
    /// running then is cancelled.
    fn ask(url: &str, flags: u32, script: Option<&str>, logon: bool, deadline: Instant) -> Result<Vec<Entry>, Failure> {
        if deadline <= Instant::now() {
            return Err(Failure::TimedOut);
        }
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let session = session().ok_or(Failure::Os(ERROR_WINHTTP_INTERNAL_ERROR))?;
        let mut resolver = Handle(std::ptr::null_mut());
        // SAFETY: an open session; the resolver handle closes when dropped (before the session).
        let error = unsafe { WinHttpCreateProxyResolver(session.0, &mut resolver.0) };
        if error != 0 {
            return Err(Failure::Os(error));
        }
        let url = wide(url);
        let script = script.map(wide);
        let detect = if flags & WINHTTP_AUTOPROXY_AUTO_DETECT != 0 { WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A } else { 0 };
        let options = Box::new(WINHTTP_AUTOPROXY_OPTIONS {
            dwFlags: flags,
            dwAutoDetectFlags: detect,
            lpszAutoConfigUrl: script.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            lpvReserved: std::ptr::null_mut(),
            dwReserved: 0,
            fAutoLogonIfChallenged: logon as i32,
        });
        let (url_at, options_at) = (url.as_ptr(), &*options as *const WINHTTP_AUTOPROXY_OPTIONS);
        let answer = Arc::new(Answer::default());
        let context = NEXT.fetch_add(1, Ordering::Relaxed);
        // In place before asking: the answer can come before the call returns.
        pending().lock().unwrap_or_else(|e| e.into_inner())
            .insert(context, Pending { _url: url, _script: script, _options: options, answer: answer.clone() });
        // SAFETY: the url and options live in `pending` until the answer, or forever if none comes.
        let error = unsafe { WinHttpGetProxyForUrlEx(resolver.0, url_at, options_at, context) };
        let got = if error == ERROR_IO_PENDING {
            let mut got = answer.got.lock().unwrap_or_else(|e| e.into_inner());
            while got.is_none() {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                got = answer.ready.wait_timeout(got, left).unwrap_or_else(|e| e.into_inner()).0;
            }
            got.take()
        } else {
            pending().lock().unwrap_or_else(|e| e.into_inner()).remove(&context);
            Some(Err(error))
        };
        // Closing cancels a question still running; its late answer finds nothing pending.
        drop(resolver);
        drop(session);
        match got {
            Some(Ok(entries)) => Ok(entries),
            Some(Err(error)) => Err(Failure::Os(error)),
            None => Err(Failure::TimedOut),
        }
    }
}

#[cfg(test)]
mod tests;
