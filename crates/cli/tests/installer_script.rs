#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

#[derive(Clone)]
enum RouteResponse {
    Ok(Vec<u8>),
    Redirect(String),
}

struct MockReleaseServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockReleaseServer {
    fn start(routes: HashMap<String, RouteResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let addr = listener.local_addr().expect("listener local_addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        handle_connection(&mut stream, &routes);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for MockReleaseServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(stream: &mut TcpStream, routes: &HashMap<String, RouteResponse>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 4096];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    match routes.get(path) {
        Some(RouteResponse::Ok(body)) => {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            if method != "HEAD" {
                let _ = stream.write_all(body);
            }
        }
        Some(RouteResponse::Redirect(location)) => {
            let header = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(header.as_bytes());
        }
        None => {
            let body = b"not found\n";
            let header = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            if method != "HEAD" {
                let _ = stream.write_all(body);
            }
        }
    }
    let _ = stream.flush();
}

fn installer_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("install.sh")
}

fn host_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        (os, arch) => panic!("unsupported host for installer tests: {os}/{arch}"),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn synthetic_binary_script(version_text: &str) -> Vec<u8> {
    format!("#!/bin/sh\nif [ \"${{1:-}}\" = \"--version\" ]; then\n  printf 'tars {version_text}\\n'\n  exit 0\nfi\nexit 0\n")
        .into_bytes()
}

fn build_valid_tarball(version_tag: &str, target: &str, version_text: &str) -> Vec<u8> {
    let stem = format!("tars-{version_tag}-{target}");
    let script = synthetic_binary_script(version_text);
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o755);
    header.set_size(script.len() as u64);
    header.set_cksum();
    builder
        .append_data(&mut header, format!("{stem}/tars"), script.as_slice())
        .expect("append tars binary");

    let readme = b"# TARS\n";
    let mut readme_hdr = tar::Header::new_gnu();
    readme_hdr.set_entry_type(tar::EntryType::Regular);
    readme_hdr.set_mode(0o644);
    readme_hdr.set_size(readme.len() as u64);
    readme_hdr.set_cksum();
    builder
        .append_data(
            &mut readme_hdr,
            format!("{stem}/README.md"),
            readme.as_slice(),
        )
        .expect("append README");

    let encoder = builder.into_inner().expect("finish tar");
    encoder.finish().expect("finish gzip")
}

fn build_traversal_tarball(version_tag: &str, target: &str, malicious_entry_path: &str) -> Vec<u8> {
    let stem = format!("tars-{version_tag}-{target}");
    let script = synthetic_binary_script("0.1.0");
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);

    // Craft raw tar header bypassing tar::Header::set_path normalization.
    let payload = b"pwned\n";
    let mut evil_hdr = tar::Header::new_gnu();
    evil_hdr.set_entry_type(tar::EntryType::Regular);
    evil_hdr.set_mode(0o644);
    evil_hdr.set_size(payload.len() as u64);
    let path_bytes = malicious_entry_path.as_bytes();
    assert!(path_bytes.len() < 100);
    let raw = evil_hdr.as_mut_bytes();
    raw[..100].fill(0);
    raw[..path_bytes.len()].copy_from_slice(path_bytes);
    evil_hdr.set_cksum();
    builder
        .append(&evil_hdr, payload.as_slice())
        .expect("append traversal entry");

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o755);
    header.set_size(script.len() as u64);
    header.set_cksum();
    builder
        .append_data(&mut header, format!("{stem}/tars"), script.as_slice())
        .expect("append binary");

    let encoder = builder.into_inner().expect("finish tar");
    encoder.finish().expect("finish gzip")
}

fn build_symlink_tarball(version_tag: &str, target: &str) -> Vec<u8> {
    let stem = format!("tars-{version_tag}-{target}");
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_mode(0o777);
    header.set_size(0);
    header.set_link_name("/bin/sh").expect("set symlink target");
    header.set_cksum();
    builder
        .append_data(&mut header, format!("{stem}/tars"), std::io::empty())
        .expect("append symlink tars");

    let encoder = builder.into_inner().expect("finish tar");
    encoder.finish().expect("finish gzip")
}

fn run_installer(args: &[&str], home_dir: &Path, extra_env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg(installer_path())
        .args(args)
        .env("HOME", home_dir)
        .env_remove("TARS_VERSION")
        .env_remove("TARS_INSTALL_DIR")
        .env_remove("TARS_RELEASE_BASE_URL")
        .env_remove("TARS_INSTALL_OS")
        .env_remove("TARS_INSTALL_ARCH");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("execute install.sh")
}

