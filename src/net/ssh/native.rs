// The `--features ssh-native` `ssh://` transport: this client speaks SSH itself, through russh,
// instead of driving a system `ssh` binary - for the hosts where there is no usable one. See
// `mod.rs` for what that costs, `host.rs` for host-key checking and `auth.rs` for authentication.
//
// The shape of the problem is that russh is async and nothing else here is: `Connection` is a
// blocking `Read`/`Write` used from two threads at once (the UI thread writes, the reader thread
// reads). So the session lives on a thread of its own with a single-threaded tokio runtime, and
// `SshStream` is a pair of channels into it:
//
//     UI thread    --(bytes, ack)-->  [ ssh thread: writer task -> channel.data  ]
//     reader thread <---(bytes)-----  [ ssh thread: reader loop <- channel.wait  ]
//
// A write waits for its acknowledgement, which is not decoration: `disconnect_and_quit` (see
// `client/client.rs`) writes the goodbye packet and then stops the event loop, relying on
// `Connection::write_all` having really handed the bytes over by the time it returns - a
// fire-and-forget queue would drop that packet on the way out. See the yield in `pump` for how far
// that guarantee reaches, which is not quite as far as a direct socket write. Waiting also means
// two threads writing at the same time cannot interleave, since each `write` hands over one whole
// buffer.
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::{env, thread};

use log::{debug, info, warn};

use russh::client::{self, Msg};
use russh::{Channel, ChannelMsg, ChannelReadHalf, ChannelWriteHalf};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use super::auth;
use super::host::Client;
use super::{remote_command, SshPrompt};

// The `known_hosts` file to check the server against, for the sessions that should not touch the
// user's own (a test, a service account with a read-only home).
pub const KNOWN_HOSTS_ENV: &str = "XPRA_SSH_KNOWN_HOSTS";

// One write, and the channel its outcome comes back on.
type Outgoing = (Vec<u8>, SyncSender<io::Result<()>>);

pub fn connect(address: &str, username: Option<&str>, display: &str, remote_xpra: Option<&str>,
               prompt: &SshPrompt) -> Result<SshStream, String> {
    let (host, port) = split_address(address)?;
    let user = match username {
        Some(user) => user.to_string(),
        None => local_user().ok_or_else(|| "no username: put one in the target (ssh://USER@HOST/) \
                                           or set USER in the environment".to_string())?,
    };
    let params = Params {
        address: address.to_string(),
        host,
        port,
        user,
        command: remote_command(display, remote_xpra),
        known_hosts: known_hosts_path()?,
        prompt: prompt.clone(),
    };
    debug!("ssh: connecting to {} as {}", params.address, params.user);

    let (out_tx, out_rx) = unbounded_channel::<Outgoing>();
    let (in_tx, in_rx) = channel::<Vec<u8>>();
    let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
    thread::Builder::new().name("ssh".to_string()).spawn(move || {
        // `enable_all` is what gives russh its timers (keepalives) as well as the socket.
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("no runtime for the ssh session: {e}")));
                return;
            }
        };
        runtime.block_on(session(params, ready_tx, out_rx, in_tx));
    }).map_err(|e| format!("cannot start the ssh thread: {e}"))?;

    // block until the session is up, so that a failure is still a failure to *connect*: no window
    // has been opened yet, and `main::connect` maps this to `ExitCode::SshFailure`.
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(SshStream { out: out_tx, inbox: Arc::new(Mutex::new(Inbox::new(in_rx))) }),
        Ok(Err(message)) => Err(message),
        Err(_) => Err("the ssh thread stopped before the session was up".to_string()),
    }
}

struct Params {
    address: String,
    host: String,
    port: u16,
    user: String,
    command: String,
    known_hosts: PathBuf,
    prompt: SshPrompt,
}

async fn session(params: Params, ready: SyncSender<Result<(), String>>,
                 out_rx: UnboundedReceiver<Outgoing>, in_tx: Sender<Vec<u8>>) {
    match open(params, &in_tx).await {
        Ok((reader, writer)) => {
            let _ = ready.send(Ok(()));
            pump(reader, writer, out_rx, in_tx).await;
        }
        Err(message) => {
            let _ = ready.send(Err(message));
        }
    }
}

