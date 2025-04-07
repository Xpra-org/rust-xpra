# rust-xpra
Xpra client for MS Windows implementated in [rust](https://www.rust-lang.org/).

## Status

This is a proof of concept only and is not usable at this point.

It only builds on MS Windows OS for now.

It only supports unauthenticated TCP connections.

No server, no audio, no clipboard, no notifications, etc..

## Usage

```shell
cargo build
./target/debug/xpra.exe HOST:PORT
```