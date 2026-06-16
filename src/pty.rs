//! PTY setup, terminal size queries, and the raw-mode guard.
//!
//! Step 1 of the build only needs the controlling terminal pieces: put the real
//! terminal into raw mode (restoring on drop, including on unwind), and read the
//! window size so we can size the child PTY and propagate `SIGWINCH` resizes.

use std::io;
use std::os::fd::RawFd;

use portable_pty::PtySize;

/// RAII guard that puts a terminal fd into raw mode and restores the previous
/// settings when dropped. Dropping happens on normal return *and* on unwind, so
/// a panic anywhere in the proxy loop still leaves the user's terminal sane.
pub struct RawModeGuard {
    fd: RawFd,
    original: libc::termios,
}

impl RawModeGuard {
    /// Enter raw mode on `fd`. Returns `Ok(None)` if `fd` is not a TTY (e.g.
    /// stdin is a pipe in a test), in which case there is nothing to restore.
    pub fn new(fd: RawFd) -> io::Result<Option<Self>> {
        // SAFETY: isatty is always safe to call on any fd.
        if unsafe { libc::isatty(fd) } != 1 {
            return Ok(None);
        }

        // SAFETY: we pass a properly-sized, zeroed termios and check return codes.
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut termios) != 0 {
                return Err(io::Error::last_os_error());
            }
            let original = termios;
            libc::cfmakeraw(&mut termios);
            if libc::tcsetattr(fd, libc::TCSANOW, &termios) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Some(Self { fd, original }))
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // SAFETY: restoring the saved termios on the same fd.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

/// Query the window size of the terminal attached to `fd`.
///
/// Returns a sensible 80x24 default if the ioctl fails (e.g. not a TTY) so the
/// child still gets a usable size rather than a 0x0 PTY.
pub fn terminal_size(fd: RawFd) -> PtySize {
    // SAFETY: zeroed winsize, fd is valid, we check the return code.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row != 0 {
            PtySize {
                rows: ws.ws_row,
                cols: ws.ws_col,
                pixel_width: ws.ws_xpixel,
                pixel_height: ws.ws_ypixel,
            }
        } else {
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            }
        }
    }
}