fn install_sentinel_binary(install_dir: &Path) -> (PathBuf, Vec<u8>) {
    fs::create_dir_all(install_dir).expect("create install dir");
    let bin = install_dir.join("tars");
    let bytes = synthetic_binary_script("0.1.0-sentinel");
    fs::write(&bin, &bytes).expect("write sentinel binary");
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod sentinel");
    (bin, bytes)
}

fn assert_no_temp_install_files(install_dir: &Path) {
    if !install_dir.exists() {
        return;
    }
    for entry in fs::read_dir(install_dir).expect("read install_dir") {
        let name = entry.expect("dir entry").file_name();
        let name_str = name.to_string_lossy();
        assert!(
            !name_str.starts_with(".tars.install."),
            "unexpected leftover temp file: {name_str}"
        );
    }
}

#[test]
fn fresh_install_with_explicit_version_succeeds_and_leaves_rc_files_untouched() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    let bashrc = home.join(".bashrc");
    let zshrc = home.join(".zshrc");
    let profile = home.join(".profile");
    fs::write(&bashrc, "# sentinel bashrc\n").expect("write bashrc");
    fs::write(&zshrc, "# sentinel zshrc\n").expect("write zshrc");
    fs::write(&profile, "# sentinel profile\n").expect("write profile");

    let target = host_target();
    let version = "v0.1.0";
    let archive = format!("tars-{version}-{target}.tar.gz");
    let tarball = build_valid_tarball(version, target, "0.1.0");
    let sums = format!("{}  {archive}\n", sha256_hex(&tarball));

    let mut routes = HashMap::new();
    routes.insert(
        format!("/releases/download/{version}/{archive}"),
        RouteResponse::Ok(tarball),
    );
    routes.insert(
        format!("/releases/download/{version}/SHA256SUMS.txt"),
        RouteResponse::Ok(sums.into_bytes()),
    );
    let server = MockReleaseServer::start(routes);

    let install_dir = temp.path().join("nested/prefix/bin");
    let base_url = server.base_url();
    let install_dir_str = install_dir.to_str().unwrap();

    let output = run_installer(
        &[
            "--version",
            version,
            "--install-dir",
            install_dir_str,
            "--base-url",
            &base_url,
        ],
        &home,
        &[("PATH", "/usr/bin:/bin")],
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("tars 0.1.0"), "stdout: {stdout}");
    assert!(stdout.contains("export PATH="), "stdout: {stdout}");

    let installed_bin = install_dir.join("tars");
    assert!(installed_bin.is_file());
    let mode = fs::metadata(&installed_bin)
        .expect("stat installed bin")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755);

    let version_out = Command::new(&installed_bin)
        .arg("--version")
        .output()
        .expect("run installed binary");
    assert!(version_out.status.success());
    assert_eq!(String::from_utf8_lossy(&version_out.stdout), "tars 0.1.0\n");

    assert_eq!(fs::read_to_string(&bashrc).unwrap(), "# sentinel bashrc\n");
    assert_eq!(fs::read_to_string(&zshrc).unwrap(), "# sentinel zshrc\n");
    assert_eq!(
        fs::read_to_string(&profile).unwrap(),
        "# sentinel profile\n"
    );
    assert_no_temp_install_files(&install_dir);
}

#[test]
fn default_latest_resolution_and_idempotent_upgrade() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    let install_dir = temp.path().join("bin");
    let (sentinel_bin, _) = install_sentinel_binary(&install_dir);

    let target = host_target();
    let version = "v0.2.0";
    let archive = format!("tars-{version}-{target}.tar.gz");
    let tarball = build_valid_tarball(version, target, "0.2.0");
    let sums = format!("{}  {archive}\n", sha256_hex(&tarball));

    let mut routes = HashMap::new();
    routes.insert(
        "/releases/latest".to_string(),
        RouteResponse::Redirect(format!("/releases/tag/{version}")),
    );
    routes.insert(
        format!("/releases/tag/{version}"),
        RouteResponse::Ok(b"release page".to_vec()),
    );
    routes.insert(
        format!("/releases/download/{version}/{archive}"),
        RouteResponse::Ok(tarball),
    );
    routes.insert(
        format!("/releases/download/{version}/SHA256SUMS.txt"),
        RouteResponse::Ok(sums.into_bytes()),
    );
    let server = MockReleaseServer::start(routes);

    let output = run_installer(
        &[
            "--install-dir",
            install_dir.to_str().unwrap(),
            "--base-url",
            &format!("{}/", server.base_url()),
        ],
        &home,
        &[],
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let version_out = Command::new(&sentinel_bin)
        .arg("--version")
        .output()
        .expect("run upgraded binary");
    assert_eq!(String::from_utf8_lossy(&version_out.stdout), "tars 0.2.0\n");
    assert_no_temp_install_files(&install_dir);
}

