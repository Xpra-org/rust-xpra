# rust-xpra
Xpra client implemented in [rust](https://www.rust-lang.org/), for MS Windows and Linux.

## Status

This is a proof of concept only and is not usable at this point.

It builds on MS Windows and Linux (X11 and Wayland).

It only supports unauthenticated TCP connections.

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
```

Only the `tcp` protocol is supported; any other protocol in the URI is rejected.
