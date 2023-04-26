mod input;
mod raw;

use crate::input::{stdin_read_task, stdin_relay_task, EscapeSequence};
use crate::raw::RawTermiosGuard;
use std::os::fd::AsRawFd;

use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed to set raw mode: {0}")]
    RawMode(#[from] raw::Error),
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

    pub async fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), std::io::Error> {
        self.stdout.write_all(bytes).await?;
        self.stdout.flush().await?;
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
