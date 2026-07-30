# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Proof-of-concept [Xpra](https://xpra.org/) client written in Rust, for MS Windows and Linux (X11 and Wayland).
Not usable yet: `tcp`/`ssl`/`ws`/`wss` connections (and `ssl`/`wss` don't verify certificates yet — see
`README.md`), plus `ssh` (via a subprocess); no server/audio/clipboard support. Password
authentication *is* supported (the `hmac+sha256` challenge digest only — see the `challenge` flow below and
`README.md`). See `README.md` for known Linux/Wayland limitations (window positioning, override-redirect, NumLock
— all downstream of Wayland not letting clients query/set absolute desktop position or create truly unmanaged
windows). MS Windows has a system tray icon with an `Exit` menu entry, which doubles as the notification
backend (server notifications become tray balloons — `src/client/tray.rs`); Linux has no tray
(StatusNotifierItem would need D-Bus, the XEmbed tray is X11-only) and therefore only logs notifications.

## Build / run

Cross-platform: builds on Windows and Linux via `winit` + `softbuffer` (no native GTK/Qt dependency). CI builds
both (see `.github/workflows/rust.yml`). On Linux, building needs `pkg-config` and X11/Wayland/xkbcommon dev
headers (see the `build-linux` job for the exact package list). Windows builds additionally want a resource
compiler on `PATH` (`rc.exe` from the Windows SDK, which the CI runners have) to embed the icon and manifest —
see `build.rs` under "Known repo quirks"; without one the build still succeeds, just with a warning.

```shell
cargo build
./target/debug/xpra HOST:PORT     # xpra.exe on Windows
./target/debug/xpra wss://HOST:PORT/
./target/debug/xpra               # no argument: ask for the details (src/client/connect_dialog.rs)
./target/debug/xpra --help        # or -h: the target forms and the environment variables
./target/debug/xpra --version     # `CLIENT_VERSION` only (= the crate version)
```

`src/lib.rs` holds both versions and they must not be conflated: `VERSION` ("6.4") is the *xpra
protocol* version announced to the server in the hello, `CLIENT_VERSION` is this crate's own
(`env!("CARGO_PKG_VERSION")`) and is the only one `--version` prints.

`.cargo/config.toml` sets `TURBOJPEG_SOURCE=pkg-config` + `TURBOJPEG_STATIC=1` so that `turbojpeg` links against
the system libjpeg-turbo (needs its dev headers, and version >= 3.0) instead of compiling `turbojpeg-sys`' vendored
copy. Cargo's `[env]` does not override variables already set in the process environment, so both CI workflows set
`TURBOJPEG_SOURCE=vendor` in their top-level `env:` block to opt back into the vendored build — the GitHub runners
have no libjpeg-turbo 3.x (and the Windows ones no `pkg-config`). Same escape hatch locally: set
`TURBOJPEG_SOURCE=vendor` in the environment (requires `cmake` + `nasm`) if the system libjpeg-turbo is too old.

`libwebp-sys` is the opposite default: it builds its **vendored** libwebp C sources with `cc` and links them
statically, which is what the all-in-one CI release binaries want and needs no extra tooling (no `cmake`/`nasm`/
`bindgen` — the bindings are pre-generated). Downstream packagers who must link the distro's shared libwebp
instead build with `--features webp-dylib` (see `Cargo.toml`), which is what the RPM and DEB builds under
`packaging/` do; it turns on `libwebp-sys/system-dylib` and makes
its `build.rs` `pkg-config`-probe the system library rather than compiling the vendored copy. The two modes are
interchangeable: the FFI surface is identical, so no code outside `Cargo.toml` is conditional on the feature.

