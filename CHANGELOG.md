# Changelog

## [0.2.1] 2026-07-16
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
  * [handle server-initiated interactive window moves and resizes](https://github.com/Xpra-org/rust-xpra/commit/61c2f41ef9ff532b45c099a301bba05252bec600)
  * [handle `configure-override-redirect` packets](https://github.com/Xpra-org/rust-xpra/commit/65156ab5c482e46c29d5d68b5712736a5320a934)
  * [bring windows to the front on `raise-window`](https://github.com/Xpra-org/rust-xpra/commit/83876b11221c75f69c94aceb01b162101088ec4f)
  * [set server-provided window icons](https://github.com/Xpra-org/rust-xpra/commit/fac44ff300c1a134e98768b01a899a730599b3a0)
  * [ring server-forwarded bells](https://github.com/Xpra-org/rust-xpra/commit/47f48769943e009ad0833e87ecf66293865586c7)
  * [apply server-provided pointer cursors](https://github.com/Xpra-org/rust-xpra/commit/377aa713b07be0c802c022d984e52b8fae0db62c)
  * [log `pointer-position` packets](https://github.com/Xpra-org/rust-xpra/commit/fcdae2e48a0547a901a799cc421caead0b22074e)
  * [report server-forwarded notifications in the client log](https://github.com/Xpra-org/rust-xpra/commit/2b1361d4ea1e18f8f352d95b691b3e3325f2c930)
  * [minimize and restore windows on `show-desktop`](https://github.com/Xpra-org/rust-xpra/commit/8792cd214b61fbde8bb3c0317f65c05b789dd2a3)
