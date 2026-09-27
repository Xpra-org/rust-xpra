// `ssh://` transport: shells out to the system `ssh` binary and treats its
// stdin/stdout pipes as the byte stream, instead of opening a TCP socket
// directly - the same approach xpra's own client uses (see
// `xpra/net/ssh/exec_client.py`), and the reason no SSH library dependency is
// pulled in here (a full SSH client implementation, e.g. `russh`, costs ~2MB
// and an async runtime - see README.md).
//
// The remote command run over that ssh session is `xpra _proxy [display]`,
// which is xpra's own subcommand for bridging stdin/stdout to an existing
// display's unix-domain socket. It's wrapped in a `command -v` guard (mirrors
// `get_ssh_command()` in the file above) so a missing remote `xpra` produces
// a clean error instead of a raw shell "command not found".
//
// `remote_xpra` is the path to run instead of the bare name, for the servers
// that are not on the remote login shell's PATH: a relocatable install under a
// shared prefix, which is how xpra is deployed on a cluster whose nodes have no
// xpra package and where no one has root. The python client calls the same
// option `--remote-xpra`.
//
// Authentication must not require interactive input on stdin, since stdin
// carries the xpra packet stream, not a terminal - use key-based auth with an
// ssh-agent (or a passphrase-less key). Host-key prompts and password
// prompts still work as normal since OpenSSH reads those from the controlling
// terminal (`/dev/tty`), not stdin, when one is available; ssh's stderr is
// inherited so any such prompts/errors are visible if the client was launched
// from a terminal.
use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

pub fn connect(address: &str, username: Option<&str>, display: &str, remote_xpra: Option<&str>)
               -> Result<SshStream, String> {
    let (host, port) = address.rsplit_once(':').ok_or_else(|| format!("missing port in {:?}", address))?;

    let mut cmd = Command::new("ssh");
    cmd.arg("-x").arg("-T");
    if port != "22" {
        cmd.arg("-p").arg(port);
    }
    if let Some(user) = username {
        cmd.arg("-l").arg(user);
    }
    cmd.arg(host);
    cmd.arg(remote_command(display, remote_xpra));
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());

    let mut child: Child = cmd.spawn().map_err(|e| format!("failed to launch ssh: {e}"))?;
    let stdin = child.stdin.take().expect("ssh stdin was piped");
    let stdout = child.stdout.take().expect("ssh stdout was piped");

    // `Child::drop` neither kills nor waits on the process; reap it in the
    // background once it exits (when the pipes are closed / ssh disconnects)
    // so it doesn't linger as a zombie for the rest of this client's runtime.
    thread::spawn(move || {
        let _ = child.wait();
    });

    Ok(SshStream { stdin: Arc::new(Mutex::new(stdin)), stdout: Arc::new(Mutex::new(stdout)) })
}

// The script is one `sh -c` argument, so every quote inside it is escaped again on the
// way out: build it separately from the wrapping, which is also the readable half to
// assert on.
fn remote_command(display: &str, remote_xpra: Option<&str>) -> String {
    format!("sh -c {}", shell_quote(&proxy_script(display, remote_xpra)))
}

fn proxy_script(display: &str, remote_xpra: Option<&str>) -> String {
    // `command -v` answers for an absolute path too (it prints it back when it is
    // executable), so the guard is the same one whether we were given a path or
    // fall back to the name on PATH.
    let xpra = shell_quote(remote_xpra.unwrap_or("xpra"));
    let proxy_cmd = if display.is_empty() { format!("{xpra} _proxy") } else { format!("{xpra} _proxy {}", shell_quote(display)) };
    format!("if command -v {xpra} > /dev/null 2>&1; then {proxy_cmd}; else echo \"no xpra command found:\" {xpra} 1>&2; exit 1; fi")
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_proxy_runs_the_name_on_path_by_default() {
        let script = proxy_script("10", None);
        assert!(script.contains("command -v 'xpra'"), "{script}");
        assert!(script.contains("'xpra' _proxy '10'"), "{script}");
    }

    #[test]
    fn a_remote_path_replaces_the_name_in_both_the_guard_and_the_proxy() {
        let script = proxy_script("10", Some("/red/ssd/appl/xpra/bin/xpra"));
        assert!(script.contains("command -v '/red/ssd/appl/xpra/bin/xpra'"), "{script}");
        assert!(script.contains("'/red/ssd/appl/xpra/bin/xpra' _proxy '10'"), "{script}");
        // the bare name must be gone, or PATH would decide after all
        assert!(!script.contains("'xpra'"), "{script}");
    }

    #[test]
    fn an_empty_display_lets_the_proxy_pick_the_session() {
        assert!(proxy_script("", None).contains("'xpra' _proxy;"));
    }

    // the script is one `sh -c` argument, and the path one word inside it: a path is only
    // ever spelled by shell_quote, so a quote in one cannot start a second command.
    #[test]
    fn a_path_cannot_break_out_of_its_quotes() {
        let path = "/opt/x'; rm -rf ~; '";
        let script = proxy_script("10", Some(path));
        assert_eq!(script.matches(&shell_quote(path)).count(), 3, "{script}");
        assert_eq!(remote_command("10", Some(path)), format!("sh -c {}", shell_quote(&script)));
    }
}

// `stdin`/`stdout` are two independent pipes (unlike a TCP or TLS session,
// there's no single object shared between the read and write directions), so
// - unlike `SharedTlsStream` - the two mutexes below are never contended: the
// reader thread only ever locks `stdout`, the UI thread only ever locks
// `stdin`. They exist only so `try_clone()` can hand the reader thread its
// own `SshStream` while the original stays with the UI thread, matching how
// `TcpStream::try_clone`/`SharedTlsStream::try_clone` are used elsewhere.
#[derive(Clone)]
pub struct SshStream {
    stdin: Arc<Mutex<ChildStdin>>,
    stdout: Arc<Mutex<ChildStdout>>,
}

impl SshStream {
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(SshStream { stdin: self.stdin.clone(), stdout: self.stdout.clone() })
    }
}

impl Read for SshStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stdout.lock().unwrap().read(buf)
    }
}

impl Write for SshStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stdin.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdin.lock().unwrap().flush()
    }
}