async fn open(params: Params, in_tx: &Sender<Vec<u8>>)
              -> Result<(ChannelReadHalf, ChannelWriteHalf<Msg>), String> {
    let config = Arc::new(client::Config {
        // an xpra session nobody is touching is legitimately silent for hours, and the server is
        // the side that decides when that has gone on too long.
        inactivity_timeout: None,
        ..client::Config::default()
    });
    let client = Client {
        host: params.host.clone(),
        port: params.port,
        known_hosts: params.known_hosts,
        prompt: params.prompt.clone(),
    };
    let mut handle = client::connect(config, params.address.as_str(), client).await
        .map_err(|e| format!("{e}"))?;
    auth::authenticate(&mut handle, &params.user, &params.prompt).await?;

    let channel: Channel<Msg> = handle.channel_open_session().await
        .map_err(|e| format!("cannot open an ssh session channel: {e}"))?;
    channel.exec(true, params.command.clone()).await
        .map_err(|e| format!("cannot run the xpra proxy command: {e}"))?;
    let (mut reader, writer) = channel.split();

    // `exec` was sent with want_reply, and that reply comes before any output from the program, so
    // waiting for it here turns a remote host that cannot run the command into a connect-time
    // error rather than a session that dies a moment later for no stated reason.
    loop {
        match reader.wait().await {
            Some(msg) => match step(msg, in_tx) {
                Step::Started => return Ok((reader, writer)),
                Step::Refused => return Err("the remote host refused to run the xpra proxy \
                                             command".to_string()),
                Step::Done => return Err("the ssh channel closed before the remote xpra \
                                          started".to_string()),
                Step::Continue => {}
            },
            None => return Err("the ssh channel closed during startup".to_string()),
        }
    }
}

async fn pump(mut reader: ChannelReadHalf, writer: ChannelWriteHalf<Msg>,
              mut out_rx: UnboundedReceiver<Outgoing>, in_tx: Sender<Vec<u8>>) {
    // the write side is its own task so that waiting for data to arrive never blocks a send.
    let writes = tokio::spawn(async move {
        while let Some((bytes, ack)) = out_rx.recv().await {
            let result = writer.data_bytes(bytes).await
                .map_err(|e| io::Error::other(format!("ssh write failed: {e}")));
            // This yield is load-bearing, and measured: `data_bytes` returns once russh's session
            // task has been *handed* the bytes, and russh's own flush is a no-op, so without it the
            // last write before an exit never reaches the socket. Writing one packet and calling
            // `process::exit` immediately - which is what `disconnect_and_quit` amounts to - landed
            // 0 of 15 bytes at the far end without the yield and all 15 with it. The session task
            // shares this single-threaded runtime and became runnable when the message was queued,
            // so yielding lets it encrypt and write before the write is reported finished.
            tokio::task::yield_now().await;
            // a failed ack only means the writer gave up waiting; the bytes went out either way.
            let _ = ack.send(result);
        }
    });
    while let Some(msg) = reader.wait().await {
        if matches!(step(msg, &in_tx), Step::Done) {
            break;
        }
    }
    // dropping `in_tx` is what ends the client's reader thread, which reads EOF and reports the
    // connection lost through the usual path.
    writes.abort();
}

// What one channel message means for the stream.
enum Step {
    Continue,
    Started, // the reply to our `exec` request
    Refused, // ... or its refusal
    Done,
}

fn step(msg: ChannelMsg, in_tx: &Sender<Vec<u8>>) -> Step {
    match msg {
        ChannelMsg::Data { data } => {
            // an empty chunk would read back as EOF, so it must not be forwarded
            if data.is_empty() {
                return Step::Continue;
            }
            if in_tx.send(data.to_vec()).is_err() { Step::Done } else { Step::Continue }
        }
        // the remote command's stderr. The subprocess transport inherits ssh's stderr and gets this
        // for free; here it has to be logged, and it is where "no xpra command found" arrives.
        ChannelMsg::ExtendedData { data, .. } => {
            let text = String::from_utf8_lossy(&data);
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                warn!("ssh: {line}");
            }
            Step::Continue
        }
        ChannelMsg::Success => Step::Started,
        ChannelMsg::Failure => Step::Refused,
        ChannelMsg::ExitStatus { exit_status } => {
            info!("the remote xpra proxy exited with status {exit_status}");
            Step::Continue
        }
        ChannelMsg::Eof | ChannelMsg::Close => Step::Done,
        other => {
            debug!("ssh: ignoring {other:?}");
            Step::Continue
        }
    }
}

