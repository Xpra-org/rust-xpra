// `ssh://` transport, in one of two mutually exclusive implementations:
//
// - `exec.rs` (the default) shells out to the system `ssh` binary and treats
//   its stdin/stdout pipes as the byte stream, the same approach xpra's own
//   client uses (see `xpra/net/ssh/exec_client.py`). No SSH library, and
//   `ssh_config`, `ProxyJump`, smartcards and Kerberos all keep working
//   because OpenSSH is doing the work.
// - `native.rs` (`--features ssh-native`) speaks SSH itself, through russh.
//   It exists for the hosts where there is no usable `ssh` binary at all -
//   corporate workstations where the OpenSSH client is not installed or is
//   blocked by policy - and it is *not* a superset: none of the ssh_config
//   machinery above comes with it.
//
// Which one is compiled is a build-time choice, so both expose the same
// `connect()` signature and the same `SshStream` type and nothing outside this
// module needs to know which is in use (`net/connection.rs` in particular).
//
// What both share is the remote command: `xpra _proxy [display]`, xpra's own
// subcommand for bridging stdin/stdout to an existing display's unix-domain
// socket, wrapped in a `command -v` guard (mirroring `get_ssh_command()` in the
// file above) so a missing remote `xpra` produces a clean error instead of a
// raw shell "command not found".
//
// `remote_xpra` is the path to run instead of the bare name, for the servers
// that are not on the remote login shell's PATH: a relocatable install under a
// shared prefix, which is how xpra is deployed on a cluster whose nodes have no
// xpra package and where no one has root. The python client calls the same
// option `--remote-xpra`.
use std::sync::Arc;

#[cfg(not(feature = "ssh-native"))]
mod exec;
#[cfg(not(feature = "ssh-native"))]
pub use exec::{connect, SshStream};

#[cfg(feature = "ssh-native")]
mod auth;
#[cfg(feature = "ssh-native")]
mod host;
#[cfg(feature = "ssh-native")]
mod native;
#[cfg(feature = "ssh-native")]
pub use native::{connect, SshStream};

// The questions an ssh handshake can ask, answered by whoever called `connect`.
// `net/` is the library half of the crate and cannot reach into `client/`,
// where the prompting lives (pinentry, the dialogs), so they arrive as
// callbacks - see `client/ask.rs` for the implementations. Both are called from
// the ssh thread, hence `Send + Sync`, and both mean "give up" when they answer
// `None`/`false`: neither is allowed to prompt in a loop.
#[derive(Clone)]
pub struct SshPrompt {
    // prompt text -> a secret (a password, or a key's passphrase)
    pub secret: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    // question text -> true to accept (an unknown host key)
    pub confirm: Arc<dyn Fn(&str) -> bool + Send + Sync>,
}

impl SshPrompt {
    // A prompt that declines everything, for the paths that cannot ask: enough
    // for an agent or a passphrase-less key, and a clean failure otherwise.
    pub fn none() -> Self {
        SshPrompt { secret: Arc::new(|_| None), confirm: Arc::new(|_| false) }
    }
}

// The script is one `sh -c` argument, so every quote inside it is escaped again on the
// way out: build it separately from the wrapping, which is also the readable half to
// assert on.
pub(crate) fn remote_command(display: &str, remote_xpra: Option<&str>) -> String {
    format!("sh -c {}", shell_quote(&proxy_script(display, remote_xpra)))
}

pub(crate) fn proxy_script(display: &str, remote_xpra: Option<&str>) -> String {
    // `command -v` answers for an absolute path too (it prints it back when it is
    // executable), so the guard is the same one whether we were given a path or
    // fall back to the name on PATH.
    let xpra = shell_quote(remote_xpra.unwrap_or("xpra"));
    let proxy_cmd = if display.is_empty() { format!("{xpra} _proxy") } else { format!("{xpra} _proxy {}", shell_quote(display)) };
    format!("if command -v {xpra} > /dev/null 2>&1; then {proxy_cmd}; else echo \"no xpra command found:\" {xpra} 1>&2; exit 1; fi")
}

pub(crate) fn shell_quote(s: &str) -> String {
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
