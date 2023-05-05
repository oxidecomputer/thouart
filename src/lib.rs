/// This crate provides some convenience code for implementing an interactive
/// raw-mode terminal interface in a CLI tool. See [Console].
mod input;
mod raw;

pub use crate::input::EscapeSequence;
pub use crate::raw::RawTermiosGuard;

use crate::input::{stdin_read_task, stdin_relay_task};
use std::os::fd::AsRawFd;

use futures::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed to set raw mode: {0}")]
    RawMode(#[from] crate::raw::Error),
    #[error("Websocket error: {0}")]
    WebsocketError(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("Writing to stdout: {0}")]
    StdoutWrite(#[from] std::io::Error),
}

/// A simple abstraction over async I/O providing cancel-safe input suitable
/// for use in e.g. `tokio::select!`, built-in support for escape-sequences
/// to end the input stream, and a provided event loop for attaching to raw
/// terminal bytes being sent to/from a WebSocket as Binary frames (provided
/// no other WebSocket frames need to be handled). Holds a [RawTermiosGuard],
/// and thus puts the given output into raw-mode, until dropped.
pub struct Console<O: AsyncWriteExt + Unpin + Send> {
    stdout: O,
    relay_rx: mpsc::Receiver<Vec<u8>>,
    read_handle: task::JoinHandle<()>,
    relay_handle: task::JoinHandle<()>,
    raw_guard: Option<RawTermiosGuard>,
}

impl<O: AsyncWriteExt + Unpin + Send + AsRawFd> Console<O> {
    /// Construct with arbitrary [AsyncReadExt] and [AsyncWriteExt] streams,
    /// supporting use cases where we might be talking to something other than
    /// the same terminal from which a tool is being invoked.
    pub async fn new<I: AsyncReadExt + Unpin + Send + 'static>(
        stdin: I,
        stdout: O,
        escape: Option<EscapeSequence>,
    ) -> Result<Self, Error> {
        let raw_guard = Some(RawTermiosGuard::stdio_guard(stdout.as_raw_fd())?);
        Ok(Self::new_inner(stdin, stdout, escape, raw_guard))
    }
}

impl Console<tokio::io::Stdout> {
    /// Construct with the normal stdin and stdout file descriptors, for
    /// typical use.
    pub async fn new_stdio(escape: Option<EscapeSequence>) -> Result<Self, Error> {
        Console::new(tokio::io::stdin(), tokio::io::stdout(), escape).await
    }
}

impl<O: AsyncWriteExt + Unpin + Send> Console<O> {
    fn new_inner<I: AsyncReadExt + Unpin + Send + 'static>(
        stdin: I,
        stdout: O,
        escape: Option<EscapeSequence>,
        raw_guard: Option<RawTermiosGuard>,
    ) -> Self {
        let (read_tx, read_rx) = mpsc::channel(16);
        let (relay_tx, relay_rx) = mpsc::channel(16);

        let read_handle = tokio::spawn(stdin_read_task(stdin, read_tx));
        let relay_handle = tokio::spawn(stdin_relay_task(read_rx, relay_tx, escape));

        Self {
            relay_rx,
            stdout,
            read_handle,
            relay_handle,
            raw_guard,
        }
    }

    /// Receive safe bytes from stdin (already processed for escape sequence
    /// matches), or `None` if the stream has closed (e.g. the escape sequence
    /// was entered by the user).
    pub async fn read_stdin(&mut self) -> Option<Vec<u8>> {
        self.relay_rx.recv().await
    }

    /// Write the given bytes to stdout.
    pub async fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.stdout.write_all(bytes).await?;
        self.stdout.flush().await?;
        Ok(())
    }

    /// An event loop that, given a raw upgraded WebSocket stream implementing
    /// [AsyncRead] + [AsyncWrite] (e.g. `Upgraded` from `reqwest` or `hyper`),
    /// forwards Binary frames to and from stdin and stdout until either the
    /// input stream is closed (e.g. by escape sequence), the remote end has
    /// sent a Close frame, or the underlying connection has itself terminated.
    /// Does not offer support for handling any other frame types, such as
    /// Text, which may make this unsuitable for use cases involving them.
    pub async fn attach_to_websocket(
        &mut self,
        upgraded: impl AsyncRead + AsyncWrite + Unpin,
    ) -> Result<(), Error> {
        let mut ws_stream = WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await;
        loop {
            tokio::select! {
                in_buf = self.read_stdin() => {
                    match in_buf {
                        Some(data) => {
                            ws_stream.send(Message::Binary(data)).await?;
                        }
                        None => break,
                    }
                }
                out_buf = ws_stream.next() => {
                    match out_buf {
                        Some(Ok(Message::Binary(data))) => self.write_stdout(&data).await?,
                        Some(Ok(Message::Close(..))) | None => break,
                        _ => continue,
                    }
                }
            }
        }
        ws_stream.send(Message::Close(None)).await?;
        Ok(())
    }
}

