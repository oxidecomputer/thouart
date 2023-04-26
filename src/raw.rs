use std::os::fd::RawFd;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("tcgetattr(stdout, termios) call failed: {0}")]
    TcGetAttr(std::io::Error),
    #[error("tcsetattr(stdout, TCSAFLUSH, termios) call failed: {0}")]
    TcSetAttr(std::io::Error),
}

/// Guard object that will set the terminal to raw mode and restore it
/// to its previous state when it's dropped
pub struct RawTermiosGuard(libc::c_int, libc::termios);

impl RawTermiosGuard {
    pub(crate) fn stdio_guard(fd: RawFd) -> Result<RawTermiosGuard, Error> {
        let termios = unsafe {
            let mut curr_termios = std::mem::zeroed();
            let r = libc::tcgetattr(fd, &mut curr_termios);
            if r == -1 {
                return Err(Error::TcGetAttr(std::io::Error::last_os_error()));
            }
            curr_termios
        };
        let guard = RawTermiosGuard(fd, termios);
        unsafe {
            let mut raw_termios = termios;
            libc::cfmakeraw(&mut raw_termios);
            let r = libc::tcsetattr(fd, libc::TCSAFLUSH, &raw_termios);
            if r == -1 {
                return Err(Error::TcSetAttr(std::io::Error::last_os_error()));
            }
        }
        Ok(guard)
    }
}

impl Drop for RawTermiosGuard {
    fn drop(&mut self) {
        let r = unsafe { libc::tcsetattr(self.0, libc::TCSADRAIN, &self.1) };
        if r == -1 {
            Err::<(), _>(std::io::Error::last_os_error()).unwrap();
        }
    }
}
