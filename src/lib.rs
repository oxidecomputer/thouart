// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! This crate provides some convenience code for implementing an interactive
//! raw-mode terminal interface in a CLI tool. See [Console].

mod input;
mod raw;

pub use crate::input::EscapeSequence;
pub use crate::raw::RawModeGuard;

use crate::input::{stdin_read_task, stdin_relay_task};

#[cfg(target_family = "unix")]
use std::os::fd::AsRawFd as AsRawFdHandle;
#[cfg(target_family = "windows")]
use std::os::windows::io::AsRawHandle as AsRawFdHandle;

use futures::stream::FuturesUnordered;
#[cfg(target_family = "unix")]
use tokio::signal::unix::{signal, SignalKind};
#[cfg(target_family = "windows")]
use tokio::signal::windows::*;

use futures::{FutureExt, SinkExt, StreamExt};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role};
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
    #[error("Server error: {0}")]
    ServerError(String),
    #[error("Terminated by signal: {0}")]
    Signal(&'static str),
}

/// A simple abstraction over a TTY's async I/O streams.
///
/// It provides:
/// - cancel-safe access to user input via [`Console::read_stdin`], suitable for
///   use in [`tokio::select!`]
/// - an implementation of escape-sequences which, when received from the user,
///   will end the stream early.
/// - [`Console::attach_to_websocket`], which will bidirectionally forward data
///   between a WebSocket and the wrapped console streams used to construct the
///   `Console` until a termination condition is met (see function docs for
///   details).
///
/// Typically one will use stdin/stdout, which can be constructed with
/// [`Console::<tokio::io::Stdout>::new_stdio`] but other TTYs, (e.g. COM
/// ports, `/dev/ttyUSB*`, etc.) may be used. Non-TTY streams are not currently
/// supported.
///
/// `Console` places the provided output into raw-mode when created and restores
/// the output it to its previous state when dropped.
pub struct Console<O: AsyncWriteExt + Unpin + Send> {
    stdout: O,
    relay_rx: mpsc::Receiver<Vec<u8>>,
    read_handle: task::JoinHandle<()>,
    relay_handle: task::JoinHandle<()>,
    raw_guard: Option<RawModeGuard>,
}

impl Console<tokio::io::Stdout> {
    /// Construct with the normal stdin and stdout file descriptors, for
    /// typical use.
    pub async fn new_stdio(escape: Option<EscapeSequence>) -> Result<Self, Error> {
        Console::new(tokio::io::stdin(), tokio::io::stdout(), escape).await
    }
}

impl<O: AsyncWriteExt + Unpin + Send + AsRawFdHandle> Console<O> {
    /// Construct with arbitrary [AsyncReadExt] and [AsyncWriteExt] streams,
    /// supporting use cases where we might be talking to something other than
    /// the same terminal from which a tool is being invoked.
    pub async fn new<I: AsyncReadExt + Unpin + Send + AsRawFdHandle + 'static>(
        stdin: I,
        stdout: O,
        escape: Option<EscapeSequence>,
    ) -> Result<Self, Error> {
        #[cfg(target_family = "unix")]
        let raw_guard = Some(RawModeGuard::new(stdout.as_raw_fd())?);
        #[cfg(target_family = "windows")]
        let raw_guard = Some(RawModeGuard::new(
            stdin.as_raw_handle(),
            stdout.as_raw_handle(),
        )?);
        Ok(Self::new_inner(stdin, stdout, escape, raw_guard))
    }
}

// this is really silly. in order to test reasonably with a tokio::io::duplex,
// which doesn't impl AsRawHandle, we only use the trait bound required by the
// raw Win32 API call path in non-tests, and use a dummy trait here otherwise
// to reduce code duplication.
#[cfg(any(test, not(target_family = "windows")))]
use std::marker::Sized as MightBeRawHandle;
#[cfg(all(not(test), target_family = "windows"))]
use std::os::windows::io::AsRawHandle as MightBeRawHandle;

impl<O: AsyncWriteExt + Unpin + Send + MightBeRawHandle> Console<O> {
    fn new_inner<I: AsyncReadExt + Unpin + Send + 'static>(
        stdin: I,
        stdout: O,
        escape: Option<EscapeSequence>,
        raw_guard: Option<RawModeGuard>,
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
        // windows io in rust fails if any byte sequences aren't valid utf8
        #[cfg(all(not(test), target_family = "windows"))]
        {
            use winapi::shared::minwindef::LPDWORD;
            use winapi::um::winnt::{HANDLE, VOID};
            let mut _lp_num_of_chars_written = 0u32;
            let res = unsafe {
                winapi::um::consoleapi::WriteConsoleA(
                    self.stdout.as_raw_handle() as HANDLE,
                    bytes.as_ptr() as *const VOID,
                    bytes.len() as u32,
                    (&mut _lp_num_of_chars_written) as LPDWORD,
                    std::ptr::null_mut::<VOID>(),
                )
            };
            if res == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(any(test, not(target_family = "windows")))]
        {
            self.stdout.write_all(bytes).await?;
            self.stdout.flush().await?;
        }
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
        // need Signal structs to live at least as long as their futures
        let mut signal_storage = Vec::new();