There are no automated tests for the GUI/protocol dispatch layer — verify changes manually against a real Xpra
server (`xpra start :100 --bind-tcp=127.0.0.1:PORT --auth=none --tcp-auth=none` works well for local testing).
What *is* covered runs under **`cargo test`**, not `cargo test --lib`: the lib target only holds `net/`, so
`--lib` skips everything in `src/client/` (the audio jitter buffer, the connect dialog's URI building, window
metadata parsing, the logger's date arithmetic) and runs three of the tests instead of all of them.

## Architecture

The crate has both a library part (`xpra`, `src/lib.rs`) and a binary (`src/main.rs`).

- `src/lib.rs` / `src/net/`: the Xpra wire-protocol layer, platform-independent.
  - `net/uri.rs`: `parse_target` turns the command-line argument into a `Target { scheme, address, path, username }`,
    accepting either a bare `host:port` (assumed `tcp`) or a `protocol://host:port/path` URI. `Scheme` is one of
    `Tcp`/`Tls`/`WebSocket`/`WebSocketTls`/`Ssh` (`tcp`/`ssl`/`ws`/`wss`/`ssh`); anything else is rejected.
    `host_only` strips the port for use as the TLS SNI/hostname argument (handles bracketed IPv6 correctly).
    `Ssh` alone allows a `user@` authority prefix (→ `username`) and defaults the port to 22 if omitted (the other
    schemes always require an explicit port); its `path` is stripped of the leading `/` since it's passed through
    as a bare xpra display number, not a URI path.
  - `net/connection.rs`: `Connection` is an enum (`Tcp`/`Tls`/`WebSocket`/`WebSocketTls`/`Ssh`) implementing
    `Read`/`Write` by dispatching to whichever transport is active, so the rest of the codebase (`io.rs`,
    `XpraClient`) doesn't need to know which one it's talking to. `try_clone()` gives the reader thread its own
    independent instance. `Connection::write_all` special-cases `Tls` to call `SharedTlsStream::write_all` (see
    below) rather than the generic `Write::write_all`.
  - `net/tls.rs`: `ssl://`/`wss://`, via `native-tls` (OpenSSL/Schannel/Security.framework) rather than a
    hand-rolled implementation — unlike WebSocket masking, TLS encryption is a real security boundary.
    **Certificate verification is currently disabled** (`danger_accept_invalid_certs`/`danger_accept_invalid_hostnames`)
    since there's no way to configure a custom CA yet; see `README.md`. `SharedTlsStream` wraps the single
    `TlsStream` in an `Arc<Mutex<_>>` so the reader thread and the UI thread (writer) can share it — a single TLS
    session isn't safe for concurrent use by two threads (see xpra's own `SSLSocketConnection`, which hits the
    same OpenSSL issue). The socket is put in **true non-blocking mode**, not a read *timeout* on an otherwise-
    blocking socket: OpenSSL only guarantees a safe retry after `WouldBlock` for a genuinely non-blocking
    transport. `SharedTlsStream::write_all` holds the lock for the *whole* write (including across `WouldBlock`
    retries), so a concurrent writer (the reader thread's automatic pong reply to a WebSocket ping, over `wss://`)
    can never interleave its bytes into the middle of another writer's frame — this was a real, reproducible bug
    (moving the mouse/typing while `wss://`-connected corrupted the packet stream) before both fixes landed.
  - `net/websocket.rs`: a minimal hand-rolled RFC 6455 client (HTTP upgrade handshake + frame masking/framing) —
    deliberately not using a websocket crate, since the protocol needed here is small; see `README.md`. Generic
    over the underlying stream (`TcpStream` for `ws://`, `SharedTlsStream` for `wss://`) via the `CloneableStream`
    trait, whose `write_frame` is what routes TLS writes through the atomic `SharedTlsStream::write_all` above.
    Requires a `Sec-WebSocket-Protocol: binary` header for the xpra server to accept the upgrade.
    `WebSocketStream` buffers one reassembled (defragmented) message at a time and serves it through `Read`,
    transparently answering pings.
  - `net/sha1.rs`: a self-contained SHA1 (only used for the websocket accept-hash, not security-sensitive) — has
    unit tests with the standard RFC 3174 test vectors, run via `cargo test`.
  - `net/sha256.rs`: a self-contained SHA-256 + HMAC-SHA256, used to answer the server's password `challenge`
    (see the client's `process_challenge`). Unlike `sha1` this *is* a security boundary, so it is verified against
    the FIPS-180 and RFC 4231 test vectors (`cargo test`). Hand-rolled rather than pulling in a crypto crate,
    matching the rest of `net/`; `hmac_sha256_hex` returns the lowercase-hex ASCII form xpra puts on the wire.
  - `net/ssh.rs`: `ssh://`, implemented by shelling out to the system `ssh` binary (`std::process::Command`) and
    treating its stdin/stdout pipes as the byte stream — no SSH library dependency, mirroring the `tcp`/`ws`
    hand-rolled-over-a-library preference here (a full client like `russh` costs ~2MB and needs `tokio`; see the
    dependency-cost notes in git history). The remote command is `sh -c 'if command -v "xpra" ...; then xpra
    _proxy [DISPLAY]; else ...; fi'`, matching what xpra's own client runs over ssh (see
    `xpra/net/ssh/exec_client.py:get_ssh_command` upstream) — `xpra _proxy` bridges stdin/stdout on the remote end
    to the target display's existing unix-domain socket. `SshStream` wraps `ChildStdin`/`ChildStdout` each in
    their own `Arc<Mutex<_>>` purely so `try_clone()` can hand the reader thread its own handle; unlike
    `SharedTlsStream` these two mutexes are never actually contended, since stdin and stdout are independent pipes
    (only the UI thread ever locks `stdin`, only the reader thread ever locks `stdout`). The spawned `Child` is
    moved into a dedicated reaper thread that blocks on `child.wait()`, since `Child::drop` neither kills nor
    waits on the process and would otherwise leave a zombie once ssh exits. Authentication must not require
    interactive stdin (it carries the xpra protocol); host-key/password prompts still work since OpenSSH reads
    those from the controlling terminal, not stdin — ssh's stderr is inherited so such prompts/errors are visible.
  - `net/io.rs`: packet framing over a `Connection` — 8-byte header (`'P'` magic, flags byte where bit 2 must be
    `FLAGS_YAML`, compression byte, chunk byte, 4-byte big-endian payload length) followed by the payload, written
    as a single `Connection::write_all` call. We write only YAML-encoded, uncompressed, unchunked packets (the
    header's compression/chunk bytes are always 0 outbound), and reject a non-YAML main packet on read.
    **Out-of-band chunks are supported inbound** (hello capability `chunks: true`): rather than base64-inlining a
    large binary item into the YAML payload, the server sends it as its own packet whose header's chunk byte is
    the index of the packet field it belongs to — pixel data (index 7 of a `draw`), window icons, cursors. The
    chunks come first, the main packet last with index 0, so `read_packet` loops until it sees index 0 and returns
    a `RawPacket { payload, chunks }`; `serde::parse_packet` moves the chunks into `Packet.raw`, where `get_bytes`
    reads them in preference to the empty placeholder the sender left in the YAML. A chunk header names no packet
    encoder (its flags byte is 0), so the `FLAGS_YAML` check applies to the main packet only — which also sets
    `FLAGS_FLUSH` (0x8) alongside it, hence a mask rather than an equality test. The limits mirror xpra's own
    receive loop (`process_payload`): chunk index < 16, at most 4 chunks, no duplicate index.
    **Inbound lz4 is supported**, too: the client advertises `compressors=["lz4"]` + a non-zero
    `compression_level` in its hello (see `send_hello`), so the server compresses its packets to us — including,
    right away, the large hello reply. When the header's compression byte is non-zero, `read_packet` decompresses
    the payload before returning it (`decompress`): the algorithm is in the byte's high bits (`0x10`=lz4,
    `0x40`=brotli, `0x80`=zstd, low nibble = level; xpra `net/protocol/header.py`) and only lz4 is accepted, since
    it's the only compressor we advertise. A chunk can carry its own compression the same way (xpra's
    `LevelCompressed`); an unsupported algorithm there is *not* fatal — the chunk is dropped with a warning and
    the field reads back empty, because the server brotli-compresses clipboard payloads over ~380 bytes without
    negotiating it (`server/source/clipboard.py`), and losing a paste beats losing the session. xpra's lz4 framing
    is a 4-byte little-endian uncompressed-size prefix +
    a raw lz4 block, which is exactly `lz4_flex`'s size-prepended block format (pure-Rust, `default-features` off
    so no xxhash/frame dependency; `safe-decode` for memory safety on adversarial input). Outbound packets stay
    uncompressed — they're small input events, all below the server's `MIN_COMPRESS_SIZE`, so there's nothing to
    gain and no compressor is linked for the write path.
  - `net/serde.rs`: `parse_packet` turns a `RawPacket` into a `Packet` — the YAML payload gives the positional
    fields, the out-of-band chunks become `raw`.
  - `net/packet.rs`: `Packet { main: Vec<Yaml>, raw: HashMap<u8, Vec<u8>> }` — `main` holds the positional fields
    of an Xpra packet (`main[0]` is always the packet type string); `raw` holds binary payloads that get spliced
    in by index (the wire's out-of-band chunks, and the decode thread's decoded pixel data). Accessors
    (`get_u32`, `get_str`, `get_bytes`, `get_hash_str`, ...) index into `main` by position — except `get_bytes`,
    which returns the `raw` entry for that index when there is one — matching the Xpra packet spec per packet type. `Packet` is `Send` (plain
    owned data) — it's passed directly across threads via `EventLoopProxy`, see below.

