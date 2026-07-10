// Parses the command line connection target into a `Target` describing which
// transport to use and the `host:port` address to connect to.
//
// Accepts either the legacy bare `host:port` form (assumed plain tcp), or a
// standard URI of the form `protocol://host:port/args`. Only `tcp` and `ws`
// are supported for now; anything else is rejected.
#[derive(Debug, PartialEq)]
pub enum Target {
    Tcp { address: String },
    WebSocket { address: String, path: String },
}

pub fn parse_target(target: &str) -> Result<Target, String> {
    let Some(scheme_end) = target.find("://") else {
        // no scheme: treat the whole thing as a bare host:port tcp address.
        return Ok(Target::Tcp { address: target.to_string() });
    };

    let scheme = &target[..scheme_end];
    let rest = &target[scheme_end + 3..];
    let (authority, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(format!("missing host:port in {:?}", target));
    }

    if scheme.eq_ignore_ascii_case("tcp") {
        Ok(Target::Tcp { address: authority.to_string() })
    } else if scheme.eq_ignore_ascii_case("ws") {
        Ok(Target::WebSocket { address: authority.to_string(), path: path.to_string() })
    } else {
        Err(format!("unsupported protocol {:?}: only 'tcp' and 'ws' are supported", scheme))
    }
}
