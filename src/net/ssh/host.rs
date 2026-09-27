// Host-key verification for the native ssh transport: the `russh::client::Handler` the handshake
// calls back into, and the `known_hosts` policy behind it.
//
// The policy is OpenSSH's, minus the prompting mechanics (those are `SshPrompt`, see `mod.rs`): a
// key already in `known_hosts` is accepted silently, an unknown one is put to the user and appended
// when they accept it, and a key that *changed* is refused outright and never prompted for - that
// is either a rebuilt server or somebody sitting in the middle, and only the user can tell which.
use std::path::{Path, PathBuf};

use log::info;

use russh::client::Session;
use russh::keys::{known_hosts, HashAlg, PublicKey, PublicKeyOrCertificate};

use super::SshPrompt;

// `Handler::Error` has to absorb `russh::Error` (and `Signer::Error` `russh::SendError`), while
// everything this transport produces itself is a sentence for the user, so neither of russh's own
// error types will do on its own.
#[derive(Debug)]
pub enum SshError {
    Russh(russh::Error),
    Message(String),
}

impl From<russh::Error> for SshError {
    fn from(e: russh::Error) -> Self {
        SshError::Russh(e)
    }
}

impl From<russh::SendError> for SshError {
    fn from(_: russh::SendError) -> Self {
        SshError::Message("the ssh session ended".to_string())
    }
}

impl std::fmt::Display for SshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SshError::Russh(e) => write!(f, "{e}"),
            SshError::Message(message) => write!(f, "{message}"),
        }
    }
}

pub struct Client {
    pub host: String,
    pub port: u16,
    pub known_hosts: PathBuf,
    pub prompt: SshPrompt,
}

impl russh::client::Handler for Client {
    type Error = SshError;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let key = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key,
            // we never offer host-certificate authentication, so a server presenting one has
            // nothing we could check it against.
            PublicKeyOrCertificate::Certificate(_) => return Err(SshError::Message(
                "the server identified itself with a host certificate, which this client cannot verify".to_string())),
        };
        accept(&self.host, self.port, key, &self.known_hosts, &self.prompt).map_err(SshError::Message)
    }

    // Corporate servers almost always have a legal notice here, and it is shown before
    // authentication, so it is worth logging rather than dropping.
    async fn auth_banner(&mut self, banner: &str, _session: &mut Session) -> Result<(), Self::Error> {
        for line in banner.lines().filter(|line| !line.trim().is_empty()) {
            info!("ssh: {line}");
        }
        Ok(())
    }
}

// What `known_hosts` has to say about the key we were offered.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Known,
    Unknown,
    Changed { line: usize },
}