// The incoming half: whole chunks as the ssh thread hands them over, served through `Read` in
// whatever sizes the caller asks for.
struct Inbox {
    rx: Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    pos: usize,
}

impl Inbox {
    fn new(rx: Receiver<Vec<u8>>) -> Self {
        Inbox { rx, chunk: Vec::new(), pos: 0 }
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.chunk.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                // the ssh thread is gone: end of stream, which is how a closed session reads
                Err(_) => return Ok(0),
            }
        }
        let n = (self.chunk.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[derive(Clone)]
pub struct SshStream {
    out: UnboundedSender<Outgoing>,
    // shared rather than split so that `try_clone` hands out the same stream, as the other
    // transports' `try_clone` does. In practice only the reader thread ever reads.
    inbox: Arc<Mutex<Inbox>>,
}

impl SshStream {
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(SshStream { out: self.out.clone(), inbox: self.inbox.clone() })
    }
}

impl Read for SshStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inbox.lock().unwrap().read(buf)
    }
}

impl Write for SshStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let (ack_tx, ack_rx) = sync_channel(1);
        self.out.send((buf.to_vec(), ack_tx))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the ssh session has ended"))?;
        match ack_rx.recv() {
            Ok(Ok(())) => Ok(buf.len()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(io::Error::new(io::ErrorKind::BrokenPipe,
                                         "the ssh session ended mid-write")),
        }
    }

    // `write` has already waited for the bytes to go out, so there is nothing left to flush.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// `address` is `host:port` (the port is never absent - `parse_target` defaults it to 22), and the
// host keeps its brackets there for `ToSocketAddrs` but loses them for `known_hosts`, which records
// a bare IPv6 address.
fn split_address(address: &str) -> Result<(String, u16), String> {
    let (host, port) = address.rsplit_once(':')
        .ok_or_else(|| format!("missing port in {address:?}"))?;
    let port = port.parse::<u16>().map_err(|_| format!("bad port in {address:?}"))?;
    Ok((host.trim_start_matches('[').trim_end_matches(']').to_string(), port))
}

fn local_user() -> Option<String> {
    ["USER", "USERNAME", "LOGNAME"].iter()
        .filter_map(|name| env::var(name).ok())
        .find(|user| !user.is_empty())
}