        let mut signaled = FuturesUnordered::new();

        #[cfg(target_family = "unix")]
        {
            signal_storage.push((signal(SignalKind::hangup())?, "HUP"));
            signal_storage.push((signal(SignalKind::interrupt())?, "INT"));
            signal_storage.push((signal(SignalKind::pipe())?, "PIPE"));
            signal_storage.push((signal(SignalKind::quit())?, "QUIT"));
            signal_storage.push((signal(SignalKind::terminate())?, "TERM"));
        }
        #[cfg(target_family = "windows")]
        {
            // no ctrl_c(), we're already in VT100 mode, and raw mode in that
            signal_storage.push((WinCtrlSignal::CBreak(ctrl_break()?), "CTRL-BREAK"));
            signal_storage.push((WinCtrlSignal::CClose(ctrl_close()?), "CTRL-CLOSE"));
            signal_storage.push((WinCtrlSignal::CLogoff(ctrl_logoff()?), "CTRL-LOGOFF"));
            signal_storage.push((WinCtrlSignal::CShutdown(ctrl_shutdown()?), "CTRL-SHUTDOWN"));
        }

        for (s_fut, s_name) in &mut signal_storage {
            signaled.push(s_fut.recv().then(|opt| async move { opt.map(|_| s_name) }));
        }

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
                        Some(Ok(Message::Close(Some(CloseFrame {code, reason})))) => {
                            eprint!("\r\nConnection closed: {:?}\r\n", code);
                            match code {
                                CloseCode::Abnormal
                                | CloseCode::Error
                                | CloseCode::Extension
                                | CloseCode::Invalid
                                | CloseCode::Policy
                                | CloseCode::Protocol
                                | CloseCode::Size
                                | CloseCode::Unsupported => {
                                    return Err(Error::ServerError(reason.to_string()));
                                }
                                _ => break,
                            }
                        }
                        Some(Ok(Message::Close(None))) => {
                            eprint!("\r\nConnection closed.\r\n");
                            break;
                        }
                        None => {
                            eprint!("\r\nConnection lost.\r\n");
                            break;
                        }
                        _ => continue,
                    }
                }
                Some(Some(signal_name)) = signaled.next() => {
                    eprint!("\r\nExiting on signal.\r\n");
                    return Err(Error::Signal(signal_name));
                }
            }
        }
        // let _: the connection may have already been dropped at this point.
        let _ = ws_stream.send(Message::Close(None)).await;
        Ok(())
    }
}

// unfortunately tokio::signal makes these all separate types...
#[cfg(target_family = "windows")]
enum WinCtrlSignal {
    CC(CtrlC),
    CBreak(CtrlBreak),
    CClose(CtrlClose),
    CLogoff(CtrlLogoff),
    CShutdown(CtrlShutdown),
}
#[cfg(target_family = "windows")]
impl WinCtrlSignal {
    async fn recv(&mut self) -> Option<()> {
        match self {
            Self::CC(c) => c.recv().await,
            Self::CBreak(c) => c.recv().await,
            Self::CClose(c) => c.recv().await,
            Self::CLogoff(c) => c.recv().await,
            Self::CShutdown(c) => c.recv().await,
        }
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

    #[tokio::test]
    async fn test_cleanup_on_signal() {
        let (_in_testdrv, in_console) = tokio::io::duplex(16);
        let (mut out_testdrv, out_console) = tokio::io::duplex(16);
        let (ws_testdrv, ws_console) = tokio::io::duplex(16);

        let mut ws = WebSocketStream::from_raw_socket(ws_testdrv, Role::Server, None).await;
        let mut console = Console::new_inner(in_console, out_console, None, None);

        let join_handle =
            tokio::spawn(async move { console.attach_to_websocket(ws_console).await });

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

        #[cfg(target_family = "unix")]
        {
            let syscall_return =
                unsafe { libc::kill(std::process::id() as libc::c_int, libc::SIGINT) };
            assert_eq!(syscall_return, 0);

            let Err(super::Error::Signal("INT")) =
                timeout(ONE_SEC, join_handle).await.unwrap().unwrap()
            else {
                panic!("Expected SIGINT!")
            };
        }
        #[cfg(target_family = "windows")]
        {
            use winapi::um::wincon::*;
            let syscall_return = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, 0) };
            assert_ne!(syscall_return, 0);

            let Err(super::Error::Signal("CTRL-BREAK")) =
                timeout(ONE_SEC, join_handle).await.unwrap().unwrap()
            else {
                panic!("Expected CTRL-BREAK!")
            };
        }
    }
}
