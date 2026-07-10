// Parses the command line connection target into a "host:port" address
// suitable for `TcpStream::connect`.
//
// Accepts either the legacy bare `host:port` form, or a standard URI of the
// form `protocol://host:port/args`. Only the `tcp` protocol is supported for
// now; anything else (and any path/args after the authority) is rejected or
// ignored respectively, since this client only ever talks to a single
// already-bound TCP xpra server.
pub fn parse_target(target: &str) -> Result<String, String> {
    let Some(scheme_end) = target.find("://") else {
        // no scheme: treat the whole thing as a bare host:port address.
        return Ok(target.to_string());
    };

    let scheme = &target[..scheme_end];
    if !scheme.eq_ignore_ascii_case("tcp") {
        return Err(format!("unsupported protocol {:?}: only 'tcp' is supported", scheme));
    }

    let rest = &target[scheme_end + 3..];
    let authority = match rest.find('/') {
        Some(slash) => &rest[..slash],
        None => rest,
    };
    if authority.is_empty() {
        return Err(format!("missing host:port in {:?}", target));
    }
    Ok(authority.to_string())
}
