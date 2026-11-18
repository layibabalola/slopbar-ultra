//! Test-only TLS: a throwaway certificate authority, made afresh in every run, and an HTTPS server
//! on this machine with a certificate it issued for `localhost` (`issue_for` issues one for another
//! name, for a test's own server). The authority cannot make other
//! authorities, and its key never leaves the test process: nothing can be issued with it later,
//! not even from the root the CI step puts in a runner's store (`write_os_store_files`).

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

pub struct Authority {
    params: rcgen::CertificateParams,
    key: rcgen::KeyPair,
    pub root: CertificateDer<'static>,
}

impl Authority {
    pub fn new() -> Authority {
        use rcgen::{BasicConstraints, DnType, IsCa, KeyUsagePurpose};
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(DnType::CommonName, "SlopBar test root (synthetic, throwaway)");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let root = params.self_signed(&key).unwrap().der().clone();
        Authority { params, key, root }
    }

    /// A server certificate for `localhost`, and its key.
    pub fn issue(&self) -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>) {
        self.issue_for("localhost")
    }

    /// A server certificate for `name`, and its key.
    pub fn issue_for(&self, name: &str) -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>) {
        use rcgen::{DnType, ExtendedKeyUsagePurpose, IsCa, KeyUsagePurpose};
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec![name.to_string()]).unwrap();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let issuer = rcgen::Issuer::from_params(&self.params, &self.key);
        let cert = params.signed_by(&key, &issuer).unwrap().der().clone();
        (cert, PrivatePkcs8KeyDer::from(key.serialize_der()))
    }

    /// The root, for a client that adds it to what it trusts.
    pub fn reqwest_root(&self) -> reqwest::Certificate {
        reqwest::Certificate::from_der(&self.root).unwrap()
    }
}

/// What the server answers for a path.
#[derive(Clone)]
pub enum Answer {
    Body { status: u16, content_type: &'static str, body: Vec<u8> },
    Redirect { status: u16, location: String },
}

/// An HTTPS server on this machine's loopback addresses (both, since `localhost` may resolve to
/// either), answering each request with `answer(path, port)`.
pub struct Server {
    pub port: u16,
    /// The paths asked, in order.
    pub asked: Arc<Mutex<Vec<String>>>,
}

impl Server {
    pub fn start(cert: CertificateDer<'static>, key: PrivatePkcs8KeyDer<'static>, answer: impl Fn(&str, u16) -> Answer + Send + Sync + 'static) -> Server {
        // Its own cryptography: the clients' (`net::builder`) must install theirs themselves.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions().unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], PrivateKeyDer::Pkcs8(key)).unwrap();
        let config = Arc::new(config);
        let answer = Arc::new(answer);
        let asked = Arc::new(Mutex::new(Vec::new()));
        // The same port on both loopback addresses, so a request to `localhost` reaches this
        // server whichever address it tries; a port taken on one of them is tried again.
        let (v4, v6, port) = (0..20).find_map(|_| {
            let v4 = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
            let port = v4.local_addr().ok()?.port();
            match std::net::TcpListener::bind(("::1", port)) {
                Ok(v6) => Some((v4, Some(v6), port)),
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => None,
                // No IPv6 loopback here: a request tries 127.0.0.1 next.
                Err(_) => Some((v4, None, port)),
            }
        }).expect("a free loopback port");
        for listener in std::iter::once(v4).chain(v6) {
            let (config, answer, asked) = (config.clone(), answer.clone(), asked.clone());
            std::thread::spawn(move || {
                for socket in listener.incoming().map_while(Result::ok) {
                    let (config, answer, asked) = (config.clone(), answer.clone(), asked.clone());
                    std::thread::spawn(move || serve(socket, config, &*answer, &asked, port));
                }
            });
        }
        Server { port, asked }
    }

    pub fn url(&self, path: &str) -> String {
        format!("https://localhost:{}{path}", self.port)
    }
}

fn serve(socket: std::net::TcpStream, config: Arc<rustls::ServerConfig>, answer: &(dyn Fn(&str, u16) -> Answer + Send + Sync), asked: &Mutex<Vec<String>>, port: u16) {
    let _ = socket.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let Ok(connection) = rustls::ServerConnection::new(config) else { return };
    let mut stream = rustls::StreamOwned::new(connection, socket);
    // A client that refuses the certificate ends the handshake here, which is no error of ours.
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
        if request.len() > 64 * 1024 {
            return;
        }
    }
    let text = String::from_utf8_lossy(&request);
    let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
    asked.lock().unwrap().push(path.clone());
    let reply = match answer(&path, port) {
        Answer::Body { status, content_type, body } => {
            let mut reply = format!("HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
            reply.extend_from_slice(&body);
            reply
        }
        Answer::Redirect { status, location } => format!("HTTP/1.1 {status} X\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes(),
    };
    let _ = stream.write_all(&reply);
    stream.conn.send_close_notify();
    let _ = stream.flush();
}

/// Where the CI step keeps the OS-store test's files: the folder in SLOPBAR_TEST_OS_STORE.
pub fn os_store_folder() -> Option<std::path::PathBuf> {
    std::env::var_os("SLOPBAR_TEST_OS_STORE").map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CI step's first part (`.github/workflows/tests.yml`): a fresh authority's root and a
    /// server certificate it issued, written to the folder in SLOPBAR_TEST_OS_STORE for the step
    /// to put the root in the runner's machine store. Never run on a computer people use.
    #[test]
    #[ignore]
    fn write_os_store_files() {
        let folder = os_store_folder().expect("SLOPBAR_TEST_OS_STORE names the folder");
        std::fs::create_dir_all(&folder).unwrap();
        let authority = Authority::new();
        let (cert, key) = authority.issue();
        std::fs::write(folder.join("root.cer"), authority.root.as_ref()).unwrap();
        std::fs::write(folder.join("server.cer"), cert.as_ref()).unwrap();
        std::fs::write(folder.join("server.key"), key.secret_pkcs8_der()).unwrap();
    }
}
