# Changelog

## [0.4.0] 2026-09-24
* 🔧 Platforms, build and packaging:
  * [ship a CycloneDX SBOM with every release binary](https://github.com/Xpra-org/rust-xpra/commit/c0011223f868df7e7ae676c0fe5033b453afef94)
  * [cover the protocol layer with tests](https://github.com/Xpra-org/rust-xpra/commit/e20e35f97c0ba4f451d58ec6f94f7cdd9672cf05)
* ✨ Features:
  * [add `-v` to raise the log level](https://github.com/Xpra-org/rust-xpra/commit/6cf2c452f048ae9fefad950141184d82a9b79c6e)
  * [send the local display size and per-monitor layout in the hello packet](https://github.com/Xpra-org/rust-xpra/commit/9814ba5a2183570c5b975daea9e639755971d17e)
  * [send monitor-relative coordinates with every position](https://github.com/Xpra-org/rust-xpra/commit/45233b9b271818a7c55c13a10530537f9ee4a897)
  * [map server window positions back to local coordinates](https://github.com/Xpra-org/rust-xpra/commit/72a120d4096e7755c640fc34c900c5f28a99a1ac)
  * [place new windows by their client area, not their frame](https://github.com/Xpra-org/rust-xpra/commit/943c02b0e09a248d24350bf47317d63b67d71785)
  * [handle interrupts gracefully](https://github.com/Xpra-org/rust-xpra/commit/3423d32f61e30709a37b6c78f7f9dbb291ca1d3a) —
    `SIGINT`/`SIGTERM`/`SIGHUP` and the Windows console events tell the server why we are leaving
  * [fix the Super key, the dead keys and the non-ascii ones](https://github.com/Xpra-org/rust-xpra/commit/642b6bd7300e98a45752fb6b404900b14e467021)
  * [take the modifier names from the server's keymap](https://github.com/Xpra-org/rust-xpra/commit/ddc611fab162d67f00608ef5208755cac7ca39de)
  * [name the shifted punctuation keysyms](https://github.com/Xpra-org/rust-xpra/commit/c5e62c2b844e257c1862ef681a56cc0ddae1fae4)
  * [don't confine the pointer to override-redirect windows](https://github.com/Xpra-org/rust-xpra/commit/8a1094fd7fc5a4b3dc40513031559fb4f8467439)
  * [don't lock the cursor when it cannot be confined](https://github.com/Xpra-org/rust-xpra/commit/a1373d8d6fab7512f45156018da9884052d15499)
  * [don't report the geometry of a minimized window](https://github.com/Xpra-org/rust-xpra/commit/f07357ff66ac1c2a5c80b2df0b62d66a3c20f650)
* 🌈 Encodings:
  * [vectorise the draw blit](https://github.com/Xpra-org/rust-xpra/commit/9101e3eb7154226b09ec1e93f0f4244fb12c5bc9)
  * [present only the regions that changed](https://github.com/Xpra-org/rust-xpra/commit/480dfe32155cfbee5cd1441980409e351550ed91)
  * [handle JPEG and PNG draw decode errors](https://github.com/Xpra-org/rust-xpra/commit/04c47137c4f4a4a98ebef895b670dd59cbf4fbd1)
* 🖧 Network:
  * [work with servers running without backwards compatibility](https://github.com/Xpra-org/rust-xpra/commit/3abb648c6eaf9e033923048288faaa682c4d3fd2) —
    every packet is accepted under both its legacy and its current name, and nothing sent depends
    on the server's `BACKWARDS_COMPATIBLE` mode any more
  * [announce 6.6 as the minimum protocol version](https://github.com/Xpra-org/rust-xpra/commit/259618eee92405dac24b098282f9fd736ab4c860)
  * [fix draw acknowledgement packet compatibility](https://github.com/Xpra-org/rust-xpra/commit/e14eba12be62975f900aad04b4f310ad13b7699c)
  * [advertise window forwarding in the `window` namespace](https://github.com/Xpra-org/rust-xpra/commit/ac18abbf78379a8d11266668e8ea07ffff5e1267)
  * [handle the modern cursor packets](https://github.com/Xpra-org/rust-xpra/commit/88ac5c708485bb444da642288d7d21db43ef6143)
  * [handle the `encoding-set` packet](https://github.com/Xpra-org/rust-xpra/commit/0c00383ce9c5de17ed540b7ed88cb6268b1ee1ba)
  * [don't ping a server that advertises no ping capability](https://github.com/Xpra-org/rust-xpra/commit/5cb9b248dbbe629fdfe663d870cbc61464deeeaa)
  * [reply to pings under the current packet name](https://github.com/Xpra-org/rust-xpra/commit/785f240562399ff1b6d47d5a8ecca155f6ff3b7e)
  * [advertise notifications under the current name too](https://github.com/Xpra-org/rust-xpra/commit/ddd5711408cff6516c306ff9c48802ee6a1d62d4)
  * [don't panic on a hash key the server did not send](https://github.com/Xpra-org/rust-xpra/commit/707c1a7539b06633a5112785f7513b86cfe97dc4)
* Documentation:
  * [document macOS support](https://github.com/Xpra-org/rust-xpra/commit/85fbcd98295ff5a00f5d0aecee92c04d8ac6d045)

## [0.3.1] 2026-08-01
* 🔧 Platforms, build and packaging:
  * [build on older distributions](https://github.com/Xpra-org/rust-xpra/commit/f90be1a74d1146f8ac66092d538af402bc2b63ad)
  * [disable lto on Debian](https://github.com/Xpra-org/rust-xpra/commit/ca39a61e271ea4d184ddbdb41f7ce17b286d0e61)
  * [build from github releases](https://github.com/Xpra-org/rust-xpra/commit/a7f4aa9220162134f3536aa154ce604dab38b48d)
  * [build Debian packages with Rust coreutils](https://github.com/Xpra-org/rust-xpra/commit/9d1072c6ebb323ad518e97acde1ba8e5cda00fb3)
* ✨ Features:
  * [mmap](https://github.com/Xpra-org/rust-xpra/commit/0a8ba31ff4ded1660d5ea7aafb602e16e1a738da)
* 🖧 Network:
  * [connect directly to Unix-domain sockets](https://github.com/Xpra-org/rust-xpra/commit/eb857de35758b5fe0a704d7846e165796e092a7c) —
    Unix builds accept `socket:///absolute/path` and `/absolute/path` targets
  * [verify SSL certificates by default](https://github.com/Xpra-org/rust-xpra/commit/7c6827df9433596718480c2a4b90ce4548f7d0c5) —
    `ssl://` and `wss://` now check the certificate chain and the hostname against the system
    trust store, with `--ssl-insecure` to opt out
* Documentation:
  * [include an interactive dependency graph](https://github.com/Xpra-org/rust-xpra/commit/47fa92ae55335114acfc59d31f93a42001d8f50f)
  * [link to repositories](https://github.com/Xpra-org/rust-xpra/commit/10366489936a8c129faa29090dc051390f6b69ea)

## [0.3.0]
* ✨ Features:
  * connection dialog when started without any arguments
  * [system tray icon with an `Exit` menu entry (MS Windows)](https://github.com/Xpra-org/rust-xpra/issues/8)
  * [show server-forwarded notifications as tray balloons (MS Windows)](https://github.com/Xpra-org/rust-xpra/issues/10)
* 🔧 Platforms, build and packaging:
  * [embed the application icon and the DPI-awareness manifest into the Windows executable](https://github.com/Xpra-org/rust-xpra/commit/1bdd8c8fb00a9b554b902be7eeae24ca1cc54bbb)
  * `rust-xpra(1)` man page, installed by the RPM and Debian packages
* 🖧 Network:
  * send the packet types introduced in xpra 6.5 — no legacy packet type is sent any more, which
    raises the minimum server version to 6.6

## [0.2.4] 2026-07-16
* ✨ Features:
  * [receive server lifecycle events](https://github.com/Xpra-org/rust-xpra/commit/a7956cb747a3a0ce079ccf887137a71d34b00c26)
  * [support server-requested pointer grabs](https://github.com/Xpra-org/rust-xpra/commit/3cbcdb8b62628d2acb5421f78ccc11fdbbdc52fd)
  * [honour window metadata updates](https://github.com/Xpra-org/rust-xpra/commit/718dc4852fefb9db1c05f7ac9b6170b3c064650a)

## [0.2.3] 2026-07-16
* 🔧 Platforms, build and packaging:
  * [support dynamic linking against the system `libwebp`](https://github.com/Xpra-org/rust-xpra/commit/5540621c20f17504deb32344f174d95497dc2457)
* 🖧 Network:
  * [support HMAC-SHA256 password authentication](https://github.com/Xpra-org/rust-xpra/commit/47e2ad5a6541692108b7d272f79b502c7868d2b8)
  * [send `ping` packets](https://github.com/Xpra-org/rust-xpra/commit/a64f16855b2084927e2126f8e7f2bdd1efff95de)
  * [decompress inbound LZ4 packets](https://github.com/Xpra-org/rust-xpra/commit/1f31cd46ec69a8ee8d7530bdc3e1e664e109e1cf)
  * [forward client logs to the server](https://github.com/Xpra-org/rust-xpra/commit/21307b995cd333871ae499e7c7f3e2dbef2bae68)
* 🌈 Encodings:
  * [decode WebP images](https://github.com/Xpra-org/rust-xpra/commit/5540621c20f17504deb32344f174d95497dc2457)
* ✨ Features:
  * [basic text clipboard](https://github.com/Xpra-org/rust-xpra/commit/8e7f68359bd8c746da402e3d5112671ce8a9629f)
  * [handle server-initiated interactive window moves and resizes](https://github.com/Xpra-org/rust-xpra/commit/61c2f41ef9ff532b45c099a301bba05252bec600)
  * [handle `configure-override-redirect` packets](https://github.com/Xpra-org/rust-xpra/commit/65156ab5c482e46c29d5d68b5712736a5320a934)
  * [bring windows to the front on `raise-window`](https://github.com/Xpra-org/rust-xpra/commit/83876b11221c75f69c94aceb01b162101088ec4f)
  * [set server-provided window icons](https://github.com/Xpra-org/rust-xpra/commit/fac44ff300c1a134e98768b01a899a730599b3a0)
  * [ring server-forwarded bells](https://github.com/Xpra-org/rust-xpra/commit/47f48769943e009ad0833e87ecf66293865586c7)
  * [apply server-provided pointer cursors](https://github.com/Xpra-org/rust-xpra/commit/377aa713b07be0c802c022d984e52b8fae0db62c)
  * [log `pointer-position` packets](https://github.com/Xpra-org/rust-xpra/commit/fcdae2e48a0547a901a799cc421caead0b22074e)
  * [report server-forwarded notifications in the client log](https://github.com/Xpra-org/rust-xpra/commit/2b1361d4ea1e18f8f352d95b691b3e3325f2c930)
  * [minimize and restore windows on `show-desktop`](https://github.com/Xpra-org/rust-xpra/commit/8792cd214b61fbde8bb3c0317f65c05b789dd2a3)