- `src/client/` (declared via `mod client;` in `main.rs`, submodules listed in `src/client/mod.rs`): the GUI
  client, built on `winit` (cross-platform windowing/event loop) + `softbuffer` (cross-platform CPU pixel
  presentation) — one implementation for both Windows and Linux.
  - `client.rs`: `XpraClient` implements `winit::application::ApplicationHandler<Packet>` directly and is the
    central state machine — owns the `Connection`, a `HashMap<u64, XpraWindow>` keyed by Xpra window id (`wid`),
    a reverse `HashMap<WindowId, u64>` (`id_map`) for looking up `wid` from winit's `WindowId` in
    `window_event`, the shared `softbuffer::Context`, and the `EventLoopProxy<Packet>`/`mpsc::Sender<Packet>`
    used to move packets between threads. Unlike the old Win32 version there's no global singleton — winit hands
    `&mut self` straight into `resumed`/`user_event`/`window_event`. `do_process_packet` dispatches incoming
    packets by their type string (`hello`, `new-window`, `new-override-redirect`, `window-move-resize`,
    `lost-window`, `window-metadata`, `draw`, `draw-decoded`, `draw-failed`, `disconnect`, ...); outgoing packets
    are built with `serde_json::json!` and sent via `write_json` → `net::io::write_packet` (`hello`,
    `window-focus`, `pointer-motion`, `pointer-button`, `keyboard-event`, `window-map`, `window-configure`,
    `window-close`, `window-draw-ack`, `ping`, `ping_echo`, `logging-event`, `connection-close`, the
    `clipboard-*` family and the `audio-*` family). Keyboard mapping (`physical_key_to_xpra_keycode`/`key_to_xpra_keyname`) derives the
    X11-style `keycode`/`keyname` xpra expects from winit's `PhysicalKey`/`Key` — see inline comments; extend the
    `NamedKey`/punctuation tables there if a real server session shows a key not being recognized.
    - **Packet names are the post-6.5 ones** — the outgoing side uses no legacy packet type. xpra 6.5
      renamed most client→server packets and put the old names behind `add_legacy_alias(...)` calls that
      only run when the server has `BACKWARDS_COMPATIBLE` (`XPRA_BACKWARDS_COMPATIBLE`, default on); the
      authoritative old→new table is xpra's `net/packet_type.py`. Three of the renames are *not* plain
      renames and must not be "simplified" back into positional packets: `keyboard-event` moved
      everything after `pressed` into an attributes dict, `window-configure` moved geometry/state/
      properties into a config dict, and `clipboard-data` (which replaced `clipboard-token`) moved the
      targets and per-target payloads into an options dict. `window-draw-ack` is the sharp edge:
      it replaced `damage-sequence` *and* reordered the fields to `wid, w, h, seq` — sending the old
      order under the new name has the server read the packet sequence as a window id, which silently
      wrecks its damage/batching bookkeeping. To check for regressions, run a server with
      `XPRA_BACKWARDS_COMPATIBLE=0`: it then refuses every legacy name outright.
      Two hello capabilities are part of the same move: the packet encoder (`encoders: ["yaml"]`) and
      the picture encodings (`encoding.options`/`encoding.core`). Each replaced a pre-6.5 spelling —
      a bare `yaml: true` and a top-level `encodings` list — that the server reads only in
      backwards-compatible mode *and* only when the modern form is absent, which makes both pure
      dead weight; neither is sent any more. Without `encoders` a non-backwards-compatible server
      drops the connection with "failed to negotiate a packet encoder", and without
      `encoding.options` with "client failed to specify any supported encodings".
      The *incoming* side is still on the legacy names (`new-window`, `draw`, `lost-window`, `notify_show`,
      ...), which is why the client only works against a server left in its default backwards-compatible
      mode; against `XPRA_BACKWARDS_COMPATIBLE=0` the handshake and input now succeed but no window is
      created (the server sends `window-create`/`encoding-set`, which `do_process_packet` doesn't know).
    - **Server events**: the hello advertises `events: true`, enabling informational
      `server-event` packets for lifecycle events such as `handshake-complete`, `startup-complete`,
      `suspend`, `resume`, and `exit`. `process_server_event` logs the event name and optional
      arguments but deliberately does not alter client state; dedicated protocol packets remain
      authoritative.
    - **Pointer grabs**: the hello advertises `pointer.grabs`, which enables `pointer-grab` and
      `pointer-ungrab` packets when a remote application grabs its pointer. The client asks winit
      for `CursorGrabMode::Confined`, falls back to `Locked`, tracks the owning `wid`, and releases
      the grab on an ungrab packet or before destroying the grabbed window.
    - **Window metadata**: `metadata.supported` is limited to the properties this backend applies:
      title, decorations, fullscreen, maximized/iconic state, above/below level, and size
      constraints. The same `apply_window_metadata` path handles initial `new-window` metadata and
      incremental `window-metadata` packets; fixed minimum/maximum sizes disable resizing.
    - **Authentication** (`process_challenge` in `client.rs`): a password-requiring server replies to our first
      `hello` with a `challenge` packet instead of its own hello. We advertise only `digest`/`salt-digest` =
      `["hmac+sha256"]`, so the server always picks that one digest (`choose_digest`, xpra `auth/sys_auth_base.py`).
      The reply is a **second** `hello` (`send_hello(Some((response, client_salt)))`) carrying `challenge_response`
      = `HMAC(password, HMAC(client_salt, server_salt))` (both HMACs lowercase-hex, via `net::sha256`). Two subtle
      points: (1) the incoming `server_salt` is a YAML `!!binary` scalar that `packet::get_bytes` already
      base64-decodes; (2) our writer emits JSON-as-YAML and *can't* carry raw binary, so `client_salt` is a random
      **ASCII hex** string (from `secure_hex`, OS-CSPRNG-seeded) rather than raw bytes — the server utf-8-decodes
      it back to the same bytes, so the digests still match. The password comes from, in order: `XPRA_PASSWORD`;
      `pinentry` if on `PATH` (driven over its Assuan protocol on a worker thread — `spawn_pinentry`/`run_pinentry`
      — which posts the result back as a synthesized `challenge-password`/`challenge-cancel` packet, the auth
      analogue of `draw-decoded`); otherwise the built-in `AuthDialog`. A wrong password ends with the server's
      `disconnect "authentication failed"` (→ `AuthenticationFailed`, exit 28); only `hmac+sha256` is handled, and
      `xor`/`des`/other digests fail cleanly. **Verify against a real server** (no test harness for this):
      `xpra start :N --bind-tcp=127.0.0.1:PORT --tcp-auth=password:value=PW`, then connect with `XPRA_PASSWORD=PW`
      (env path) or without it (pinentry / dialog path) and confirm `startup complete!`.
  - `auth_dialog.rs`: the built-in password prompt (`AuthDialog`) used when there is no
    `XPRA_PASSWORD` and no `pinentry` — a plain `winit`+`softbuffer` window drawn like `XpraWindow` but
    self-contained: it collects a password (echoing only `*`) and reports `Submit`/`Cancel` to `client.rs`, which
    routes its events in `window_event` *before* the `id_map` lookup (the dialog has no `wid`).
    Works on every platform, so it is the universal fallback (Windows without GnuPG in particular).
  - `connect_dialog.rs`: the **connection dialog** (`ConnectDialog`), shown when the binary is started with no
    argument at all — protocol drop-down (`tcp`/`ssl`/`ws`/`wss`/`ssh`, each pre-filling the port with its
    default: 10000, or 22 for ssh), host, port, optional username and password, plus `Cancel`/`Connect`. It only
    *collects*: `handle_key`/`handle_mouse` return a `ConnectAction`, whose `Connect(ConnectDetails)` carries a
    URI in exactly the form `parse_target` takes on the command line (`build_uri`, unit-tested against
    `parse_target`) plus the username/password. Connecting, and the state machine around it, live in `main.rs`.
    Two things it does that `auth_dialog.rs` does not: it is **DPI-aware** (a logical-size window, with the
    layout written in the units of a 100% display and scaled through `px()`/`font_scale()`), and it hit-tests the
    pointer itself (`Rect::contains` against rectangles computed in *physical* pixels, the same ones used to
    draw, so the two cannot drift apart). Fields freeze while a connection attempt is in flight (`Status`).
  - `font.rs` + `paint.rs`: what both dialogs draw with, since there is no text or widget dependency here.
    `font.rs` is Spleen 8x16 (BSD-2-Clause, credited in the file and in `README.md`), printable ASCII only,
    converted from the upstream BDF with the bit order reversed so bit 0 is the leftmost column; it replaced a
    public-domain 8x8 font that had to be drawn at double size and looked it. `paint.rs` is `fill_rect` and
    `outline`. Both dialogs measure text by counting characters, which only works because the font is monospaced
    (`font::GLYPH_W`/`GLYPH_H`, `font::text_width`).
  - `remote_logging.rs`: **client→server log forwarding** (xpra's `--remote-logging=send`), plus the whole of the
    *local* log output. `RemoteLogger` is the global `log::Log`: it prints locally via `LocalLogger` *and* forwards
    info-and-above
    records to the server as `logging-event` packets, so they land in the server's log file (handy when the client runs
    headless / its stderr isn't visible). `init()` (called from `main`)
    returns a `LogSink = Arc<Mutex<Option<EventLoopProxy<Packet>>>>` that starts empty; `XpraClient::process_hello`
    drops the proxy into it **only when the server's hello advertises `remote-logging.receive`** (a nested
    `{receive, send}` dict, xpra `server/subsystem/logging.py`), so we never send `logging-event` packets to a server
    that would reject them (verified against both `--remote-logging` default and `=no`). Forwarding, like the
    ping timer, doesn't touch the socket: the logger posts a synthesized client-side `send-log` packet (carrying
    the python logging level + text) via the proxy from *whatever* thread logged, and the UI thread turns it into
    the wire `logging-event` packet (`send_log` → `["logging-event", level, msg, dtime]`, `dtime` = ms since
    `start`). Two
    loop guards, mirroring xpra's own handler: only Info+ is forwarded (the write path only logs at debug/trace,
    or errors that set `exit_code` and make `write_json` a no-op — so a normal send never re-logs), and a
    thread-local `IN_FORWARD` flag stops a forward that itself logs from recursing. Level mapping is `log`→python:
    Error 40 / Warn 30 / Info 20.
    `LocalLogger` is what `simple_logger` used to do, hand-rolled — same output, byte for byte:
    `2026-07-30T09:42:06.176Z ERROR [xpra] message`, on **stdout** (not stderr), level padded to five columns and
    coloured red/yellow/cyan/purple with Trace left plain. Colour is decided once at init, as `colored` decided it:
    only when stdout is a terminal and `NO_COLOR` is unset — and on Windows only if `enable_ansi()` can turn
    `ENABLE_VIRTUAL_TERMINAL_PROCESSING` on, which `colored` used to do for us and without which conhost prints the
    escapes literally (Windows Terminal enables it itself). The timestamp comes from `SystemTime` through
    `civil_from_days`, Howard Hinnant's algorithm, which has the unit test — the epoch, a pre-epoch day (negative
    day numbers need flooring division, not truncating) and all three leap-year rules including 1900 and 2100.
    Dropping `simple_logger` took **nine** crates out of the graph (it and `colored`, plus the `time` tree behind its
    default `timestamps` feature) and, more importantly, unpinned the build from rustc 1.88: every `time` release
    `simple_logger 5.2.0` allows declares that MSRV, and since `simple_logger` itself declares none, cargo's
    MSRV-aware resolver could not back away from it. See the packaging notes below.
  - `tray.rs` (Windows-only, `#[cfg(windows)]`): the notification-area icon, its right-click menu (a greyed
    header naming the session, a separator, `Exit`) and the balloon notifications it raises. Hand-rolled on
    `Shell_NotifyIconW` via the `windows` crate
    Media Foundation already pulls in — no new crate, just the `Win32_UI_Shell`/`Win32_UI_WindowsAndMessaging`/
    `Win32_System_LibraryLoader`/`Win32_Graphics_Gdi` features (that last one only because `WNDCLASSW` and
    `RegisterClassW` are gated on it). Two non-obvious constraints, both easy to regress:
    (1) **no extra thread, by design.** Windows message delivery is thread-affine and winit's Windows backend
    pumps every window owned by the thread that called `run_app` — `PeekMessageW(&msg, 0, ..)` + `DispatchMessageW`
    (`0` = any window of the calling thread; dispatch goes to *that window's* class wndproc, ours not winit's),
    waiting in `MsgWaitForMultipleObjectsEx(.., QS_ALLINPUT, ..)`, which wakes on the shell's posted callback.
    So `Tray::new` is called from `XpraClient::resumed` on the UI thread and winit pumps it for free. The wndproc
    can't reach the `ActiveEventLoop` that `quit` needs, so `Exit` posts a synthesized client-side `tray-exit`
    packet through the `EventLoopProxy` — same pattern as `send-ping`/`draw-decoded`. The wndproc must never call
    `PostQuitMessage`: it shares winit's message queue.
    (2) the window is a never-shown *top-level* window, **not** `HWND_MESSAGE` — message-only windows don't
    receive broadcasts, and `TaskbarCreated` (re-add the icon after an explorer restart) is broadcast.
    `TrackPopupMenu` runs a nested modal loop, so the session stops repainting while the menu is open; that is
    normal for a native app, not a bug. Cleanup is `impl Drop` (`NIM_DELETE` + `DestroyWindow`), which runs
    because `main::run` holds the `XpraClient` in a local and drops it on return, on the UI thread.
    **Notifications** ride on the same icon and are therefore Windows-only: `show_notification` /
    `close_notification` are `Shell_NotifyIconW(NIM_MODIFY, ..)` with `NIF_INFO` set on a *copy* of the stored
    `NOTIFYICONDATAW` (the stored one keeps the `NIF_ICON|NIF_MESSAGE|NIF_TIP` flags the `TaskbarCreated`
    re-add needs). `szInfoTitle` = the notification's summary, `szInfo` = its body — and since a balloon with an
    empty `szInfo` is not shown at all, a body-less notification puts the summary in `szInfo` and the app name in
    the title. `dwInfoFlags` is `NIIF_USER` (with `NIF_ICON` so `hIcon` is read), which uses our own xpra icon as
    the balloon icon instead of the generic `NIIF_INFO` "i". Withdrawal is the same call with an empty `szInfo`,
    guarded by a `shown_notification: Option<u64>` so a `notify_close` for a *different* xpra `nid` doesn't take
    the current balloon down. Ignored by design (the user asked for the simple version): actions, hints, the
    per-notification icon, and `expire_timeout` (`uTimeout` has been ignored since Vista). On Windows 10+ the
    shell turns balloons into toasts and applies its own Focus-assist/notification policy, so one legitimately
    landing in the Action Center rather than on screen is not a bug. `client.rs`'s `process_notify_show` still
    logs every notification on every platform (that log line is also what remote logging sends the server); the
    tray call is an extra `#[cfg(windows)]` step after it.
  - `window.rs`: `XpraWindow` owns a `winit::window::Window`, a `softbuffer::Surface`, and a persistent
    `framebuffer: Vec<u32>` (softbuffer only hands you the *live* to-be-presented buffer on each
    `buffer_mut()` call, not a persistently addressable store, so `XpraWindow` keeps its own full-window pixel
    buffer as the source of truth). `paint()` converts decoded pixels (jpeg → `BGRA`, png → `RGBA8`) into
    softbuffer's `0x00RRGGBB` `u32` format per-pixel and writes the damaged sub-rect into `framebuffer`;
    `draw_screen()` (on `WindowEvent::RedrawRequested`) copies the whole `framebuffer` into the surface buffer
    and presents it; `resize()` reallocates `framebuffer` (zero-filled — relies on the server re-sending damage
    after a `window-configure` round-trip rather than preserving old contents).
  - `draw_decoder.rs`: decodes `jpeg` (via `turbojpeg`), `png` (via `spng`) and `webp` (via `libwebp-sys`)
    payloads into raw pixel buffers — platform-independent, unchanged by the GUI backend. These are *stateless*
    (one packet in, one image out). `webp` uses `WebPDecodeBGRA`, which both allocates its output (so the pixels
    have to be copied into a `Vec` and the buffer handed back to `WebPFree`) and hands back BGRA — the same layout
    turbojpeg produces, so `window::paint` treats `webp` exactly like `jpeg` and no new pixel path was needed.
  - `mediafoundation.rs` (Windows-only, `#[cfg(windows)]`): `h264` video decode via Media Foundation — no
    third-party codec is linked (the decoder lives in the OS, `msmpeg2vdec.dll`; +~13KB to the binary, just the
    COM/MF bindings from the `windows` crate). Pipeline is `CLSID_CMSH264DecoderMFT` (H.264 Annex-B → NV12) →
    `CLSID_VideoProcessorMFT` (NV12 → RGB32, which in memory is softbuffer's BGRA), so `window::paint` treats
    h264 exactly like turbojpeg's BGRA. Unlike jpeg/png the decoder is **stateful** (H.264 is inter-frame
    predicted): `start_draw_decode_loop` keeps a per-`wid` `HashMap<u64, H264Decoder>` local to the decode thread
    (these COM objects never cross threads, so nothing is `Send`). `H264Decoder::decode` returns
    `Ok(Some(bgra))` (frame ready), `Ok(None)` (input consumed, decoder still warming up — the sequence is still
    acked, painting is skipped), or `Err`. Advertising is Windows-only and needs *two* things for the server to
    actually send video: `h264` in the advertised encoding lists (`encoding.options`/`encoding.core`, which
    `send_hello` fills from one `encodings` vec), **and** a nested `encoding` caps dict with
    `full_csc_modes = {"h264": ["YUV420P"]}` (the server reads `hello["encoding"]["full_csc_modes"]` and only
    offers a video encoding whose listed colourspaces intersect its encoder's — see xpra
    `server/source/encoding.py`). We list only `YUV420P` and pin `encoding.h264 = {"YUV420P.profile": "high"}`
    because the MF decoder only handles 8-bit 4:2:0 up to High profile (never 4:2:2/4:4:4/High10).
    Colour range is handled explicitly: MF's H.264 decoder doesn't reliably surface the VUI
    `video_full_range_flag`, and xpra's encoders default to *full* range and only send the `full-range` draw
    option on transitions/keyframes (omitted in steady state), so `H264Decoder` tracks it per-stream
    (defaulting to `true`, `None` = unchanged) and stamps `MF_MT_VIDEO_NOMINAL_RANGE` on the Video Processor's
    NV12 input. The remaining unverified knob is RGB32 orientation (we request top-down via a positive
    `MF_MT_DEFAULT_STRIDE`); `MF_MT_YUV_MATRIX` is left to the VP's pick-by-resolution BT.601/709 default.
    Per-window decoders are released when the window closes: the UI thread forwards `lost-window` down the
    same channel as draws (so still-queued draws for that window drain first), and the decode loop drops that
    `wid`'s `H264Decoder`.

