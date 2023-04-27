mod input;
mod raw;

pub use crate::input::EscapeSequence;

use crate::input::{stdin_read_task, stdin_relay_task};
use crate::raw::RawTermiosGuard;
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

pub struct Console<O: AsyncWriteExt + Unpin + Send> {
    stdout: O,
    relay_rx: mpsc::Receiver<Vec<u8>>,
    read_handle: task::JoinHandle<()>,
    relay_handle: task::JoinHandle<()>,
    raw_guard: Option<RawTermiosGuard>,
}

impl<O: AsyncWriteExt + Unpin + Send + AsRawFd> Console<O> {
    pub async fn new<I: AsyncReadExt + Unpin + Send + 'static>(
        stdin: I,
        stdout: O,
        escape: Option<EscapeSequence>,
    ) -> Result<Self, Error> {
        let raw_guard = Some(RawTermiosGuard::stdio_guard(stdout.as_raw_fd())?);
        Ok(Self::new_inner(stdin, stdout, escape, raw_guard))
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

    pub async fn read_stdin(&mut self) -> Option<Vec<u8>> {
        self.relay_rx.recv().await
    }

    pub async fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.stdout.write_all(bytes).await?;
        self.stdout.flush().await?;
        Ok(())
    }

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

        Ok(())
    }
}

impl<O: AsyncWriteExt + Unpin + Send> Drop for Console<O> {
    fn drop(&mut self) {
        self.relay_handle.abort();
        self.read_handle.abort();
        self.raw_guard.take();
        // TODO tokio::spawn(self.stdout.write("\n".as_bytes()));
    }
}
