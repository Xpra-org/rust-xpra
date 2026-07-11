// Parses the command line connection target into a `Target` describing which
// transport to use and the `host:port` address to connect to.
//
// Accepts either the legacy bare `host:port` form (assumed plain tcp), or a
// standard URI of the form `protocol://host:port/args`. Only `tcp`, `ssl`,
// `ws` and `wss` are supported for now; anything else is rejected.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Scheme {
    Tcp,
    Tls,
    WebSocket,
    WebSocketTls,
}

#[derive(Debug, PartialEq)]
pub struct Target {
    pub scheme: Scheme,
    pub address: String,
    pub path: String,
}

pub fn parse_target(target: &str) -> Result<Target, String> {
    let Some(scheme_end) = target.find("://") else {
        // no scheme: treat the whole thing as a bare host:port tcp address.
        return Ok(Target { scheme: Scheme::Tcp, address: target.to_string(), path: String::new() });
    };

    let scheme_str = &target[..scheme_end];
    let rest = &target[scheme_end + 3..];
    let (authority, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(format!("missing host:port in {:?}", target));
    }

    let scheme = if scheme_str.eq_ignore_ascii_case("tcp") {
        Scheme::Tcp
    } else if scheme_str.eq_ignore_ascii_case("ssl") {
        Scheme::Tls
    } else if scheme_str.eq_ignore_ascii_case("ws") {
        Scheme::WebSocket
    } else if scheme_str.eq_ignore_ascii_case("wss") {
        Scheme::WebSocketTls
    } else {
        return Err(format!("unsupported protocol {:?}: only 'tcp', 'ssl', 'ws' and 'wss' are supported", scheme_str));
    };
    Ok(Target { scheme, address: authority.to_string(), path: path.to_string() })
}

// splits a "host:port" address into just the host part, for use as the TLS
// SNI/hostname argument. Handles bracketed IPv6 addresses (e.g. "[::1]:1234")
// correctly since the brackets stay attached to the host part either way.
pub fn host_only(address: &str) -> &str {
    match address.rsplit_once(':') {
        Some((host, _port)) => host,
        None => address,
    }
}