- `src/main.rs`: binary entry point. Builds a `winit::event_loop::EventLoop<Packet>`, spawns the decode thread,
  and runs `event_loop.run_app(&mut app)`. `App` — **not** `XpraClient` — is the `ApplicationHandler` for the
  process, because **winit allows one event loop per process** (`EventLoopError::RecreationAttempt`), so the
  connection dialog cannot be a throwaway loop run before the client's. `App` is therefore a two-state machine:
  - `AppState::Session(XpraClient)`, entered immediately when a target was given on the command line (parsed and
    connected *before* the event loop exists, so a bad address still exits with the right code and no window),
    or later from the dialog. Every `ApplicationHandler` callback `XpraClient` implements is delegated to it,
    which is a thing to keep in sync when adding one there.
  - `AppState::Prompt(Option<ConnectDialog>)` otherwise: the dialog is created in `resumed` (creating a window
    needs the `ActiveEventLoop`). `Connect` runs `connect()` on a **worker thread** — a dropped SYN takes tens of
    seconds to time out and the dialog must keep drawing — which hands the `Connection` back over a channel and
    posts a synthesized client-side `connect-result` packet, the same pattern as `pinentry`/`draw-decoded`.
    A failure is shown in the dialog and retried, not exited on. On success `start_session` builds the client,
    passes it the dialog's softbuffer `Context` and the collected username/password, and **calls `resumed` on it
    by hand**: winit only fires `resumed` once, back when the dialog was up.

  `XpraClient::username`/`password` are `None` on the command-line path; when the dialog filled them in, the
  username overrides the environment's in `hello` and the password is the first source `process_challenge` tries.

