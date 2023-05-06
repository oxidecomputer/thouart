// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use regex::bytes::Regex;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Prefix length ({0}) is greater than length of escape string ({1})")]
    PrefixTooLong(usize, usize),
    #[error("Error compiling regular expression: {0}")]
    Regex(#[from] regex::Error),
}

// https://docs.rs/tokio/latest/tokio/io/trait.AsyncReadExt.html#method.read_exact
// is not cancel safe! Meaning reads from tokio::io::stdin are not cancel
// safe. Spawn a separate task to read and put bytes onto this channel.
pub(crate) async fn stdin_read_task<I: AsyncReadExt + Unpin + Send + 'static>(
    mut stdin: I,
    stdintx: mpsc::Sender<Vec<u8>>,
) {
    let mut inbuf = [0u8; 1024];
    loop {
        let n = match stdin.read(&mut inbuf).await {
            Err(_) | Ok(0) => break,
            Ok(n) => n,
        };

        if stdintx.send(inbuf[0..n].to_vec()).await.is_err() {
            break;
        }
    }
}

pub(crate) async fn stdin_relay_task(
    mut stdinrx: mpsc::Receiver<Vec<u8>>,
    wstx: mpsc::Sender<Vec<u8>>,
    mut escape: Option<EscapeSequence>,
) {
    while let Some(inbuf) = stdinrx.recv().await {
        if let Some(esc_sequence) = &mut escape {
            // process potential matches of our escape sequence to determine
            // whether we should exit the loop
            let (outbuf, exit) = esc_sequence.process(inbuf);

            // Send what we have, even if we're about to exit.
            if !outbuf.is_empty() {
                if wstx.send(outbuf).await.is_err() {
                    // if the channel's closed, no point going on
                    break;
                }
            }

            if exit {
                break;
            }
        } else {
            if wstx.send(inbuf).await.is_err() {
                break;
            }
        }
    }
}

/// Stateful abstraction for filtering input bytes to determine if the given
/// escape sequence has been detected, and what bytes are ready to be sent
/// for normal processing (i.e. aren't an incomplete match of the escape
/// sequence).
pub struct EscapeSequence {
    bytes: Vec<u8>,
    prefix_length: usize,

    // the following are member variables because their values persist between
    // invocations of EscapeSequence::process, because the relevant bytes of
    // the things for which we're checking likely won't all arrive at once.
    // ---
    // position of next potential match in the escape sequence
    esc_pos: usize,
    // buffer for accumulating characters that may be part of an ANSI Cursor
    // Position Report sent from xterm-likes that we should ignore (this will
    // otherwise render any escape sequence containing newlines before its
    // `prefix_length` unusable, if they're received by a shell that sends
    // requests for these reports for each newline received)
    ansi_curs_check: Vec<u8>,
    // pattern used for matching partial-to-complete versions of the above.
    // stored here such that it's only instantiated once at construction time.
    ansi_curs_pat: Regex,
}

impl EscapeSequence {
    /// If the given `bytes` sequence is ever fed through any successive or
    /// individual calls to [EscapeSequence::process], the client should exit.
    ///
    /// `prefix_length` is the number of bytes from the beginning of `bytes`
    /// that should be forwarded through prior to buffering inputs until a
    /// mismatch is encountered.
    ///
    /// For example, to mimic the enter-tilde-dot escape sequence behavior
    /// from SSH, such that the newline gets sent to the remote machine
    /// immediately while still continuing to check for the rest of the
    /// sequence and not send a subsequent `~` unless it's followed by
    /// something other than a `.`, you would construct this like so:
    /// ```
    /// thouart::EscapeSequence::new(b"\n~.".to_vec(), 1).unwrap();
    /// ```
    pub fn new(bytes: Vec<u8>, prefix_length: usize) -> Result<Self, Error> {
        let escape_len = bytes.len();
        if prefix_length > escape_len {
            return Err(Error::PrefixTooLong(prefix_length, escape_len));
        }
        // matches partial prefixes of 'CSI row ; column R' (e.g. "\x1b[14;30R")
        let ansi_curs_pat = Regex::new("^\x1b(\\[([0-9]+(;([0-9]+R?)?)?)?)?$")?;

        Ok(EscapeSequence {
            bytes,
            prefix_length,
            esc_pos: 0,
            ansi_curs_check: Vec::new(),
            ansi_curs_pat,
        })
    }

