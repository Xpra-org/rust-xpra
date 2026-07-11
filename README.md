# rust-xpra
Xpra client implemented in [rust](https://www.rust-lang.org/), for MS Windows and Linux.

## Status

This is a proof of concept only and is not usable at this point.

It builds on MS Windows and Linux (X11 and Wayland).

It only supports unauthenticated `tcp`/`ssl`/`ws`/`wss` connections, plus `ssh` (via a subprocess, see below).

No server, no audio, no clipboard, no notifications, etc..

### Known Linux limitations

The windowing/painting layer is built on [winit](https://github.com/rust-windowing/winit) +
[softbuffer](https://github.com/rust-windowing/softbuffer), which run on both platforms, but the Wayland protocol
itself does not let clients query or set their absolute desktop position:
- Override-redirect windows (used by the server for tooltips/menus/dropdowns) have no Wayland equivalent at all,
  and degrade to undecorated, non-resizable but still WM-managed windows (visible in taskbars/window-switchers,
  unlike true override-redirect).
- Server-initiated window moves (`window-move-resize`) only apply the size on Wayland; the position component is
  silently skipped.
- Outgoing window geometry (`map-window`/`configure-window`) reports `(0, 0)` as the position on Wayland, since
  there is no OS API to query it.
- NumLock state is not reported to the server (winit does not expose toggle/lock key state, only held
  modifiers).

Running under XWayland (the X11 backend) instead of native Wayland avoids all of the above.

## Usage

```shell
cargo build
./target/debug/xpra HOST:PORT
./target/debug/xpra tcp://HOST:PORT/
./target/debug/xpra ssl://HOST:PORT/
./target/debug/xpra ws://HOST:PORT/
./target/debug/xpra wss://HOST:PORT/
./target/debug/xpra ssh://[USER@]HOST[:PORT]/[DISPLAY]
```

Only the `tcp`, `ssl`, `ws`, `wss` and `ssh` protocols are supported; any other protocol in the URI is rejected.
`ws` support (HTTP upgrade handshake and frame framing) is hand-rolled against `std` only, to avoid pulling in a
websocket crate and its dependencies. `ssl`/`wss` use [`native-tls`](https://docs.rs/native-tls) (OpenSSL on
Linux, Schannel on Windows, Security.framework on macOS) rather than a hand-rolled implementation, since TLS
encryption (unlike WebSocket masking) is a real security boundary. **Certificate verification is currently
disabled** (self-signed/invalid certificates are accepted) since this client has no way to configure a custom CA
yet; `ssl`/`wss` connections are not authenticated against a trusted CA and are vulnerable to interception.

`ssh` shells out to the system `ssh` binary and uses its stdin/stdout pipes as the byte stream (no SSH library
dependency), running `xpra _proxy [DISPLAY]` on the remote end — the same mechanism xpra's own client uses to
bridge stdin/stdout to an existing display's socket. This requires a working `ssh` in `PATH` (OpenSSH on Linux,
or the bundled OpenSSH client on Windows 10 1809+) and `xpra` installed on the remote host. Authentication must
not require interactive input on stdin (stdin carries the xpra protocol, not a terminal), so use key-based auth
via an ssh-agent or a passphrase-less key; host-key confirmation and password prompts still work normally since
OpenSSH reads those from the controlling terminal, not stdin.