#[test]
fn sha256_mismatch_fails_closed_without_modifying_existing_binary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    let install_dir = temp.path().join("bin");
    let (sentinel_bin, sentinel_bytes) = install_sentinel_binary(&install_dir);

    let target = host_target();
    let version = "v0.2.0";
    let archive = format!("tars-{version}-{target}.tar.gz");
    let tarball = build_valid_tarball(version, target, "0.2.0");
    let bad_sums = format!("{}  {archive}\n", "0".repeat(64));

    let mut routes = HashMap::new();
    routes.insert(
        format!("/releases/download/{version}/{archive}"),
        RouteResponse::Ok(tarball),
    );
    routes.insert(
        format!("/releases/download/{version}/SHA256SUMS.txt"),
        RouteResponse::Ok(bad_sums.into_bytes()),
    );
    let server = MockReleaseServer::start(routes);

    let output = run_installer(
        &[
            "--version",
            version,
            "--install-dir",
            install_dir.to_str().unwrap(),
            "--base-url",
            &server.base_url(),
        ],
        &home,
        &[],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("mismatch"),
        "expected mismatch error, got: {stderr}"
    );
    assert_eq!(fs::read(&sentinel_bin).unwrap(), sentinel_bytes);
    assert_no_temp_install_files(&install_dir);
}

#[test]
fn missing_checksum_entry_fails_closed_without_modifying_existing_binary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    let install_dir = temp.path().join("bin");
    let (sentinel_bin, sentinel_bytes) = install_sentinel_binary(&install_dir);

    let target = host_target();
    let version = "v0.2.0";
    let archive = format!("tars-{version}-{target}.tar.gz");
    let tarball = build_valid_tarball(version, target, "0.2.0");
    let sums = format!("{}  tars-{version}-other-target.tar.gz\n", "a".repeat(64));

    let mut routes = HashMap::new();
    routes.insert(
        format!("/releases/download/{version}/{archive}"),
        RouteResponse::Ok(tarball),
    );
    routes.insert(
        format!("/releases/download/{version}/SHA256SUMS.txt"),
        RouteResponse::Ok(sums.into_bytes()),
    );
    let server = MockReleaseServer::start(routes);

    let output = run_installer(
        &[
            "--version",
            version,
            "--install-dir",
            install_dir.to_str().unwrap(),
            "--base-url",
            &server.base_url(),
        ],
        &home,
        &[],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not contain an entry"),
        "expected missing entry error, got: {stderr}"
    );
    assert_eq!(fs::read(&sentinel_bin).unwrap(), sentinel_bytes);
    assert_no_temp_install_files(&install_dir);
}

#[test]
fn path_traversal_tarball_is_rejected_even_with_valid_sha256() {
    let target = host_target();
    let version = "v0.2.0";
    let archive = format!("tars-{version}-{target}.tar.gz");

    for malicious_entry in ["../evil", "/etc/evil"] {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        fs::create_dir_all(&home).expect("create home");
        let install_dir = temp.path().join("bin");
        let (sentinel_bin, sentinel_bytes) = install_sentinel_binary(&install_dir);

        let tarball = build_traversal_tarball(version, target, malicious_entry);
        let sums = format!("{}  {archive}\n", sha256_hex(&tarball));

        let mut routes = HashMap::new();
        routes.insert(
            format!("/releases/download/{version}/{archive}"),
            RouteResponse::Ok(tarball),
        );
        routes.insert(
            format!("/releases/download/{version}/SHA256SUMS.txt"),
            RouteResponse::Ok(sums.into_bytes()),
        );
        let server = MockReleaseServer::start(routes);

        let output = run_installer(
            &[
                "--version",
                version,
                "--install-dir",
                install_dir.to_str().unwrap(),
                "--base-url",
                &server.base_url(),
            ],
            &home,
            &[],
        );

        assert!(
            !output.status.success(),
            "expected failure for malicious entry {malicious_entry}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("unsafe"),
            "expected unsafe path error for {malicious_entry}, got: {stderr}"
        );
        assert!(!temp.path().join("evil").exists());
        assert_eq!(fs::read(&sentinel_bin).unwrap(), sentinel_bytes);
        assert_no_temp_install_files(&install_dir);
    }
}