pub fn verdict(host: &str, port: u16, key: &PublicKey, path: &Path) -> Result<Verdict, String> {
    // no file at all is not an error: it is what a fresh account looks like.
    if !path.exists() {
        return Ok(Verdict::Unknown);
    }
    match known_hosts::check_known_hosts_path(host, port, key, path) {
        Ok(true) => Ok(Verdict::Known),
        Ok(false) => Ok(Verdict::Unknown),
        Err(russh::keys::Error::KeyChanged { line }) => Ok(Verdict::Changed { line }),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

// Near enough to OpenSSH's wording that a user who has seen it once recognises it.
pub fn question(host: &str, key: &PublicKey) -> String {
    format!("The authenticity of host '{host}' can't be established.\n\
             {} key fingerprint is {}.\n\
             Are you sure you want to continue connecting?",
            key.algorithm().as_str(), key.fingerprint(HashAlg::Sha256))
}

fn accept(host: &str, port: u16, key: &PublicKey, path: &Path, prompt: &SshPrompt)
          -> Result<bool, String> {
    match verdict(host, port, key, path)? {
        Verdict::Known => Ok(true),
        Verdict::Changed { line } => Err(format!(
            "the host key for {host} has changed: it does not match the one recorded at line {line} \
             of {}. If the server really was rebuilt, remove that line; otherwise somebody is \
             intercepting this connection.", path.display())),
        Verdict::Unknown => {
            if !(prompt.confirm)(&question(host, key)) {
                return Err(format!(
                    "the host key for {host} ({}) was not accepted. Set XPRA_SSH_ACCEPT_NEW_HOST=yes \
                     to accept it, or add it to {} yourself.",
                    key.fingerprint(HashAlg::Sha256), path.display()));
            }
            known_hosts::learn_known_hosts_path(host, port, key, path)
                .map_err(|e| format!("cannot add the host key to {}: {e}", path.display()))?;
            info!("added the host key for {host} to {}", path.display());
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use russh::keys::ssh_key::public::{Ed25519PublicKey, KeyData};

    use super::*;

    // Any 32 bytes are a well-formed ed25519 public key, which is all `known_hosts` handling cares
    // about - nothing here verifies a signature - so the tests need no key generation.
    fn key(seed: u8) -> PublicKey {
        PublicKey::new(KeyData::Ed25519(Ed25519PublicKey([seed; 32])), "")
    }

    // a path of our own per test, since these write to it. Not cleaned up on failure on purpose:
    // the file is what one would want to look at.
    fn known_hosts_file() -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let name = format!("xpra-known-hosts-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        std::env::temp_dir().join(name)
    }

    fn prompt(answer: bool) -> SshPrompt {
        SshPrompt { secret: Arc::new(|_| None), confirm: Arc::new(move |_| answer) }
    }

    #[test]
    fn a_missing_file_makes_every_host_unknown() {
        let path = known_hosts_file();
        assert_eq!(verdict("host", 22, &key(1), &path), Ok(Verdict::Unknown));
    }

    #[test]
    fn an_accepted_key_is_learned_and_then_known() {
        let path = known_hosts_file();
        assert_eq!(verdict("host", 22, &key(1), &path), Ok(Verdict::Unknown));
        assert_eq!(accept("host", 22, &key(1), &path, &prompt(true)), Ok(true));
        assert_eq!(verdict("host", 22, &key(1), &path), Ok(Verdict::Known));
        // and the second connection does not ask again, so a prompt that would decline is fine:
        assert_eq!(accept("host", 22, &key(1), &path, &prompt(false)), Ok(true));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_declined_key_is_not_learned() {
        let path = known_hosts_file();
        let error = accept("host", 22, &key(1), &path, &prompt(false)).unwrap_err();
        assert!(error.contains("XPRA_SSH_ACCEPT_NEW_HOST"), "{error}");
        assert!(error.contains("SHA256:"), "{error}");
        assert_eq!(verdict("host", 22, &key(1), &path), Ok(Verdict::Unknown));
    }

    // the whole point of the file: a different key for a host we have seen is never a prompt.
    #[test]
    fn a_changed_key_is_refused_without_asking() {
        let path = known_hosts_file();
        assert_eq!(accept("host", 22, &key(1), &path, &prompt(true)), Ok(true));
        // line 2, not 1: russh's writer opens the file in append mode and leads with a newline, so
        // the first entry it ever adds lands on the second line. The number goes into the error
        // message below, and has to be the one the user will find the entry on.
        assert_eq!(verdict("host", 22, &key(2), &path), Ok(Verdict::Changed { line: 2 }));
        let error = accept("host", 22, &key(2), &path, &prompt(true)).unwrap_err();
        assert!(error.contains("has changed"), "{error}");
        let _ = std::fs::remove_file(&path);
    }

    // the port is part of the identity: a different one is a different host entry.
    #[test]
    fn a_different_port_is_a_different_host() {
        let path = known_hosts_file();
        assert_eq!(accept("host", 22, &key(1), &path, &prompt(true)), Ok(true));
        assert_eq!(verdict("host", 2222, &key(1), &path), Ok(Verdict::Unknown));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_question_names_the_host_and_the_fingerprint() {
        let question = question("example.com", &key(7));
        assert!(question.contains("example.com"), "{question}");
        assert!(question.contains("ssh-ed25519 key fingerprint is SHA256:"), "{question}");
        assert!(question.contains("continue connecting?"), "{question}");
    }
}
