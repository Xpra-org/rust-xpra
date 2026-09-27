// Authentication for the native ssh transport, in OpenSSH's order: the agent first, then the key
// files under ~/.ssh, then keyboard-interactive, then a plain password.
//
// Two rules keep this from misbehaving where the system `ssh` would not. A method the server did
// not offer is skipped, so a password-only server does not make us read key files it will refuse;
// and every prompt is asked at most once, because `SshPrompt` is one-shot by contract (see
// `client/ask.rs`) and asking again would either loop on the same wrong answer or, with pinentry,
// pester the user for a password the server has already rejected.
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use log::{debug, info, warn};

use russh::client::{AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{load_secret_key, HashAlg, PrivateKeyWithHashAlg};
use russh::{MethodKind, MethodSet, Signer};

use super::host::Client;
use super::SshPrompt;

// The private key to use, instead of looking under ~/.ssh.
pub const KEY_ENV: &str = "XPRA_SSH_KEY";

pub async fn authenticate(handle: &mut Handle<Client>, user: &str, prompt: &SshPrompt)
                          -> Result<(), String> {
    // "none" is not really an authentication method, it is the question "which ones do you take?":
    // the failure it draws lists them, which is what lets everything below skip what cannot work.
    // A server with no authentication at all answers success.
    let mut offered = match handle.authenticate_none(user).await {
        Ok(AuthResult::Success) => return Ok(()),
        Ok(AuthResult::Failure { remaining_methods, .. }) => remaining_methods,
        Err(e) => return Err(format!("ssh authentication failed: {e}")),
    };
    debug!("the server accepts {} for {user}", methods(&offered));
    let mut tried: Vec<String> = Vec::new();

    if offered.contains(&MethodKind::PublicKey) {
        if let Some(mut agent) = agent().await {
            match agent.0.request_identities().await {
                Ok(identities) => {
                    for identity in identities {
                        let name = describe(&identity);
                        tried.push(format!("agent ({name})"));
                        match with_agent(handle, user, &mut agent, &identity).await {
                            Ok(AuthResult::Success) => return succeeded(&format!("agent key {name}")),
                            Ok(AuthResult::Failure { remaining_methods, .. }) => offered = remaining_methods,
                            Err(e) => warn!("the agent could not sign with {name}: {e}"),
                        }
                    }
                }
                Err(e) => warn!("cannot list the ssh agent's keys: {e}"),
            }
        }
        for path in identity_paths(home().as_deref(), env::var(KEY_ENV).ok().as_deref()) {
            if !path.exists() {
                continue;
            }
            tried.push(path.display().to_string());
            let key = match secret_key(&path, prompt) {
                Ok(key) => key,
                Err(e) => {
                    warn!("cannot use {}: {e}", path.display());
                    continue;
                }
            };
            match handle.authenticate_publickey(user, key).await {
                Ok(AuthResult::Success) => return succeeded(&path.display().to_string()),
                Ok(AuthResult::Failure { remaining_methods, .. }) => offered = remaining_methods,
                Err(e) => return Err(format!("ssh authentication failed: {e}")),
            }
        }
    }

    if offered.contains(&MethodKind::KeyboardInteractive) {
        tried.push("keyboard-interactive".to_string());
        match interactive(handle, user, prompt).await? {
            Some(remaining) => offered = remaining,
            None => return succeeded("keyboard-interactive"),
        }
    }

    if offered.contains(&MethodKind::Password) {
        tried.push("password".to_string());
        if let Some(password) = (prompt.secret)(&format!("{user}'s password")) {
            match handle.authenticate_password(user, password).await {
                Ok(AuthResult::Success) => return succeeded("password"),
                Ok(AuthResult::Failure { .. }) => {}
                Err(e) => return Err(format!("ssh authentication failed: {e}")),
            }
        }
    }

    Err(if tried.is_empty() {
        format!("ssh authentication failed: no method this client supports is accepted (the server \
                 offers {})", methods(&offered))
    } else {
        format!("ssh authentication failed (tried: {})", tried.join(", "))
    })
}

fn succeeded(how: &str) -> Result<(), String> {
    info!("ssh authenticated with {how}");
    Ok(())
}

fn methods(offered: &MethodSet) -> String {
    if offered.is_empty() {
        return "nothing".to_string();
    }
    offered.iter().map(|method| <&str>::from(method)).collect::<Vec<_>>().join(", ")
}

// A key file, with one passphrase prompt if it turns out to be encrypted. Anything else - a key we
// cannot parse, a file we cannot read - is the caller's cue to move on to the next candidate.
fn secret_key(path: &Path, prompt: &SshPrompt) -> Result<PrivateKeyWithHashAlg, String> {
    let key = match load_secret_key(path, None) {
        Err(russh::keys::Error::KeyIsEncrypted) => {
            let passphrase = (prompt.secret)(&format!("passphrase for {}", path.display()))
                .ok_or("no passphrase given")?;
            load_secret_key(path, Some(&passphrase)).map_err(|e| e.to_string())?
        }
        other => other.map_err(|e| e.to_string())?,
    };
    // `new` ignores the hash algorithm for everything but RSA, where it is the difference between
    // the SHA-512 signature modern servers want and the SHA-1 one they refuse.
    Ok(PrivateKeyWithHashAlg::new(Arc::new(key), Some(HashAlg::Sha512)))
}

// `Ok(None)` means authenticated; `Ok(Some(methods))` that the server said no and what it will
// still consider.
async fn interactive(handle: &mut Handle<Client>, user: &str, prompt: &SshPrompt)
                     -> Result<Option<MethodSet>, String> {
    let mut response = handle.authenticate_keyboard_interactive_start(user, None).await
        .map_err(|e| format!("ssh authentication failed: {e}"))?;
    // A server may send any number of info requests, including empty ones (they carry banners), so
    // this has to loop - but only a handful of times, and only one round may ask for a secret.
    let mut asked = false;
    for _ in 0..8 {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(None),
            KeyboardInteractiveAuthResponse::Failure { remaining_methods, .. } =>
                return Ok(Some(remaining_methods)),
            KeyboardInteractiveAuthResponse::InfoRequest { instructions, prompts, .. } => {
                for line in instructions.lines().filter(|line| !line.trim().is_empty()) {
                    info!("ssh: {line}");
                }
                let mut answers = Vec::with_capacity(prompts.len());
                for question in &prompts {
                    if asked {
                        return Err("ssh authentication failed: the server asked more than once, \
                                    and there is only one password to give".to_string());
                    }
                    asked = true;
                    let answer = (prompt.secret)(question.prompt.trim_end())
                        .ok_or("ssh authentication failed: no password given")?;
                    answers.push(answer);
                }
                response = handle.authenticate_keyboard_interactive_respond(answers).await
                    .map_err(|e| format!("ssh authentication failed: {e}"))?;
            }
        }
    }
    Err("ssh authentication failed: the server kept asking".to_string())
}

