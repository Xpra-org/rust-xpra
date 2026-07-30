use std::{str};

// The xpra *protocol* version we claim in the hello packet, which is what the server checks for
// compatibility - deliberately not this crate's own version below.
pub const VERSION: &str = "6.4";

// This client's own version, as reported by `--version`. Read from Cargo.toml so that the package
// version is the single place to bump (the man page header and debian/changelog follow by hand).
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const VERSION_KEY_STR: &str = "version";

pub mod exit_codes;
pub mod net;
