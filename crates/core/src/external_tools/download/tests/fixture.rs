//! Local HTTPS fixture for the download tests.
//!
//! The fixture generates a certificate authority and a server certificate at
//! test time, serves HTTP/1.1 over TLS on a loopback port and hands its CA
//! certificate to the test so it can be trusted through the injected
//! `DownloadSource`. No committed key material and no live network are
//! involved.

use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

/// One HTTP/1.1 response; the body is written exactly as given.
pub(super) struct Reply {
    pub(super) status: &'static str,
    pub(super) headers: Vec<(String, String)>,
    pub(super) content_length: Option<u64>,
    pub(super) body: Vec<u8>,
}

impl Reply {
    pub(super) fn ok(body: Vec<u8>) -> Self {
        Self {
            status: "200 OK",
            headers: Vec::new(),
            content_length: Some(body.len() as u64),
            body,
        }
    }

    pub(super) fn redirect(location: String) -> Self {
        Self {
            status: "302 Found",
            headers: vec![("Location".to_owned(), location)],
            content_length: Some(0),
            body: Vec::new(),
        }
    }
}

/// Request handler: the raw request head and the fixture's address.
pub(super) type Handler = Arc<dyn Fn(&str, SocketAddr) -> Reply + Send + Sync>;

/// A running HTTPS fixture.
pub(super) struct Fixture {
    pub(super) address: SocketAddr,
    pub(super) ca_pem: Vec<u8>,
    pub(super) requests: Arc<AtomicUsize>,
}

impl Fixture {
    /// Number of HTTP requests the fixture received so far.
    pub(super) fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// Start the fixture with a TLS server certificate for `127.0.0.1`.
pub(super) async fn start(handler: Handler) -> Fixture {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let (config, ca_pem) = server_config();
    let acceptor = TlsAcceptor::from(config);
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    // The accept loop lives until the test's runtime is dropped.
    std::mem::drop(tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            let handler = handler.clone();
            let counter = counter.clone();
            std::mem::drop(tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(stream).await else {
                    return;
                };
                let _ = serve(stream, address, handler, counter).await;
            }));
        }
    }));
    Fixture {
        address,
        ca_pem,
        requests,
    }
}

async fn serve(
    mut stream: tokio_rustls::server::TlsStream<TcpStream>,
    fixture_address: SocketAddr,
    handler: Handler,
    counter: Arc<AtomicUsize>,
) -> io::Result<()> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 2048];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if request.len() > 128 * 1024 {
            return Ok(());
        }
    }
    counter.fetch_add(1, Ordering::SeqCst);
    let reply = handler(&String::from_utf8_lossy(&request), fixture_address);
    let mut head = format!("HTTP/1.1 {}\r\n", reply.status);
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(length) = reply.content_length {
        head.push_str(&format!("Content-Length: {length}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&reply.body).await?;
    stream.flush().await?;
    stream.shutdown().await.ok();
    Ok(())
}

/// Build the fixture's CA and a `127.0.0.1` server certificate, returning the
/// CA certificate in PEM form for the client to trust.
fn server_config() -> (Arc<ServerConfig>, Vec<u8>) {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    };

    // The dependency graph enables both rustls providers (rmcp's HTTP client
    // pulls aws-lc-rs), so the fixture selects the ring provider explicitly
    // instead of relying on crate-feature inference.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "tars external tool test CA");
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();

    let mut server_params = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    server_params
        .distinguished_name
        .push(DnType::CommonName, "tars external tool fixture");
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_key = KeyPair::generate().unwrap();
    let server_cert = server_params
        .signed_by(&server_key, &ca_cert, &ca_key)
        .unwrap();

    let chain = vec![server_cert.der().clone()];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der()));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
    (Arc::new(config), ca_cert.pem().into_bytes())
}

/// One entry of a fixture archive, written as raw tar bytes so invalid names
/// and link entries can be produced exactly.
pub(super) enum Entry<'a> {
    File(&'a str, &'a [u8]),
    #[cfg_attr(not(unix), allow(dead_code))]
    Dir(&'a str),
    Symlink(&'a str, &'a str),
    HardLink(&'a str, &'a str),
}

/// Build a gzip-compressed tar archive from explicit entries.
pub(super) fn tar_gz(entries: &[Entry<'_>]) -> Vec<u8> {
    let mut tar = Vec::new();
    for entry in entries {
        let (name, kind, body, link): (&str, u8, &[u8], &[u8]) = match entry {
            Entry::File(name, body) => (name, b'0', body, b""),
            Entry::Dir(name) => (name, b'5', b"", b""),
            Entry::Symlink(name, target) => (name, b'2', b"", target.as_bytes()),
            Entry::HardLink(name, target) => (name, b'1', b"", target.as_bytes()),
        };
        tar.extend_from_slice(&raw_header(name.as_bytes(), kind, body.len() as u64, link));
        tar.extend_from_slice(body);
        let padding = (512 - body.len() % 512) % 512;
        tar.extend(std::iter::repeat_n(0u8, padding));
    }
    tar.extend(std::iter::repeat_n(0u8, 1024));
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&tar).unwrap();
    encoder.finish().unwrap()
}

fn raw_header(name: &[u8], kind: u8, size: u64, link: &[u8]) -> [u8; 512] {
    let mut block = [0u8; 512];
    block[..name.len()].copy_from_slice(name);
    block[100..108].copy_from_slice(b"0000755\0");
    block[108..116].copy_from_slice(b"0000000\0");
    block[116..124].copy_from_slice(b"0000000\0");
    block[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    block[136..148].copy_from_slice(b"00000000000\0");
    block[148..156].copy_from_slice(b"        ");
    block[156] = kind;
    if !link.is_empty() {
        block[157..157 + link.len()].copy_from_slice(link);
    }
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    let checksum: u32 = block.iter().map(|byte| u32::from(*byte)).sum();
    block[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    block
}