    /// Return the bytes we can safely commit to sending to the terminal, and
    /// determine if the user has entered the escape sequence completely.
    /// Returns true iff the program should exit.
    pub fn process(&mut self, inbuf: Vec<u8>) -> (Vec<u8>, bool) {
        // Put bytes from inbuf to outbuf, but don't send characters in the
        // escape string sequence unless we bail.
        let mut outbuf = Vec::with_capacity(inbuf.len());

        for c in inbuf {
            if !self.ignore_ansi_cpr_seq(&mut outbuf, c) {
                // is this char a match for the next byte of the sequence?
                if c == self.bytes[self.esc_pos] {
                    self.esc_pos += 1;
                    if self.esc_pos == self.bytes.len() {
                        // Exit on completed escape string
                        return (outbuf, true);
                    } else if self.esc_pos <= self.prefix_length {
                        // let through incomplete prefix up to the given limit
                        outbuf.push(c);
                    }
                } else {
                    // they bailed from the sequence,
                    // feed everything that matched so far through
                    if self.esc_pos != 0 {
                        outbuf.extend(&self.bytes[self.prefix_length..self.esc_pos])
                    }
                    self.esc_pos = 0;
                    outbuf.push(c);
                }
            }
        }
        (outbuf, false)
    }

    // ignore ANSI escape sequence for the Cursor Position Report sent by
    // xterm-likes in response to shells requesting one after each newline.
    // returns true if further processing of character `c` shouldn't apply
    // (i.e. we find a partial or complete match of the ANSI CSR pattern)
    fn ignore_ansi_cpr_seq(&mut self, outbuf: &mut Vec<u8>, c: u8) -> bool {
        if self.esc_pos > 0
            && self.esc_pos <= self.prefix_length
            && b"\r\n".contains(&self.bytes[self.esc_pos - 1])
        {
            self.ansi_curs_check.push(c);
            if self.ansi_curs_pat.is_match(&self.ansi_curs_check) {
                // end of the sequence?
                if c == b'R' {
                    outbuf.extend(&self.ansi_curs_check);
                    self.ansi_curs_check.clear();
                }
                return true;
            } else {
                self.ansi_curs_check.pop(); // we're not `continue`ing
                outbuf.extend(&self.ansi_curs_check);
                self.ansi_curs_check.clear();
            }
        }
        false
    }
}

#[tokio::test]
async fn test_stdin_relay_task() {
    use tokio::sync::mpsc::error::TryRecvError;

    let (stdintx, stdinrx) = tokio::sync::mpsc::channel(16);
    let (wstx, mut wsrx) = tokio::sync::mpsc::channel(16);

    let escape = Some(EscapeSequence::new(vec![0x1d, 0x03], 0).unwrap());
    tokio::spawn(async move { stdin_relay_task(stdinrx, wstx, escape).await });

    // send characters, receive characters
    stdintx
        .send("test post please ignore".chars().map(|c| c as u8).collect())
        .await
        .unwrap();
    let actual = wsrx.recv().await.unwrap();
    assert_eq!(
        String::from_utf8(actual).unwrap(),
        "test post please ignore"
    );

    // don't send a started escape sequence
    stdintx
        .send("\x1d".chars().map(|c| c as u8).collect())
        .await
        .unwrap();
    assert_eq!(wsrx.try_recv(), Err(TryRecvError::Empty));

    // since we didn't enter the \x03, the previous \x1d shows up here
    stdintx
        .send("test".chars().map(|c| c as u8).collect())
        .await
        .unwrap();
    let actual = wsrx.recv().await.unwrap();
    assert_eq!(String::from_utf8(actual).unwrap(), "\x1dtest");

    // \x03 gets sent if not preceded by \x1d
    stdintx
        .send("\x03".chars().map(|c| c as u8).collect())
        .await
        .unwrap();
    let actual = wsrx.recv().await.unwrap();
    assert_eq!(String::from_utf8(actual).unwrap(), "\x03");

    // \x1d followed by \x03 means exit, even if they're separate messages
    stdintx
        .send("\x1d".chars().map(|c| c as u8).collect())
        .await
        .unwrap();
    stdintx
        .send("\x03".chars().map(|c| c as u8).collect())
        .await
        .unwrap();
    assert_eq!(wsrx.try_recv(), Err(TryRecvError::Empty));

    // channel is closed
    assert!(wsrx.recv().await.is_none());
}
