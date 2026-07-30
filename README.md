<img src="assets/xpra.png" alt="Xpra logo" width="256">

# rust-xpra

Xpra client implemented in [rust](https://www.rust-lang.org/), for MS Windows and Linux.

## Status

It builds on MS Windows and Linux (X11 and Wayland).

It supports `tcp`/`ssl`/`ws`/`wss` connections, plus `ssh` (via a subprocess, see below). Password
authentication is supported (HMAC digest challenges — see [Authentication](#authentication) below).

It requires an **xpra 6.6 or later** server: every packet it sends uses the packet types introduced in
xpra 6.5, two of which (`clipboard-data`, and the argument order of `window-draw-ack`) only settled in
6.6. The server must be left in its default backwards-compatible mode, since the packets the client
*receives* are still the pre-6.5 ones.

There is no server implementation. Plain-text clipboard synchronization is supported, as is automatic
server-to-client speaker forwarding on Windows.

On MS Windows there is a system tray icon with an **Exit** menu entry, and server-forwarded
notifications are shown as balloons on it — see [System tray](#system-tray). Elsewhere notifications are only
written to the client log.

### Windows speaker forwarding

On Windows 10 and later, server audio is enabled automatically when the system Media Foundation Opus decoder and
the default WASAPI output endpoint are available. The client negotiates only the bare `opus` codec (no Matroska
or Ogg container), receives audio asynchronously, and renders it through a bounded adaptive jitter buffer. If
the native probe or output-device recovery fails, audio is disabled for that session without disconnecting it.

Speaker forwarding is receive-only in this milestone: microphone forwarding, non-Opus codecs, and audio output
on Linux/macOS are not implemented.

### Known Linux limitations

The windowing/painting layer is built on [winit](https://github.com/rust-windowing/winit) +
[softbuffer](https://github.com/rust-windowing/softbuffer), which run on both platforms, but the Wayland protocol
itself does not let clients query or set their absolute desktop position:
- Override-redirect windows (used by the server for tooltips/menus/dropdowns) have no Wayland equivalent at all,
  and degrade to undecorated, non-resizable but still WM-managed windows (visible in taskbars/window-switchers,
  unlike true override-redirect).
- Server-initiated window moves (`window-move-resize`) only apply the size on Wayland; the position component is
  silently skipped.
- Outgoing window geometry (`window-map`/`window-configure`) reports `(0, 0)` as the position on Wayland, since
  there is no OS API to query it.
- NumLock state is not reported to the server (winit does not expose toggle/lock key state, only held
  modifiers).

Running under XWayland (the X11 backend) instead of native Wayland avoids all of the above.

Server-forwarded bells (`bell`) play a real tone on Windows, but on Linux fall back to writing the terminal
bell (`^G`) to stderr - there is no portable desktop bell without an X11 or audio-server dependency, which this
client avoids - so a bell is only audible when the client was started from a terminal whose bell is enabled.

There is no system tray icon on Linux either (see [System tray](#system-tray) below): the freedesktop
StatusNotifierItem protocol needs a D-Bus dependency, and the older XEmbed tray is X11-only. Server-forwarded
notifications are shown as balloons on that tray icon, so they too are Windows-only and are merely logged here.

## Usage

```shell
cargo build
./target/debug/xpra                 # asks for the connection details, see below
./target/debug/xpra HOST:PORT
./target/debug/xpra tcp://HOST:PORT/
./target/debug/xpra ssl://HOST:PORT/
./target/debug/xpra ws://HOST:PORT/
./target/debug/xpra wss://HOST:PORT/
./target/debug/xpra ssh://[USER@]HOST[:PORT]/[DISPLAY]
```

Started **without any argument**, the client opens a small connection dialog instead of exiting: a protocol
drop-down (which pre-fills the port with that protocol's default — 10000, or 22 for `ssh`), a host, a port, and
an optional username and password, plus **Cancel** and **Connect**. `Tab` moves between the fields, the arrow
keys pick the protocol, `Enter` connects and `Esc` cancels (exit status `0`). The connection is made in the
background, so the dialog stays responsive and reports a failure (wrong port, no server, bad certificate, ...)
in place, ready for another attempt, rather than exiting.

The username is only part of the connection itself for `ssh` (`ssh://USER@HOST/`); for the other protocols it is
sent in the client's `hello`, which is what a server authenticating per-user matches against. The password is
the *session* password — it answers the server's authentication challenge (see [Authentication](#authentication))
without prompting again — and never the ssh password: `ssh` asks for its own credentials on the terminal.

Like the password prompt, the dialog is drawn with the same `winit`/`softbuffer` stack as the rest of the client
(there is no widget toolkit here — windows are server-rendered pixels), with its text blitted from a bundled
bitmap font: [Spleen](https://github.com/fcambus/spleen) 8x16, Copyright (c) 2018-2026, Frederic Cambus,
BSD-2-Clause.

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

## System tray

On MS Windows the client puts an icon in the notification area for as long as it is connected. Right-clicking it
opens a menu naming the session (`xpra @ HOST:PORT`) with an **Exit** entry, which is the only way to shut the
client down from the GUI — closing a window only tells the server to close that window. Exiting this way sends a
`disconnect` to the server first and exits with status `0`.

The icon is `assets/xpra.ico`, embedded into the executable by `build.rs` (which also embeds `exe.manifest`, the
per-monitor-V2 DPI manifest, so it is the executable's icon in Explorer too). It is implemented directly on
`Shell_NotifyIconW` through the `windows` crate that Media Foundation already requires, so it adds no new
dependency and no extra thread — the tray window is created on the UI thread and winit's own message loop pumps
it.

### Notifications

The same icon also carries **desktop notifications**: a notification forwarded by the server becomes a balloon
on the tray icon, with the notification's summary as the title and its body as the text. This needs no notifier
library — it is one more `Shell_NotifyIconW` call — which is why it is Windows-only. On Windows 10 and later the
shell renders these as toasts, so they follow the user's notification settings and Focus assist, and may land in
the Action Center instead of appearing on screen.

Only the text is used: notification *actions* (buttons) and hints are ignored, as are per-notification icons and
the server's expiry timeout (Windows has ignored the requested balloon timeout since Vista, in favour of the
system accessibility timeout). A notification the server withdraws is taken back, if it is still the one being
shown.

There is no tray, and therefore no notifications, on Linux or macOS — the client just logs them (both platforms
would need a D-Bus dependency this client avoids).

## Authentication

Servers that require a password (xpra's `password`/`file`/`multifile`/`sqlite`/`pam`/... auth modules) send a
`challenge`; the client answers it with an HMAC-SHA256 digest of the password. When a challenge arrives, the
password is obtained from — in order:

1. the connection dialog's password field, when the client was started without arguments and it was filled in;
2. the `XPRA_PASSWORD` environment variable, if set (non-interactive; handy for scripts);
3. [`pinentry`](https://www.gnupg.org/related_software/pinentry/), if one is found on `PATH` (honouring
   `PINENTRY_PROGRAM`) — the same native, secure prompt GnuPG uses (GTK/Qt/curses on Linux, `pinentry-mac` on
   macOS);
4. otherwise a small built-in password dialog (drawn with the same `winit`/`softbuffer` stack as the rest of the
   client), so a prompt is always available — including on Windows, where `pinentry` is normally absent.

Only the `hmac+sha256` digest is implemented (it is the only one advertised, so the server always picks it);
Kerberos/GSS/SCRAM/U2F and the legacy `xor`/`des` digests are not supported and fail cleanly. The HMAC response
never reveals the password itself, and the server mixes in a fresh per-connection salt so a captured response
cannot be replayed — but the session payload is still in the clear over `tcp`/`ws`, so use `ssl`/`wss` (or `ssh`)
for confidentiality.

## Picture encodings

`jpeg` (libjpeg-turbo), `png` (libspng), `webp` (libwebp), and `h264` on Windows only (decoded by the OS through
Media Foundation, no codec is bundled).

### Linking libwebp

By default `libwebp` is built from the vendored C sources and linked **statically**, so that the release binaries
are self-contained — this needs no extra tooling (a C compiler only: no `cmake`, no `nasm`, no `bindgen`).

Distribution packages generally must not bundle their own copy of a library the distribution already ships and has
to be able to patch, so there is a feature to link the system `libwebp` shared library instead (located with
`pkg-config`):

```shell
cargo build --release --features webp-dylib
```
