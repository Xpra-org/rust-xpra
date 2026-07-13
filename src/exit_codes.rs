// Process exit codes, mirroring xpra's own `ExitCode` (see `xpra/exit_codes.py` upstream) so that
// anything wrapping this client sees the same values as with the python client.
// Only the subset we can actually produce is listed here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    Ok = 0,
    // the connection dropped after the session was established
    ConnectionLost = 1,
    Failure = 7,
    SshFailure = 8,
    // an unusable / unparseable packet from an established session
    PacketFailure = 9,
    InternalError = 14,
    SslFailure = 16,
    // we never got a working session: refused / unreachable / not an xpra server
    ConnectionFailed = 18,
    AuthenticationFailed = 28,
    // bad command line
    ArgumentMismatch = 34,
}

impl ExitCode {
    pub fn value(self) -> i32 {
        self as i32
    }
}
