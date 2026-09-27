// The answers to the questions an ssh handshake can ask - `net::ssh::SshPrompt` - assembled from
// whatever this platform offers *without* an event loop. On the command-line path `connect()` runs
// before the winit event loop exists (see `main::run`), so neither dialog in `client/` can be used
// here: what is left is the connection dialog's own password field (when the target came from
// there), the environment, and `pinentry` as a subprocess.
//
// Both answers are one-shot by contract: a `None`/`false` means "give up", and the caller in
// `net/ssh/native.rs` never asks the same question twice, so a wrong password fails the connection
// rather than looping.
use std::env;
use std::sync::Arc;

use log::warn;

use xpra::net::ssh::SshPrompt;

use super::pinentry;

// The ssh password (or a private key's passphrase), for a non-interactive client.
pub const PASSWORD_ENV: &str = "XPRA_SSH_PASSWORD";
// Accept an unknown host key and add it to `known_hosts`, for a client that cannot be asked. Not a
// blanket "trust everything": a key that *changed* is still a hard failure, as with OpenSSH.
pub const ACCEPT_ENV: &str = "XPRA_SSH_ACCEPT_NEW_HOST";

pub fn prompts(dialog_password: Option<String>) -> SshPrompt {
    SshPrompt {
        secret: Arc::new(move |prompt| secret(prompt, dialog_password.as_deref())),
        confirm: Arc::new(confirm),
    }
}

// A password we were given up front beats prompting for one. The same secret answers both a login
// password and a key passphrase - they are asked for one connection, by one user, and telling them
// apart would mean two dialog fields for a case that does not arise in practice.
fn secret(prompt: &str, dialog_password: Option<&str>) -> Option<String> {
    if let Some(password) = given(dialog_password, env::var(PASSWORD_ENV).ok().as_deref()) {
        return Some(password);
    }
    let prog = pinentry::find_pinentry()?;
    match pinentry::run_pinentry(&prog, prompt) {
        Ok(secret) => secret,
        Err(e) => {
            warn!("pinentry failed ({e}), so there is no way to ask for the ssh password");
            None
        }
    }
}

// pinentry has the first word, so that a user who is asked and declines is not overruled by an
// environment variable they set for some earlier connection.
fn confirm(question: &str) -> bool {
    if let Some(prog) = pinentry::find_pinentry() {
        match pinentry::confirm_pinentry(&prog, question) {
            Ok(answer) => return answer,
            Err(e) => warn!("pinentry could not ask ({e})"),
        }
    }
    accepts(env::var(ACCEPT_ENV).ok().as_deref())
}

// The two decisions that do not need a subprocess, split out so they can be tested without
// touching the process environment.
fn given(dialog_password: Option<&str>, environment: Option<&str>) -> Option<String> {
    [dialog_password, environment]
        .into_iter()
        .flatten()
        .find(|password| !password.is_empty())
        .map(str::to_string)
}

fn accepts(environment: Option<&str>) -> bool {
    matches!(environment, Some("yes") | Some("true") | Some("1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dialogs_password_wins_over_the_environment() {
        assert_eq!(given(Some("dialog"), Some("env")).as_deref(), Some("dialog"));
        assert_eq!(given(None, Some("env")).as_deref(), Some("env"));
        assert_eq!(given(Some("dialog"), None).as_deref(), Some("dialog"));
        assert_eq!(given(None, None), None);
    }

    // an empty field is the dialog's way of saying nothing was typed, and an empty variable is how
    // a shell spells "unset": neither is a password.
    #[test]
    fn an_empty_password_is_no_password() {
        assert_eq!(given(Some(""), Some("env")).as_deref(), Some("env"));
        assert_eq!(given(Some(""), Some("")), None);
    }

    #[test]
    fn only_an_explicit_yes_accepts_an_unknown_host() {
        for yes in ["yes", "true", "1"] {
            assert!(accepts(Some(yes)), "{yes}");
        }
        for no in [None, Some(""), Some("no"), Some("0"), Some("YES")] {
            assert!(!accepts(no), "{no:?}");
        }
    }
}
