//! ConPTY backend — runs a persistent shell inside a Windows pseudo-console.
//!
//! Unlike Cmd/Pwsh backends (which spawn `cmd /C <command>` per call),
//! ConPtyBackend maintains one long-lived shell process. Commands are written
//! to the PTY input pipe; output (raw VT/ANSI bytes) is read from the output
//! pipe by a background thread.

use std::collections::HashMap;
use std::io::Write;
use std::sync::mpsc::{self, Receiver};

use wst_protocol::{BackendKind, ExecRequest, OutputChunk, SessionEvent, SessionId, TaskId};

use crate::{Backend, BackendError};

// ── Windows handle wrappers (Send-safe) ───────────────────────────────────────

/// Wrapper around a raw Windows HANDLE value (isize) that is safe to Send.
/// We store the raw isize rather than windows::Win32::Foundation::HANDLE
/// to avoid the `*mut c_void` Send restriction.
#[cfg(windows)]
struct OwnedHandle(isize);

#[cfg(windows)]
unsafe impl Send for OwnedHandle {}

/// Wrapper around HPCON (also a raw isize).
#[cfg(windows)]
struct OwnedHPCON(isize);

#[cfg(windows)]
unsafe impl Send for OwnedHPCON {}

// ── Session state ─────────────────────────────────────────────────────────────

struct Session {
    /// Write end of the PTY input pipe — send characters to the shell here.
    input_write: std::fs::File,
    /// Receives raw VT bytes from the background reader thread.
    output_rx: Receiver<Vec<u8>>,
    /// Leftover bytes from the last poll (partial UTF-8 line, no trailing \n).
    pending: Vec<u8>,
    /// Most recently issued task id.
    current_task: TaskId,

    // Windows handles kept alive for cleanup.
    #[cfg(windows)]
    hpcon: OwnedHPCON,
    #[cfg(windows)]
    proc_handle: OwnedHandle,
    #[cfg(windows)]
    thread_handle: OwnedHandle,
}

// ── ConPtyBackend ─────────────────────────────────────────────────────────────

pub struct ConPtyBackend {
    /// Shell executable (e.g. "cmd.exe", "powershell.exe").
    shell: String,
    /// PTY width in columns.
    cols: u16,
    /// PTY height in rows.
    rows: u16,
    sessions: HashMap<SessionId, Session>,
    next_session: SessionId,
    next_task: TaskId,
}

impl ConPtyBackend {
    pub fn new(shell: impl Into<String>) -> Self {
        Self {
            shell: shell.into(),
            cols: 220,
            rows: 50,
            sessions: HashMap::new(),
            next_session: 1,
            next_task: 1,
        }
    }

    pub fn cmd() -> Self { Self::new("cmd.exe") }
    pub fn pwsh() -> Self { Self::new("powershell.exe") }
}

// ── Windows implementation ────────────────────────────────────────────────────