### Threading model

Three threads, and GUI/`winit`/`softbuffer` calls must only ever happen on the UI thread:

1. **UI thread** (`main`) — runs the `winit` event loop, owns `XpraClient` and all `XpraWindow`/softbuffer state.
2. **Reader thread** (`XpraClient::start_read_loop`) — blocking loop calling `net::io::read_packet` on the
   socket, parses each payload into a `Packet`, and sends it straight to the UI thread via
   `EventLoopProxy::send_event` (delivered as `ApplicationHandler::user_event`).
3. **Decode thread** (`XpraClient::start_draw_decode_loop`) — receives `draw` packets forwarded by the UI thread
   over a plain `mpsc::Sender<Packet>` (no `EventLoopProxy` equivalent exists for UI-thread → other-thread), calls
   `draw_decoder::decode` to turn compressed image data into a raw pixel buffer off the UI thread, then sends the
   result back to the UI thread as a synthesized `draw-decoded` (or `decoding-failed`) packet via its own
   `EventLoopProxy<Packet>` clone.

`ApplicationHandler::user_event` is the only place that receives packets from these threads and dispatches them
via `do_process_packet`.

### Shutdown / connection loss / exit codes

`[profile.release]` sets `panic = "abort"`, so a panic on *any* thread kills the whole process — the I/O paths
must return errors, not `unwrap()`. Only the UI thread can stop the event loop (`ActiveEventLoop::exit` is only
reachable from an `ApplicationHandler` callback), so the reader thread (on read/parse failure) and the write path
(on a failed `write_packet`) both synthesize a client-side packet — `connection-lost` or `invalid-packet`, neither
of which exists on the wire — and send it to the UI thread through the usual `EventLoopProxy`, which logs the
reason and exits. The decode thread just breaks out of its loop when its `mpsc` channel closes (UI thread gone) —
a killed server used to abort here with `RecvError`.

