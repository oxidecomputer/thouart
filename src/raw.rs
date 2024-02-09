// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub use platform_impl::*;

#[cfg(target_family = "windows")]
mod platform_impl {
    use std::os::windows::io::RawHandle;
    use thiserror::Error;
    use winapi::shared::minwindef::DWORD;
    use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
    use winapi::um::wincon::{
        DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    };
    use winapi::um::winnt::HANDLE;

    #[derive(Error, Debug)]
    pub enum Error {
        #[error("GetConsoleMode(hConsoleHandle, lpMode) call failed: {0}")]
        GetConsoleMode(std::io::Error),
        #[error("SetConsoleMode(hConsoleHandle, dwMode) call failed: {0}")]
        SetConsoleMode(std::io::Error),
    }

    /// Guard object that will set the terminal to raw mode and restore it
    /// to its previous state when it's dropped.
    pub struct RawModeGuard {
        in_handle: HANDLE,
        out_handle: HANDLE,
        in_mode: DWORD,
        out_mode: DWORD,
    }

    unsafe impl Send for RawModeGuard {}

    fn get_mode(handle: HANDLE) -> Result<DWORD, Error> {
        unsafe {
            let mut curr_mode = std::mem::zeroed();
            let r = GetConsoleMode(handle, &mut curr_mode);
            if r == 0 {
                return Err(Error::GetConsoleMode(std::io::Error::last_os_error()));
            }
            Ok(curr_mode)
        }
    }

    impl RawModeGuard {
        pub(crate) fn new(
            stdin_handle: RawHandle,
            stdout_handle: RawHandle,
        ) -> Result<Self, Error> {
            let in_handle = stdin_handle as HANDLE;
            let out_handle = stdout_handle as HANDLE;
            let in_mode = get_mode(in_handle)?;
            let out_mode = get_mode(out_handle)?;
            let guard = Self {
                in_handle,
                out_handle,
                in_mode,
                out_mode,
            };
            unsafe {
                let vt_out_mode = guard.out_mode
                    | ENABLE_VIRTUAL_TERMINAL_PROCESSING
                    | DISABLE_NEWLINE_AUTO_RETURN;
                let r = SetConsoleMode(out_handle, vt_out_mode);
                if r == 0 {
                    // may be only failing to disable newline auto-return, try without
                    let just_vt = guard.out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
                    let r = SetConsoleMode(out_handle, just_vt);
                    if r == 0 {
                        return Err(Error::SetConsoleMode(std::io::Error::last_os_error()));
                    }
                }
                let vt_in_mode = (guard.in_mode | ENABLE_VIRTUAL_TERMINAL_INPUT)
                    & !(ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT | ENABLE_ECHO_INPUT);
                let r = SetConsoleMode(in_handle, vt_in_mode);
                if r == 0 {
                    return Err(Error::SetConsoleMode(std::io::Error::last_os_error()));
                }
            }
            Ok(guard)
        }
    }

    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            unsafe {
                let r = SetConsoleMode(self.in_handle, self.in_mode);
                if r == 0 {
                    panic!(
                        "\r\n{}\r\n",
                        Error::SetConsoleMode(std::io::Error::last_os_error())
                    );
                }
                let r = SetConsoleMode(self.out_handle, self.out_mode);
                if r == 0 {
                    panic!(
                        "\r\n{}\r\n",
                        Error::SetConsoleMode(std::io::Error::last_os_error())
                    );
                }
            }
        }
    }
}

#[cfg(target_family = "unix")]
mod platform_impl {
    use std::os::fd::{FromRawFd, IntoRawFd, RawFd};
    use thiserror::Error;

    #[derive(Error, Debug)]
    pub enum Error {
        #[error("tcgetattr(stdout, termios) call failed: {0}")]
        TcGetAttr(std::io::Error),
        #[error("tcsetattr(stdout, {1}, termios) call failed: {0}")]
        TcSetAttr(std::io::Error, &'static str),
    }

    /// Guard object that will set the terminal to raw mode and restore it
    /// to its previous state when it's dropped. If it is unable to restore
    /// the previous termcap state, [Drop::drop] will panic.
    ///
    /// Additionally, if the terminfo database for the current terminal is
    /// available, the reset sequences from it are emitted to return it from
    /// any unknown state. Failures in finding or outputting these are ignored.
    pub struct RawModeGuard(libc::c_int, libc::termios);

    impl RawModeGuard {
        /// Attach to the terminal whose stdout is the given fd.
        pub(crate) fn new(fd: RawFd) -> Result<Self, Error> {
            let termios = unsafe {
                let mut curr_termios = std::mem::zeroed();
                let r = libc::tcgetattr(fd, &mut curr_termios);
                if r == -1 {
                    return Err(Error::TcGetAttr(std::io::Error::last_os_error()));
                }
                curr_termios
            };
            let guard = Self(fd, termios);
            unsafe {
                let mut raw_termios = termios;
                libc::cfmakeraw(&mut raw_termios);
                let r = libc::tcsetattr(fd, libc::TCSAFLUSH, &raw_termios);
                if r == -1 {
                    return Err(Error::TcSetAttr(
                        std::io::Error::last_os_error(),
                        "TCSAFLUSH",
                    ));
                }
            }
            Ok(guard)
        }
    }

    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            // reset the termcaps to what they were before we were constructed.
            // (similar to `stty sane` if the tty was that way to begin with)
            let r = unsafe { libc::tcsetattr(self.0, libc::TCSADRAIN, &self.1) };
            if r == -1 {
                // some \r\n because we might still be in a raw mode...
                panic!(
                    "\r\n{}\r\n",
                    Error::TcSetAttr(std::io::Error::last_os_error(), "TCSADRAIN")
                );
            }
            // if we have a terminfo database and this terminal is in it,
            // try to emit the strings to reset from an indeterminate state.
            // (similar to `tput reset`)
            if let Ok(info) = terminfo::Database::from_env() {
                let mut stdout = unsafe { std::fs::File::from_raw_fd(self.0) };
                if let Some(rs1) = info.get::<terminfo::capability::Reset1String>() {
                    rs1.expand().to(&mut stdout).unwrap();
                }
                if let Some(rs2) = info.get::<terminfo::capability::Reset2String>() {
                    rs2.expand().to(&mut stdout).unwrap();
                }
                // the order here comes from the X/Open Curses Issue 4 Version 2
                // technical standard, repeated in the terminfo(5) manpage:
                // "Sequences that do a reset from a totally unknown state
                // can be given as rs1, rs2, rf and rs3"
                if let Some(rf) = info.get::<terminfo::capability::ResetFile>() {
                    rf.expand()
                        .to_vec()
                        .ok()
                        .and_then(|path_vec| String::from_utf8(path_vec).ok())
                        .and_then(|path| std::fs::File::open(path).ok())
                        .and_then(|mut f| std::io::copy(&mut f, &mut stdout).ok());
                }
                if let Some(rs3) = info.get::<terminfo::capability::Reset3String>() {
                    rs3.expand().to(&mut stdout).unwrap();
                }
                // relinquish ownership - we may not want to close on drop
                stdout.into_raw_fd();
            }
        }
    }
}