// The candidate key files, most-preferred first. `XPRA_SSH_KEY` replaces the list rather than
// extending it: it is there to say "this key, not whatever else is lying around".
fn identity_paths(home: Option<&Path>, pinned: Option<&str>) -> Vec<PathBuf> {
    if let Some(path) = pinned.filter(|path| !path.is_empty()) {
        return vec![PathBuf::from(path)];
    }
    let Some(home) = home else {
        return Vec::new();
    };
    ["id_ed25519", "id_ecdsa", "id_rsa"].iter().map(|name| home.join(".ssh").join(name)).collect()
}

pub fn home() -> Option<PathBuf> {
    // `$HOME` on Unix and `%USERPROFILE%` on Windows - the same two `ssh` itself looks at, and no
    // new crate to read them. The order matters on Windows: a Git Bash / MSYS shell exports a
    // `HOME` holding a POSIX path (`/c/Users/name`) that this process cannot open at all, while
    // `USERPROFILE` is always the real directory - and is what OpenSSH for Windows uses.
    let names: &[&str] = if cfg!(windows) { &["USERPROFILE", "HOME"] } else { &["HOME"] };
    names.iter()
        .filter_map(|name| env::var_os(name))
        .map(PathBuf::from)
        .find(|path| !path.as_os_str().is_empty())
}