#[cfg(windows)]
mod win_impl {
    use super::*;
    use std::io::Read;
    use std::mem::size_of;
    use std::os::windows::io::FromRawHandle;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Console::{
        ClosePseudoConsole, CreatePseudoConsole, COORD, HPCON,
    };
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList,
        InitializeProcThreadAttributeList, UpdateProcThreadAttribute,
        EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, STARTUPINFOW,
        STARTUPINFOEXW,
    };
    use windows::core::PWSTR;

    // PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE = 0x00020016
    const ATTR_PSEUDOCONSOLE: usize = 0x0002_0016;

    pub unsafe fn make_session(shell: &str, cols: u16, rows: u16) -> Result<Session, BackendError> {
        // ── Create two pipe pairs ─────────────────────────────────────────────
        let mut pty_in_rd = HANDLE::default();
        let mut pty_in_wr = HANDLE::default();
        CreatePipe(&mut pty_in_rd, &mut pty_in_wr, None, 0)
            .map_err(|e| BackendError::Other(format!("CreatePipe(in): {}", e)))?;

        let mut pty_out_rd = HANDLE::default();
        let mut pty_out_wr = HANDLE::default();
        CreatePipe(&mut pty_out_rd, &mut pty_out_wr, None, 0)
            .map_err(|e| BackendError::Other(format!("CreatePipe(out): {}", e)))?;

        // ── Create the pseudo console ─────────────────────────────────────────
        // In windows 0.58, CreatePseudoConsole returns Result<HPCON> directly
        let size = COORD { X: cols as i16, Y: rows as i16 };
        let hpcon: HPCON = CreatePseudoConsole(size, pty_in_rd, pty_out_wr, 0)
            .map_err(|e| BackendError::Other(format!("CreatePseudoConsole: {}", e)))?;

        // The PTY owns these ends now; close our copies.
        let _ = CloseHandle(pty_in_rd);
        let _ = CloseHandle(pty_out_wr);

        // ── Build STARTUPINFOEXW with the ConPTY attribute ────────────────────
        let mut attr_size = 0usize;
        // First call is expected to fail with ERROR_INSUFFICIENT_BUFFER;
        // it sets attr_size to the required allocation.
        let _ = InitializeProcThreadAttributeList(
            windows::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST(
                std::ptr::null_mut(),
            ),
            1,
            0,
            &mut attr_size,
        );

        let mut attr_buf = vec![0u8; attr_size];
        let attr_list = windows::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST(
            attr_buf.as_mut_ptr() as *mut _,
        );
        InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size)
            .map_err(|e| BackendError::Other(format!("InitProcThreadAttrList: {}", e)))?;

        let hpcon_val: isize = hpcon.0;
        UpdateProcThreadAttribute(
            attr_list,
            0,
            ATTR_PSEUDOCONSOLE,
            Some(&hpcon_val as *const isize as *const std::ffi::c_void),
            size_of::<isize>(),
            None,
            None,
        )
        .map_err(|e| BackendError::Other(format!("UpdateProcThreadAttr: {}", e)))?;

        let si_ex = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: size_of::<STARTUPINFOEXW>() as u32,
                ..Default::default()
            },
            lpAttributeList: attr_list,
        };

        // ── Launch the shell inside the pseudo console ────────────────────────
        let mut cmd_line: Vec<u16> = shell.encode_utf16().chain(std::iter::once(0)).collect();
        let mut pi = PROCESS_INFORMATION::default();
        CreateProcessW(
            None,
            PWSTR(cmd_line.as_mut_ptr()),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT,
            None,
            None,
            // Cast STARTUPINFOEXW* → STARTUPINFOW* (first field is the same struct)
            &si_ex.StartupInfo as *const STARTUPINFOW,
            &mut pi,
        )
        .map_err(|e| BackendError::Other(format!("CreateProcessW: {}", e)))?;

        DeleteProcThreadAttributeList(attr_list);

        // ── Wrap handles in safe Rust types ──────────────────────────────────
        let input_write =
            std::fs::File::from_raw_handle(pty_in_wr.0 as *mut std::ffi::c_void);

        // Wrap the raw output-read handle as an integer so it can safely cross the thread boundary.
        // SAFETY: We transfer ownership of this handle to the reader thread which closes it on exit.
        let output_rd_isize: isize = pty_out_rd.0 as isize;

        // ── Background reader thread ──────────────────────────────────────────
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            // Re-construct the raw pointer inside the thread from the isize value.
            let output_rd_raw = output_rd_isize as *mut std::ffi::c_void;
            let mut f = std::fs::File::from_raw_handle(output_rd_raw);
            let mut buf = [0u8; 4096];
            loop {
                match f.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Session {
            input_write,
            output_rx: rx,
            pending: Vec::new(),
            current_task: 0,
            hpcon: OwnedHPCON(hpcon.0),
            proc_handle: OwnedHandle(pi.hProcess.0 as isize),
            thread_handle: OwnedHandle(pi.hThread.0 as isize),
        })
    }

    impl Backend for ConPtyBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::ConPty
        }

        fn active_session_ids(&self) -> Vec<SessionId> {
            self.sessions.keys().cloned().collect()
        }

        fn spawn_session(&mut self) -> Result<SessionId, BackendError> {
            let session = unsafe { make_session(&self.shell, self.cols, self.rows)? };
            let id = self.next_session;
            self.next_session += 1;
            self.sessions.insert(id, session);
            Ok(id)
        }

        fn exec(&mut self, session: SessionId, req: ExecRequest) -> Result<TaskId, BackendError> {
            let sess = self
                .sessions
                .get_mut(&session)
                .ok_or_else(|| BackendError::Other("session not found".into()))?;

            let task_id = self.next_task;
            self.next_task += 1;
            sess.current_task = task_id;

            let mut line = req.command_line;
            line.push_str("\r\n");
            sess.input_write
                .write_all(line.as_bytes())
                .map_err(BackendError::Io)?;
            sess.input_write.flush().map_err(BackendError::Io)?;

            Ok(task_id)
        }

        fn interrupt(&mut self, session: SessionId, _task: TaskId) -> Result<(), BackendError> {
            if let Some(sess) = self.sessions.get_mut(&session) {
                // Ctrl+C = 0x03
                let _ = sess.input_write.write_all(&[0x03]);
                let _ = sess.input_write.flush();
            }
            Ok(())
        }

        fn poll_events(&mut self, session: SessionId) -> Result<Vec<SessionEvent>, BackendError> {
            let mut events = Vec::new();
            let sess = match self.sessions.get_mut(&session) {
                Some(s) => s,
                None => return Ok(events),
            };

            // Drain all bytes the reader thread has queued.
            while let Ok(chunk) = sess.output_rx.try_recv() {
                sess.pending.extend_from_slice(&chunk);
            }

            if sess.pending.is_empty() {
                return Ok(events);
            }

            // Convert to UTF-8 (lossy — ConPTY outputs UTF-8 on modern Windows).
            let raw = String::from_utf8_lossy(&sess.pending).into_owned();
            sess.pending.clear();

            let task_id = sess.current_task;

            // Split on LF; strip trailing CR from each piece.
            let parts = raw.split('\n');
            for part in parts {
                let line = part.trim_end_matches('\r').to_string();
                // Emit even if empty so blank lines and prompts are preserved.
                events.push(SessionEvent::Output(OutputChunk {
                    task_id,
                    is_stderr: false,
                    text: line,
                }));
            }

            Ok(events)
        }

        fn reset(&mut self) {
            for (_id, sess) in self.sessions.drain() {
                unsafe {
                    ClosePseudoConsole(HPCON(sess.hpcon.0));
                    let _ = CloseHandle(HANDLE(sess.proc_handle.0 as *mut std::ffi::c_void));
                    let _ = CloseHandle(HANDLE(sess.thread_handle.0 as *mut std::ffi::c_void));
                }
            }
            self.next_session = 1;
            self.next_task = 1;
        }
    }
}

// ── Stub for non-Windows builds ───────────────────────────────────────────────

#[cfg(not(windows))]
impl Backend for ConPtyBackend {
    fn kind(&self) -> BackendKind { BackendKind::ConPty }
    fn active_session_ids(&self) -> Vec<SessionId> { vec![] }
    fn spawn_session(&mut self) -> Result<SessionId, BackendError> {
        Err(BackendError::Other("ConPTY not supported on this platform".into()))
    }
    fn exec(&mut self, _s: SessionId, _r: ExecRequest) -> Result<TaskId, BackendError> {
        Err(BackendError::Other("not supported".into()))
    }
    fn interrupt(&mut self, _s: SessionId, _t: TaskId) -> Result<(), BackendError> { Ok(()) }
    fn poll_events(&mut self, _s: SessionId) -> Result<Vec<SessionEvent>, BackendError> { Ok(vec![]) }
    fn reset(&mut self) {}
}
