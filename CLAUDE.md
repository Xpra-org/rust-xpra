# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Proof-of-concept [Xpra](https://xpra.org/) client written in Rust, for MS Windows and Linux (X11 and Wayland).
Not usable yet: unauthenticated `tcp`/`ws` connections only, no server/audio/clipboard/notifications support. See
`README.md` for known Linux/Wayland limitations (window positioning, override-redirect, NumLock — all downstream
of Wayland not letting clients query/set absolute desktop position or create truly unmanaged windows).

## Build / run

Cross-platform: builds on Windows and Linux via `winit` + `softbuffer` (no native GTK/Qt dependency). CI builds
both (see `.github/workflows/rust.yml`). On Linux, building needs X11/Wayland/xkbcommon dev headers (see the
`build-linux` job for the exact package list) plus `pkg-config` + libjpeg-turbo dev headers for `turbojpeg`.

```shell
cargo build
./target/debug/xpra HOST:PORT     # xpra.exe on Windows
./target/debug/xpra ws://HOST:PORT/
```

`config.toml` sets `TURBOJPEG_SOURCE=pkg-config` and `TURBOJPEG_STATIC=1`, required for the `turbojpeg` crate to
build against a local libjpeg-turbo.

There are no automated tests for the GUI/protocol dispatch layer — verify changes manually against a real Xpra
server (`xpra start :100 --bind-tcp=127.0.0.1:PORT --auth=none --tcp-auth=none` works well for local testing).
The only automated tests are `net::sha1`'s unit tests (`cargo test --lib`).

## Architecture

The crate has both a library part (`xpra`, `src/lib.rs`) and a binary (`src/main.rs`).

- `src/lib.rs` / `src/net/`: the Xpra wire-protocol layer, platform-independent.
  - `net/uri.rs`: `parse_target` turns the command-line argument into a `Target` (`Tcp` or `WebSocket`), accepting
    either a bare `host:port` (assumed `tcp`) or a `protocol://host:port/path` URI. Any protocol other than `tcp`
    or `ws` is rejected.
  - `net/connection.rs`: `Connection` is an enum (`Tcp(TcpStream)` / `WebSocket(WebSocketStream)`) implementing
    `Read`/`Write` by dispatching to whichever transport is active, so the rest of the codebase (`io.rs`,
    `XpraClient`) doesn't need to know which one it's talking to. `try_clone()` gives the reader thread its own
    independent instance, mirroring how `TcpStream::try_clone` is used elsewhere.
  - `net/websocket.rs`: a minimal hand-rolled RFC 6455 client (HTTP upgrade handshake + frame masking/framing) —
    deliberately not using a websocket crate, since the protocol needed here is small; see `README.md`. Requires a
    `Sec-WebSocket-Protocol: binary` header for the xpra server to accept the upgrade. `WebSocketStream` buffers
    one reassembled (defragmented) message at a time and serves it through `Read`, transparently answering pings.
  - `net/sha1.rs`: a self-contained SHA1 (only used for the websocket accept-hash, not security-sensitive) — has
    unit tests with the standard RFC 3174 test vectors, run via `cargo test --lib`.
  - `net/io.rs`: packet framing over a `Connection` — 8-byte header (`'P'` magic, flags byte where bit 2 must be
    `FLAGS_YAML`, compression byte, chunk byte, 4-byte big-endian payload length) followed by the payload, written
    as a single `write_all` call. Only YAML encoding, no compression, no chunking are currently supported
    (anything else is a hard error).
  - `net/serde.rs`: parses the YAML payload into a `Packet`.
  - `net/packet.rs`: `Packet { main: Vec<Yaml>, raw: HashMap<u8, Vec<u8>> }` — `main` holds the positional fields
    of an Xpra packet (`main[0]` is always the packet type string); `raw` holds binary payloads that get spliced
    in by index (used for decoded pixel data). Accessors (`get_u32`, `get_str`, `get_bytes`, `get_hash_str`, ...)
    index into `main` by position, matching the Xpra packet spec for each packet type. `Packet` is `Send` (plain
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
    are built with `serde_json::json!` and sent via `write_json` → `net::io::write_packet` (`hello`, `focus`,
    `pointer`, `pointer-button`, `key-action`, `map-window`, `configure-window`, `close-window`,
    `damage-sequence`). Keyboard mapping (`physical_key_to_xpra_keycode`/`key_to_xpra_keyname`) derives the
    X11-style `keycode`/`keyname` xpra expects from winit's `PhysicalKey`/`Key` — see inline comments; extend the
    `NamedKey`/punctuation tables there if a real server session shows a key not being recognized.
  - `window.rs`: `XpraWindow` owns a `winit::window::Window`, a `softbuffer::Surface`, and a persistent
    `framebuffer: Vec<u32>` (softbuffer only hands you the *live* to-be-presented buffer on each
    `buffer_mut()` call, not a persistently addressable store, so `XpraWindow` keeps its own full-window pixel
    buffer as the source of truth). `paint()` converts decoded pixels (jpeg → `BGRA`, png → `RGBA8`) into
    softbuffer's `0x00RRGGBB` `u32` format per-pixel and writes the damaged sub-rect into `framebuffer`;
    `draw_screen()` (on `WindowEvent::RedrawRequested`) copies the whole `framebuffer` into the surface buffer
    and presents it; `resize()` reallocates `framebuffer` (zero-filled — relies on the server re-sending damage
    after a `configure-window` round-trip rather than preserving old contents).
  - `draw_decoder.rs`: decodes `jpeg` (via `turbojpeg`) and `png` (via `spng`) payloads into raw pixel buffers —
    platform-independent, unchanged by the GUI backend.

- `src/main.rs`: binary entry point. Connects the `TcpStream`, builds a `winit::event_loop::EventLoop<Packet>`,
  spawns the decode thread, constructs `XpraClient`, and runs `event_loop.run_app(&mut client)`.

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

## Known repo quirks

- `exe.manifest` (Windows DPI-awareness manifest) is Windows-build-specific and harmless to leave as-is on
  Linux; nothing in the current `winit`-based code references it.