impl<O: AsyncWriteExt + Unpin + Send> Drop for Console<O> {
    fn drop(&mut self) {
        self.relay_handle.abort();
        self.read_handle.abort();
        self.raw_guard.take();
    }
}

#[cfg(test)]
mod tests {
    use crate::{Console, EscapeSequence};
    use futures::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;
    use tokio_tungstenite::tungstenite::protocol::Role;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::WebSocketStream;

    #[tokio::test]
    async fn test_websocket_loop() {
        let (mut in_testdrv, in_console) = tokio::io::duplex(16);
        let (mut out_testdrv, out_console) = tokio::io::duplex(16);
        let (ws_testdrv, ws_console) = tokio::io::duplex(16);

        let mut ws = WebSocketStream::from_raw_socket(ws_testdrv, Role::Server, None).await;

        let escape = Some(EscapeSequence::new(vec![1, 2, 3], 1).unwrap());
        let mut console = Console::new_inner(in_console, out_console, escape, None);

        let join_handle = tokio::spawn(async move {
            console.attach_to_websocket(ws_console).await.unwrap();
        });

        ws.send(Message::Binary(vec![1, 2, 3, 4, 5, 6]))
            .await
            .unwrap();

        let mut read_buf = [0u8; 6];
        const ONE_SEC: Duration = Duration::from_secs(1);
        timeout(ONE_SEC, out_testdrv.read_exact(&mut read_buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read_buf, [1, 2, 3, 4, 5, 6]);

        // EscapeSequence is [1, 2, 3] with prefix_length 1, so only the first
        // [0, 1] should be sent through so far.
        in_testdrv.write(&[0, 1, 2]).await.unwrap();
        let msg = timeout(ONE_SEC, ws.next()).await.unwrap().unwrap().unwrap();
        assert_eq!(msg, Message::Binary(vec![0, 1]));

        // this isn't 3, so this should bail from the EscapeSequence and send
        // the previously-witheld 2 now we know it's not part of an escape.
        in_testdrv.write(&[4, 5]).await.unwrap();
        let msg = timeout(ONE_SEC, ws.next()).await.unwrap().unwrap().unwrap();
        assert_eq!(msg, Message::Binary(vec![2, 4, 5]));

        // this should trigger EscapeSequence and send a Close frame.
        in_testdrv.write(&[0, 1, 2, 3, 4]).await.unwrap();
        let msg = timeout(ONE_SEC, ws.next()).await.unwrap().unwrap().unwrap();
        assert_eq!(msg, Message::Binary(vec![0, 1]));
        let msg = timeout(ONE_SEC, ws.next()).await.unwrap().unwrap().unwrap();
        assert_eq!(msg, Message::Close(None));

        // ...and end the event loop.
        timeout(ONE_SEC, join_handle).await.unwrap().unwrap();
    }
}
