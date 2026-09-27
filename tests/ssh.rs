// End-to-end test of the native ssh transport (`--features ssh-native`) against a real `sshd`:
// key authentication, the host-key check, the remote command, and both directions of the stream.
//
// Unix only, and `#[ignore]`d, because it needs `sshd` and `ssh-keygen` on the machine running it -
// `cargo test --features ssh-native -- --ignored`. The CI job `ssh-native` installs
// openssh-server and runs exactly that; everything the unit tests can cover without a server is
// covered there instead (see src/net/ssh/).
#![cfg(all(feature = "ssh-native", unix))]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use xpra::net::ssh::{connect, SshPrompt};

// what the stub "xpra" prints before it starts echoing, so the test can tell it apart from any
// noise the ssh session itself might produce.
const SENTINEL: &[u8] = b"XPRA-STUB-READY\n";

#[test]
#[ignore = "needs sshd and ssh-keygen on this machine"]
fn a_session_runs_over_the_native_ssh_transport() {
    let dir = scratch("session");
    let port = free_port();
    let mut sshd = start_sshd(&dir, port);
    let known_hosts = dir.join("known_hosts");

    // safety: these are read by net::ssh on the thread this test drives, and no other test in this
    // binary runs concurrently with it (it is the only test in the file).
    unsafe {
        std::env::set_var("XPRA_SSH_KEY", dir.join("user_key"));
        std::env::set_var("XPRA_SSH_KNOWN_HOSTS", &known_hosts);
    }

    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = asked.clone();
    let prompt = SshPrompt {
        // key authentication only: nothing may ask for a password
        secret: Arc::new(|prompt| panic!("unexpected secret prompt: {prompt}")),
        confirm: Arc::new(move |question| {
            assert!(question.contains("SHA256:"), "{question}");
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        }),
    };

    let stub = stub_xpra(&dir);
    let user = std::env::var("USER").expect("USER");
    let mut stream = connect(&format!("127.0.0.1:{port}"), Some(&user), "", Some(&stub), &prompt)
        .unwrap_or_else(|e| {
            let _ = sshd.kill();
            panic!("ssh connect failed: {e}");
        });

    // the remote command ran, and its output reaches us
    let mut buf = vec![0u8; SENTINEL.len()];
    read_exact(&mut stream, &mut buf);
    assert_eq!(buf, SENTINEL);

    // ... and what we write reaches it: the stub echoes its stdin back
    stream.write_all(b"round-trip\n").expect("write");
    let mut echo = vec![0u8; b"round-trip\n".len()];
    read_exact(&mut stream, &mut echo);
    assert_eq!(echo, b"round-trip\n");

    // the unknown host key was put to us once, and accepting it recorded exactly one line
    assert_eq!(asked.load(std::sync::atomic::Ordering::Relaxed), 1);
    let recorded = fs::read_to_string(&known_hosts).expect("known_hosts");
    assert_eq!(recorded.lines().filter(|line| !line.trim().is_empty()).count(), 1, "{recorded}");
    assert!(recorded.contains(&format!("[127.0.0.1]:{port}")), "{recorded}");

    // a second connection takes the "known" path, so the prompt is not consulted again
    let second = connect(&format!("127.0.0.1:{port}"), Some(&user), "", Some(&stub), &prompt);
    assert!(second.is_ok(), "{:?}", second.err());
    assert_eq!(asked.load(std::sync::atomic::Ordering::Relaxed), 1);

    drop(stream);
    drop(second);
    let _ = sshd.kill();
    let _ = fs::remove_dir_all(&dir);
}

// A directory of our own, not cleaned up when a test fails: the sshd log and the keys in it are
// what one would want to look at.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xpra-ssh-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch directory");
    // sshd refuses to use a key the group or world can read, whatever StrictModes says
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("scratch permissions");
    dir
}

fn free_port() -> u16 {
    // the usual small race: the port is free now and sshd claims it a moment later.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    listener.local_addr().expect("local address").port()
}

fn keygen(path: &Path) {
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .status()
        .expect("ssh-keygen");
    assert!(status.success(), "ssh-keygen failed for {}", path.display());
}

fn start_sshd(dir: &Path, port: u16) -> Child {
    let host_key = dir.join("host_key");
    let user_key = dir.join("user_key");
    keygen(&host_key);
    keygen(&user_key);
    fs::copy(user_key.with_extension("pub"), dir.join("authorized_keys")).expect("authorized_keys");

    let config = dir.join("sshd_config");
    fs::write(&config, format!("\
Port {port}
ListenAddress 127.0.0.1
HostKey {home}/host_key
PidFile {home}/sshd.pid
AuthorizedKeysFile {home}/authorized_keys
# the test runs as an ordinary user out of a temporary directory
StrictModes no
UsePAM no
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
PrintMotd no
LogLevel VERBOSE
", home = dir.display())).expect("sshd_config");

    let sshd = ["/usr/sbin/sshd", "/usr/bin/sshd", "/sbin/sshd"].iter()
        .find(|path| Path::new(path).exists())
        .unwrap_or_else(|| panic!("no sshd found: this test needs openssh-server"));
    let log = fs::File::create(dir.join("sshd.log")).expect("sshd log");
    let child = Command::new(sshd)
        .arg("-D")
        .arg("-f").arg(&config)
        .arg("-e")
        .stderr(log)
        .spawn()
        .expect("sshd");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return child;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("sshd did not start listening on {port}; see {}/sshd.log", dir.display());
}

// Stands in for the remote `xpra`: prints a sentinel, then echoes its stdin, which is exactly the
// shape of `xpra _proxy` as far as this transport is concerned. `--remote-xpra` is what lets the
// test point at it, and `command -v` accepts an absolute path as long as it is executable.
fn stub_xpra(dir: &Path) -> String {
    let path = dir.join("xpra");
    fs::write(&path, format!("#!/bin/sh\nprintf '{}'\nexec cat\n",
                             String::from_utf8_lossy(SENTINEL).replace('\n', "\\n")))
        .expect("stub xpra");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("stub permissions");
    path.display().to_string()
}

fn read_exact<R: Read>(stream: &mut R, buf: &mut [u8]) {
    let mut filled = 0;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => panic!("the ssh stream ended after {filled} of {} bytes", buf.len()),
            Ok(n) => filled += n,
            Err(e) => panic!("read failed after {filled} bytes: {e}"),
        }
    }
}