#[test]
fn symlink_binary_inside_tarball_is_rejected_even_with_valid_sha256() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    let install_dir = temp.path().join("bin");
    let (sentinel_bin, sentinel_bytes) = install_sentinel_binary(&install_dir);

    let target = host_target();
    let version = "v0.2.0";
    let archive = format!("tars-{version}-{target}.tar.gz");
    let tarball = build_symlink_tarball(version, target);
    let sums = format!("{}  {archive}\n", sha256_hex(&tarball));

    let mut routes = HashMap::new();
    routes.insert(
        format!("/releases/download/{version}/{archive}"),
        RouteResponse::Ok(tarball),
    );
    routes.insert(
        format!("/releases/download/{version}/SHA256SUMS.txt"),
        RouteResponse::Ok(sums.into_bytes()),
    );
    let server = MockReleaseServer::start(routes);

    let output = run_installer(
        &[
            "--version",
            version,
            "--install-dir",
            install_dir.to_str().unwrap(),
            "--base-url",
            &server.base_url(),
        ],
        &home,
        &[],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("symlink"),
        "expected symlink rejection error, got: {stderr}"
    );
    assert_eq!(fs::read(&sentinel_bin).unwrap(), sentinel_bytes);
    assert_no_temp_install_files(&install_dir);
}

#[test]
fn unsupported_os_and_arch_fail_closed_with_source_build_hint() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    let install_dir = temp.path().join("bin");

    for (os, arch) in [("FreeBSD", "x86_64"), ("Linux", "riscv64")] {
        let output = run_installer(
            &[
                "--version",
                "v0.1.0",
                "--install-dir",
                install_dir.to_str().unwrap(),
            ],
            &home,
            &[("TARS_INSTALL_OS", os), ("TARS_INSTALL_ARCH", arch)],
        );

        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("unsupported platform") && stderr.contains("cargo build"),
            "unexpected stderr for {os}/{arch}: {stderr}"
        );
    }
}

#[test]
fn http_404_failure_cleans_up_and_preserves_existing_binary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    let install_dir = temp.path().join("bin");
    let (sentinel_bin, sentinel_bytes) = install_sentinel_binary(&install_dir);

    let server = MockReleaseServer::start(HashMap::new());
    let output = run_installer(
        &[
            "--version",
            "v0.9.9",
            "--install-dir",
            install_dir.to_str().unwrap(),
            "--base-url",
            &server.base_url(),
        ],
        &home,
        &[],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to download"),
        "expected download failure error, got: {stderr}"
    );
    assert_eq!(fs::read(&sentinel_bin).unwrap(), sentinel_bytes);
    assert_no_temp_install_files(&install_dir);
}

#[test]
fn missing_home_and_install_dir_fails_closed_with_exit_2() {
    let output = Command::new("/bin/sh")
        .arg(installer_path())
        .env_remove("HOME")
        .env_remove("TARS_INSTALL_DIR")
        .env_remove("TARS_VERSION")
        .env_remove("TARS_RELEASE_BASE_URL")
        .env_remove("TARS_INSTALL_OS")
        .env_remove("TARS_INSTALL_ARCH")
        .output()
        .expect("execute install.sh without HOME");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("install directory cannot be empty"),
        "expected empty install dir error, got: {stderr}"
    );
}

#[test]
fn help_and_invalid_arguments_behave_consistently() {
    let temp = tempfile::tempdir().expect("tempdir");
    let help = run_installer(&["--help"], temp.path(), &[]);
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("--version"));
    assert!(stdout.contains("--install-dir"));
    assert!(stdout.contains("--base-url"));

    let unknown = run_installer(&["--bogus"], temp.path(), &[]);
    assert_eq!(unknown.status.code(), Some(2));

    let bad_version = run_installer(&["--version", "0.1.0"], temp.path(), &[]);
    assert!(!bad_version.status.success());
}