`XpraClient::quit` records the cause in `exit_code: Option<ExitCode>` (first cause wins) and stops the event loop;
`main::run` returns it and `main` hands it to `process::exit`. A set `exit_code` also silently drops further
outgoing packets, since the event loop keeps delivering queued input events on its way out.

`src/exit_codes.rs` mirrors the subset of xpra's own `ExitCode` (`xpra/exit_codes.py`) that we can produce, so
wrapper scripts see the same values as with the python client: `ConnectionFailed`(18) for anything that fails
before there is a session (connect refused, ws handshake, garbage from a non-xpra peer, kicked out before
`startup-complete`), `SslFailure`(16)/`SshFailure`(8) for those transports' setup, `ConnectionLost`(1) once the
session was up, `PacketFailure`(9) for an unparseable packet mid-session, `AuthenticationFailed`(28),
`ArgumentMismatch`(34) for a bad command line, and `Ok`(0) for a plain server-sent `disconnect`. The
before/after-`startup_complete` split and `disconnect_is_an_error` mirror xpra's `client/base/client.py`
(`_process_connection_lost`, `server_disconnect_exit_code`) — a disconnect whose reason mentions "error" (or a
non-idle "timeout") is a failure, everything else ("server shutdown", "new client", ...) is a normal goodbye.