// russh declares the `Signer` trait for agent-backed public-key authentication but implements it
// for nothing, so this is that implementation: one call through to the agent. The stream type is
// erased (`dynamic`) so that the Unix socket and the Windows named pipe are one type here.
struct Agent(AgentClient<Box<dyn AgentStream + Send + Unpin>>);

#[derive(Debug)]
struct AgentError(String);

impl From<russh::SendError> for AgentError {
    fn from(_: russh::SendError) -> Self {
        AgentError("the ssh session ended".to_string())
    }
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Signer for Agent {
    type Error = AgentError;

    async fn auth_sign(&mut self, key: &AgentIdentity, hash_alg: Option<HashAlg>, to_sign: Vec<u8>)
                       -> Result<Vec<u8>, Self::Error> {
        self.0.sign_request(key, hash_alg, to_sign).await.map_err(|e| AgentError(e.to_string()))
    }
}

async fn with_agent(handle: &mut Handle<Client>, user: &str, agent: &mut Agent,
                    identity: &AgentIdentity) -> Result<AuthResult, AgentError> {
    let AgentIdentity::PublicKey { key, .. } = identity else {
        // certificates would need `authenticate_certificate_with` and a server that asked for one
        return Err(AgentError("an agent certificate, which is not supported".to_string()));
    };
    // RSA again: an agent will sign with SHA-1 unless asked otherwise, and that is refused by
    // anything recent.
    let hash_alg = key.algorithm().is_rsa().then_some(HashAlg::Sha512);
    handle.authenticate_publickey_with(user, key.clone(), hash_alg, agent).await
}

fn describe(identity: &AgentIdentity) -> String {
    match identity {
        AgentIdentity::PublicKey { comment, .. } if !comment.is_empty() => comment.to_string(),
        AgentIdentity::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256).to_string(),
        AgentIdentity::Certificate { .. } => "a certificate".to_string(),
    }
}

#[cfg(unix)]
async fn agent() -> Option<Agent> {
    match AgentClient::connect_env().await {
        Ok(agent) => Some(Agent(agent.dynamic())),
        Err(e) => {
            debug!("no ssh agent: {e}");
            None
        }
    }
}

#[cfg(windows)]
async fn agent() -> Option<Agent> {
    // OpenSSH for Windows runs its agent on a named pipe, and when `SSH_AUTH_SOCK` is set at all it
    // names that pipe rather than a socket.
    let path = env::var("SSH_AUTH_SOCK")
        .unwrap_or_else(|_| r"\\.\pipe\openssh-ssh-agent".to_string());
    match AgentClient::connect_named_pipe(&path).await {
        Ok(agent) => Some(Agent(agent.dynamic())),
        Err(e) => {
            debug!("no ssh agent on {path}: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_openssh_defaults_are_tried_in_order() {
        let paths = identity_paths(Some(Path::new("/home/user")), None);
        let names: Vec<String> =
            paths.iter().map(|path| path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["id_ed25519", "id_ecdsa", "id_rsa"]);
        assert!(paths[0].ends_with("user/.ssh/id_ed25519") || paths[0].ends_with(r"user\.ssh\id_ed25519"),
                "{:?}", paths[0]);
    }

    #[test]
    fn a_pinned_key_replaces_the_defaults() {
        let paths = identity_paths(Some(Path::new("/home/user")), Some("/keys/deploy"));
        assert_eq!(paths, vec![PathBuf::from("/keys/deploy")]);
    }

    // an unset variable reads back as an empty string often enough to be worth ignoring, and
    // "no home directory" has to be a short list rather than a panic.
    #[test]
    fn nothing_to_look_at_is_an_empty_list() {
        assert_eq!(identity_paths(Some(Path::new("/home/user")), Some("")).len(), 3);
        assert_eq!(identity_paths(None, None), Vec::<PathBuf>::new());
        assert_eq!(identity_paths(None, Some("/keys/deploy")), vec![PathBuf::from("/keys/deploy")]);
    }
}
