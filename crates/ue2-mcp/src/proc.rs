//! Process control that differs between Unix and Windows (docs/specs/S22-windows.md §3), and the instance watchdog
//! (`ue2-mcp --watchdog PID PORT`).

use std::io::{BufRead, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

/// How hard to stop a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// SIGTERM; on Windows there is no request a console-less child can answer, so this terminates too.
    Term,
    /// SIGKILL; TerminateProcess on Windows.
    Kill,
}

impl Stop {
    pub fn name(self) -> &'static str {
        match (self, cfg!(windows)) {
            (_, true) => "TerminateProcess",
            (Stop::Term, false) => "SIGTERM",
            (Stop::Kill, false) => "SIGKILL",
        }
    }
}

/// Stop `pid` (it must be a process this server started and has not reaped).
pub fn stop(pid: u32, how: Stop) {
    #[cfg(unix)]
    {
        let sig = match how {
            Stop::Term => libc::SIGTERM,
            Stop::Kill => libc::SIGKILL,
        };
        // SAFETY: plain kill(2).
        unsafe {
            libc::kill(pid as i32, sig);
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        let _ = how;
        // SAFETY: a handle this block opens and closes; TerminateProcess on it.
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }
}

/// Whether `pid` names a running process (someone else's counts).
pub fn alive(pid: i64) -> bool {
    if pid <= 0 || pid > i64::from(i32::MAX) {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks for existence. EPERM means it exists but belongs to someone else.
        let exists = unsafe { libc::kill(pid as i32, 0) == 0 };
        exists || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        // SAFETY: a handle this block opens and closes; the exit code is written into a local.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if h.is_null() {
                // Access denied still means it exists.
                return std::io::Error::last_os_error().raw_os_error() == Some(5);
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            ok && code == STILL_ACTIVE as u32
        }
    }
}

/// The signal that ended a process (Unix only).
pub fn exit_signal(status: &ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    return std::os::unix::process::ExitStatusExt::signal(status);
    #[cfg(windows)]
    {
        let _ = status;
        None
    }
}

/// Keep a child out of the terminal's reach: its own process group (Unix) or process group plus no console window
/// (Windows), so a Ctrl-C aimed at the MCP client does not kill it before a clean `quit`.
pub fn detach(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
}

/// `ue2-mcp --watchdog PID PORT`: stop the emulator when the server that started it dies without cleaning up.
///
/// macOS has no parent-death signal and ue2emu does not watch stdin, so the watchdog blocks reading a pipe whose
/// write end only the server holds. A clean `emu_stop` writes `done`; any other end of the pipe (EOF because the
/// server died) makes it quit the emulator through its control port (flash is saved), then stop it if it is still
/// there. Returns the process exit code.
pub fn watchdog(args: &[String]) -> i32 {
    let pid = args.first().and_then(|a| a.parse::<u32>().ok());
    let port = args.get(1).and_then(|a| a.parse::<u16>().ok());
    let (Some(pid), Some(port)) = (pid, port) else {
        eprintln!("ue2-mcp --watchdog: expected PID PORT");
        return 2;
    };
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    if line.trim() == "done" || !alive(i64::from(pid)) {
        return 0;
    }
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    if let Ok(mut conn) = TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
        let _ = conn.set_write_timeout(Some(Duration::from_secs(3)));
        let _ = conn.write_all(b"quit\n");
    }
    let gone_within = |d: Duration| {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if !alive(i64::from(pid)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        !alive(i64::from(pid))
    };
    if !gone_within(Duration::from_secs(3)) {
        stop(pid, Stop::Term);
        if !gone_within(Duration::from_secs(2)) {
            stop(pid, Stop::Kill);
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive_and_nonsense_is_not() {
        assert!(alive(i64::from(std::process::id())));
        assert!(!alive(0) && !alive(-3) && !alive(i64::from(u32::MAX)));
    }
}