## Distribution packaging (`packaging/`)

RPM and DEB build definitions for [repo-build-scripts](https://github.com/Xpra-org/repo-build-scripts), which
builds them in per-distribution containers. Mirrors the layout of xpra's own `packaging/`, but for a single
package: `packaging/target-repository` (`beta`), `packaging/rust-xpra.desktop`, `packaging/rust-xpra.1`
(the man page — installed by the spec's `%install` and, on the Debian side, by `dh_installman` via
`rust-xpra.manpages`; it documents the *installed* name, so `rust-xpra(1)`, since `xpra(1)` is the python
client's), `packaging/rpm/`
(`rust-xpra.spec` + `default.list`, the last-resort manifest name the build scripts look for, holding just
`rust-xpra`), and `packaging/debian/` (`build.sh`, which unpacks the tarball and runs `debuild`, plus
`rust-xpra/` which becomes the source tree's `debian/`). `packaging/README.md` has the details; the traps:

- **The binary installs as `/usr/bin/rust-xpra`**, not `xpra` — that path belongs to the python `xpra` package
  and the two must be co-installable. Both builds rename it; the cargo binary is still called `xpra`.
- **`spng-sys` forces `libz-sys/static`**, so zlib is always compiled from source and bundled. The spec therefore
  has to set `%global _lto_cflags %{nil}`: Fedora's global `-flto` reaches every C file the `cc` crate compiles
  through `$CFLAGS`, and the resulting LTO objects break archive member resolution — the link fails on
  `undefined reference to inflateInit_`. Do not "clean this up".
- **`TURBOJPEG_SOURCE` and `TURBOJPEG_STATIC` must move together.** Both builds probe with
  `pkg-config --atleast-version=3.0 libturbojpeg` and fall back to the crate's vendored copy where the system one
  is too old (EL9 ships 2.0.90 in CRB, Debian trixie and Ubuntu 24.04 ship 2.1.5) — which is what `cmake` and
  `nasm` are build dependencies for. Setting `TURBOJPEG_STATIC=0` on the *vendored* path makes `turbojpeg-sys`
  emit `-l dylib=turbojpeg`, which links the too-old system library and fails on `undefined reference to tj3Init`.
  Hence `%{turbojpeg_static}` / `$(if $(filter vendor,...))` rather than a constant.
- `--features webp-dylib` is passed unconditionally so `libwebp-sys` links the distro's shared libwebp.
- **Six runtime dependencies are listed by hand** (`libX11`, `libXcursor`, `libXi`, `libxcb`, `libxkbcommon`,
  `libwayland-client`): winit, softbuffer and x11rb `dlopen` them, so neither `dh_shlibdeps` nor rpm's dependency
  generator can see them. Anything new that gets `dlopen`ed has to be added to both.
- `debian/rules` appends the `embedded-library libjpeg` lintian override itself, and only on the vendored path —
  in the static overrides file it would be a `mismatched-override` warning everywhere else.
- **`Cargo.lock` is in `.gitignore`**, so the release tarball carries none and cargo re-resolves against live
  crates.io on every build: two builds of the same tarball can differ. Committing it would fix that and make the
  declared `rust >= 1.85` / `rustc (>= 1.85)` exact rather than merely correct.
- The `Source:` URL points at the `v<version>` GitHub tag, so **the tag has to exist** before a build; the version
  there and in `debian/changelog` must match.

Verified end to end for 0.3.0 on Fedora 44 (rpm), Debian trixie (deb, vendored turbojpeg) and Debian sid (deb,
system turbojpeg) — all three install and run, lintian reports only `initial-upload-closes-no-bugs`.

## Known repo quirks

- `build.rs` embeds the Windows resources: `assets/xpra.ico` (name id `1` — the executable's icon in Explorer,
  and what `tray.rs` loads back at runtime with `LoadImageW`) and `exe.manifest` (the per-monitor-V2 DPI
  manifest, which used to be carried in the repo unreferenced). It gates on `CARGO_CFG_TARGET_OS`, **not**
  `cfg!(windows)`: a build script runs on the *host*, so `cfg!` asks the wrong question and would silently drop
  the resources from a Linux→Windows cross build. For the same reason `winresource` is a plain
  `[build-dependencies]` — Cargo resolves target-specific build-deps against the host too. A failed resource
  compile only warns; `tray.rs` then falls back to `IDI_APPLICATION`. The manifest asks for the same DPI
  awareness winit sets programmatically (`become_dpi_aware`), so embedding it changes no behaviour.
- **Do not enable `libwebp-sys`'s `sse41` / `avx2` features.** They look like free speed and are neither free nor
  speed. `libwebp-sys`' `build.rs` puts `-msse4.1`/`-mavx2` on the *whole* `cc::Build` — every vendored `.c` file,
  unlike libwebp's own CMake, which applies them per-file precisely so that the generic and SSE2 paths stay
  baseline. With `avx2` on, `dec_sse2.o` (the path libwebp's *runtime* CPU dispatch picks on any SSE2 machine)
  comes out full of VEX-encoded instructions and even `ymm` registers, so the binary `SIGILL`s on any pre-AVX2
  CPU (pre-2013 Intel, and current low-power Celeron/Pentium/Atom N-series — exactly the thin clients this is
  for), runtime dispatch notwithstanding.
  And they buy nothing measurable, on either of the two encodings xpra actually sends (it picks lossless VP8L for
  text-heavy/few-colour rects and lossy VP8 for the rest). Best-of-7 × 200 iterations on an i7-6700K, which has
  both feature bits, with the kernels confirmed compiled in (37 `SSE41` symbols vs 9 in the default build, 38
  `AVX2` ones) and therefore dispatched:

  |                       | default (SSE2) | `sse41` | `sse41`+`avx2` |
  |-----------------------|----------------|---------|----------------|
  | 1080p lossy (VP8)     | 8.92 ms        | 8.97 ms | 8.82 ms        |
  | 1080p lossless (VP8L) | 9.50 ms        | 9.54 ms | 9.40 ms        |
  | text lossy (VP8)      | 5.72 ms        | 5.70 ms | 5.70 ms        |
  | text lossless (VP8L)  | 0.58 ms        | 0.57 ms | 0.63 ms        |

  All within noise. For `avx2` that is structural, not luck: `lossless_avx2.c` is the *only* decode-side AVX2 file
  in libwebp, so AVX2 has no lossy-VP8-decode kernels to run at all, and the VP8L ones it does have don't move the
  needle. SSE4.1 does have real lossy-decode kernels (`dec_sse41.c`, `upsampling_sse41.c`, `yuv_sse41.c`) — they
  just don't beat the SSE2 ones the decoder already uses.