fn known_hosts_path() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os(KNOWN_HOSTS_ENV).filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    auth::home().map(|home| home.join(".ssh").join("known_hosts"))
        .ok_or_else(|| format!("no home directory to find known_hosts in: set {KNOWN_HOSTS_ENV}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_splits_into_a_host_and_a_port() {
        assert_eq!(split_address("host:22"), Ok(("host".to_string(), 22)));
        assert_eq!(split_address("10.0.0.1:2222"), Ok(("10.0.0.1".to_string(), 2222)));
        // known_hosts records IPv6 addresses without the brackets the URI form needs
        assert_eq!(split_address("[::1]:22"), Ok(("::1".to_string(), 22)));
        assert!(split_address("host").is_err());
        assert!(split_address("host:port").is_err());
        assert!(split_address("host:99999").is_err());
    }

    // one chunk, read out in pieces smaller than it
    #[test]
    fn a_chunk_is_served_in_as_many_reads_as_it_takes() {
        let (tx, rx) = channel();
        let mut inbox = Inbox::new(rx);
        tx.send(b"abcde".to_vec()).unwrap();
        let mut buf = [0u8; 2];
        assert_eq!(inbox.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf, b"ab");
        assert_eq!(inbox.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf, b"cd");
        assert_eq!(inbox.read(&mut buf).unwrap(), 1);
        assert_eq!(buf[0], b'e');
    }

    // and the other way round: a buffer bigger than the chunk is not filled from the next one,
    // because that would mean blocking for data the caller does not need.
    #[test]
    fn a_read_stops_at_the_end_of_a_chunk() {
        let (tx, rx) = channel();
        let mut inbox = Inbox::new(rx);
        tx.send(b"ab".to_vec()).unwrap();
        tx.send(b"cd".to_vec()).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(inbox.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"ab");
        assert_eq!(inbox.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"cd");
    }

    #[test]
    fn a_closed_session_reads_as_end_of_stream() {
        let (tx, rx) = channel();
        let mut inbox = Inbox::new(rx);
        tx.send(b"ab".to_vec()).unwrap();
        drop(tx);
        let mut buf = [0u8; 8];
        assert_eq!(inbox.read(&mut buf).unwrap(), 2);
        // ... and stays there, however often it is asked
        assert_eq!(inbox.read(&mut buf).unwrap(), 0);
        assert_eq!(inbox.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn an_empty_buffer_reads_nothing_and_keeps_the_data() {
        let (tx, rx) = channel();
        let mut inbox = Inbox::new(rx);
        tx.send(b"ab".to_vec()).unwrap();
        assert_eq!(inbox.read(&mut []).unwrap(), 0);
        let mut buf = [0u8; 8];
        assert_eq!(inbox.read(&mut buf).unwrap(), 2);
    }

    // an empty chunk must never reach the inbox: it would read back as end of stream
    #[test]
    fn an_empty_data_message_is_dropped_rather_than_forwarded() {
        let (tx, rx) = channel();
        assert!(matches!(step(ChannelMsg::Data { data: Vec::new().into() }, &tx), Step::Continue));
        assert!(matches!(step(ChannelMsg::Data { data: b"xy".to_vec().into() }, &tx), Step::Continue));
        assert_eq!(rx.recv().unwrap(), b"xy");
    }

    #[test]
    fn the_channel_messages_that_end_the_stream() {
        let (tx, _rx) = channel();
        assert!(matches!(step(ChannelMsg::Eof, &tx), Step::Done));
        assert!(matches!(step(ChannelMsg::Close, &tx), Step::Done));
        assert!(matches!(step(ChannelMsg::Success, &tx), Step::Started));
        assert!(matches!(step(ChannelMsg::Failure, &tx), Step::Refused));
        assert!(matches!(step(ChannelMsg::ExitStatus { exit_status: 1 }, &tx), Step::Continue));
    }

    // a reader that has gone away ends the session rather than filling memory
    #[test]
    fn a_dropped_reader_ends_the_stream() {
        let (tx, rx) = channel();
        drop(rx);
        assert!(matches!(step(ChannelMsg::Data { data: b"xy".to_vec().into() }, &tx), Step::Done));
    }

    #[test]
    fn a_write_to_a_dead_session_is_a_broken_pipe() {
        let (out, out_rx) = unbounded_channel::<Outgoing>();
        let (_in_tx, in_rx) = channel();
        let mut stream = SshStream { out, inbox: Arc::new(Mutex::new(Inbox::new(in_rx))) };
        drop(out_rx);
        let e = stream.write(b"hello").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::BrokenPipe);
    }

    // a write is only finished once the ssh side says so, which is what `disconnect_and_quit`
    // depends on: no queue is left holding the goodbye packet.
    #[test]
    fn a_write_waits_for_the_ssh_side() {
        let (out, mut out_rx) = unbounded_channel::<Outgoing>();
        let (_in_tx, in_rx) = channel();
        let mut stream = SshStream { out, inbox: Arc::new(Mutex::new(Inbox::new(in_rx))) };
        let writer = thread::spawn(move || {
            let (bytes, ack) = out_rx.blocking_recv().expect("a write to acknowledge");
            assert_eq!(bytes, b"hello");
            ack.send(Ok(())).unwrap();
            // nothing acknowledges the second write, so it must not report success
            let (_, ack) = out_rx.blocking_recv().expect("a second write");
            drop(ack);
        });
        assert_eq!(stream.write(b"hello").unwrap(), 5);
        assert_eq!(stream.write(b"again").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        writer.join().unwrap();
    }
}
