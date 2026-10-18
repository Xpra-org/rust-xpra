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
// `command` is the ssh to run, with its own options, from `--ssh`: a ProxyCommand
// for a host that is only reachable through a relay, another ssh binary, `plink`.
// The python client calls the same option `--ssh`. The connection's own arguments
// (`-x -T`, port, user, host, the proxy script) are appended to it either way.
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

// How to run the ssh side of an `ssh://` connection: both options only mean
// something there, so they travel together.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    // `--ssh`, already split into words: the program and its own options. `None` runs `ssh`.
    pub command: Option<Vec<String>>,
    // `--remote-xpra`: the xpra to run on the remote host instead of the one on its PATH.
    pub remote_xpra: Option<String>,
}

impl Settings {
    pub fn is_set(&self) -> bool {
        self.command.is_some() || self.remote_xpra.is_some()
    }
}

pub fn connect(address: &str, username: Option<&str>, display: &str, settings: &Settings)
               -> Result<SshStream, String> {
    let (host, port) = address.rsplit_once(':').ok_or_else(|| format!("missing port in {:?}", address))?;

    let default = ["ssh".to_string()];
    let words = settings.command.as_deref().unwrap_or(&default);
    let (program, options) = words.split_first().ok_or_else(|| "the ssh command is empty".to_string())?;
    let mut cmd = Command::new(program);
    cmd.args(options);
    cmd.arg("-x").arg("-T");
    if port != "22" {
        cmd.arg("-p").arg(port);
    }
    if let Some(user) = username {
        cmd.arg("-l").arg(user);
    }
    cmd.arg(host);
    cmd.arg(remote_command(display, settings.remote_xpra.as_deref()));
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());

    let mut child: Child = cmd.spawn().map_err(|e| format!("failed to launch {program}: {e}"))?;
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

// Splits `--ssh`'s value into words. Whitespace separates them, and `'...'` or `"..."`
// group text, also in the middle of a word (`-oX='a b'` is `-oX=a b`). Unlike the python
// client's `shlex`, a backslash is just a character: on MS Windows `C:\Program Files\...`
// has to arrive intact, and nothing here needs escaping.
pub fn split_command(s: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            None => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if let Some(q) = quote {
        return Err(format!("unterminated {q} quote in {s:?}"));
    }
    if in_word {
        words.push(word);
    }
    if words.is_empty() {
        return Err("no command given".to_string());
    }
    Ok(words)
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

    fn split(s: &str) -> Vec<String> {
        split_command(s).unwrap()
    }

    #[test]
    fn a_command_splits_on_whitespace() {
        assert_eq!(split("ssh -v"), ["ssh", "-v"]);
        assert_eq!(split("  ssh \t -v  "), ["ssh", "-v"]);
    }

    // what SEE passes over its web tunnel: the ProxyCommand is one argument, spaces and all
    #[test]
    fn quotes_keep_a_proxy_command_in_one_word() {
        assert_eq!(split("ssh '-oProxyCommand=see.exe connect %h %p --url wss://t/tunnel'"),
                   ["ssh", "-oProxyCommand=see.exe connect %h %p --url wss://t/tunnel"]);
        assert_eq!(split("ssh \"-oProxyCommand=a b\""), ["ssh", "-oProxyCommand=a b"]);
    }

    #[test]
    fn a_quote_can_start_inside_a_word() {
        assert_eq!(split(r#"ssh -oProxyCommand="C:\x y\see.exe" connect"#),
                   ["ssh", r"-oProxyCommand=C:\x y\see.exe", "connect"]);
        // and the other kind of quote is just a character inside one
        assert_eq!(split("ssh '-oX=\"a b\"'"), ["ssh", "-oX=\"a b\""]);
        assert_eq!(split("''"), [""]);
    }

    #[test]
    fn backslashes_are_not_escapes() {
        assert_eq!(split(r#""C:\Program Files\OpenSSH\ssh.exe" -oBatchMode=yes"#),
                   [r"C:\Program Files\OpenSSH\ssh.exe", "-oBatchMode=yes"]);
        assert_eq!(split(r"C:\ssh\ssh.exe"), [r"C:\ssh\ssh.exe"]);
    }

    #[test]
    fn an_unterminated_quote_or_nothing_at_all_is_an_error() {
        assert!(split_command("ssh 'unterminated").is_err());
        assert!(split_command("ssh \"-oX=a").is_err());
        assert!(split_command("").is_err());
        assert!(split_command("   ").is_err());
    }

    #[test]
    fn settings_are_set_by_either_option() {
        assert!(!Settings::default().is_set());
        assert!(Settings { command: Some(vec!["ssh".into()]), remote_xpra: None }.is_set());
        assert!(Settings { command: None, remote_xpra: Some("/x".into()) }.is_set());
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
