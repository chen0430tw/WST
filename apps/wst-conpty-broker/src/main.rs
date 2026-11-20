//! wst-conpty-broker: spawned with DETACHED_PROCESS so it has no console.
//! Creates a ConPTY session and bridges stdin→shell, shell→stdout.
//!
//! Usage: wst-conpty-broker.exe <shell> <cols> <rows>
//!   stdin  → PTY input  (commands sent to shell)
//!   stdout ← PTY output (shell output back to wst-ui)

// NOTE: intentionally no #![windows_subsystem = "windows"] — we use DETACHED_PROCESS
// at spawn time so no console window appears. Keeping console subsystem lets
// GetStdHandle work normally for the piped stdin/stdout.

use std::io::{Read, Write};
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
    EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
    STARTUPINFOW, STARTUPINFOEXW,
};
use windows::core::PWSTR;

const ATTR_PSEUDOCONSOLE: usize = 0x0002_0016;

macro_rules! log {
    ($log:expr, $($arg:tt)*) => {
        if let Some(ref mut f) = $log {
            let _ = writeln!(f, $($arg)*);
        }
    };
}

fn open_log() -> Option<std::fs::File> {
    let p = std::env::temp_dir().join("wst_broker_debug.log");
    std::fs::OpenOptions::new().create(true).append(true).open(p).ok()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let shell = args.get(1).map(|s| s.as_str()).unwrap_or("powershell.exe");
    let cols: i16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(220);
    let rows: i16 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(50);

    let mut log = open_log();
    log!(log, "[broker] started: shell={:?} cols={} rows={}", shell, cols, rows);

    match run(shell, cols, rows, log) {
        Ok(()) => {}
        Err(e) => {
            let mut log2 = open_log();
            log!(log2, "[broker] FATAL: {}", e);
            std::process::exit(1);
        }
    }
}

fn run(shell: &str, cols: i16, rows: i16, mut log: Option<std::fs::File>) -> Result<(), String> {
    unsafe {
        // ── Pipe pair: stdin of broker → PTY input ───────────────────────────
        let mut pty_in_rd = HANDLE::default();
        let mut pty_in_wr = HANDLE::default();
        CreatePipe(&mut pty_in_rd, &mut pty_in_wr, None, 0)
            .map_err(|e| format!("CreatePipe(in): {}", e))?;
        log!(log, "[broker] pipe(in) ok");

        // ── Pipe pair: PTY output → stdout of broker ─────────────────────────
        let mut pty_out_rd = HANDLE::default();
        let mut pty_out_wr = HANDLE::default();
        CreatePipe(&mut pty_out_rd, &mut pty_out_wr, None, 0)
            .map_err(|e| format!("CreatePipe(out): {}", e))?;
        log!(log, "[broker] pipe(out) ok");

        // ── Create the pseudo console ─────────────────────────────────────────
        let size = COORD { X: cols, Y: rows };
        let hpcon: HPCON = CreatePseudoConsole(size, pty_in_rd, pty_out_wr, 0)
            .map_err(|e| format!("CreatePseudoConsole: {}", e))?;
        log!(log, "[broker] CreatePseudoConsole ok hpcon={}", hpcon.0);

        let _ = CloseHandle(pty_in_rd);
        let _ = CloseHandle(pty_out_wr);

        // ── Build STARTUPINFOEXW ──────────────────────────────────────────────
        let mut attr_size = 0usize;
        let _ = InitializeProcThreadAttributeList(
            windows::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST(
                std::ptr::null_mut(),
            ),
            1, 0, &mut attr_size,
        );
        let mut attr_buf = vec![0u8; attr_size];
        let attr_list = windows::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST(
            attr_buf.as_mut_ptr() as *mut _,
        );
        InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size)
            .map_err(|e| format!("InitProcThreadAttrList: {}", e))?;

        // Pass HPCON directly as lpValue (it IS a void*, not a pointer to one).
        // Passing &hpcon would be a double-pointer and silently corrupt the attribute.
        UpdateProcThreadAttribute(
            attr_list, 0, ATTR_PSEUDOCONSOLE,
            Some(hpcon.0 as *const std::ffi::c_void),
            size_of::<isize>(),
            None, None,
        ).map_err(|e| format!("UpdateProcThreadAttr: {}", e))?;
        let hpcon_val: isize = hpcon.0 as isize;

        // STARTF_USESTDHANDLES with null handles: without it the shell inherits
        // the broker's own redirected stdin/stdout pipes and bypasses the
        // ConPTY — it then steals our stdin and writes raw ANSI-codepage bytes
        // straight to wst-ui.
        let si_ex = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: size_of::<STARTUPINFOEXW>() as u32,
                dwFlags: STARTF_USESTDHANDLES,
                ..Default::default()
            },
            lpAttributeList: attr_list,
        };

        // ── Spawn shell ───────────────────────────────────────────────────────
        let mut cmd_line: Vec<u16> = shell.encode_utf16().chain(std::iter::once(0)).collect();
        let start_dir = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string());
        let mut start_dir_w: Vec<u16> = start_dir.encode_utf16().chain(std::iter::once(0)).collect();
        let mut pi = PROCESS_INFORMATION::default();

        CreateProcessW(
            None,
            PWSTR(cmd_line.as_mut_ptr()),
            None, None, false,
            EXTENDED_STARTUPINFO_PRESENT,
            None,
            windows::core::PCWSTR(start_dir_w.as_ptr()),
            &si_ex.StartupInfo as *const STARTUPINFOW,
            &mut pi,
        ).map_err(|e| format!("CreateProcessW: {}", e))?;
        log!(log, "[broker] CreateProcessW ok pid={}", pi.dwProcessId);

        DeleteProcThreadAttributeList(attr_list);
        let _ = CloseHandle(pi.hThread);

        // ── Bridge threads ────────────────────────────────────────────────────
        // Thread 1: our stdin → PTY input pipe.
        // On stdin EOF (wst-ui closed the pipe or exited) terminate the shell,
        // so the broker's wait below returns and no broker/shell is orphaned.
        let pty_in_wr_isize = pty_in_wr.0 as isize;
        let shell_proc_isize = pi.hProcess.0 as isize;
        std::thread::spawn(move || {
            let mut pty_in = std::fs::File::from_raw_handle(
                pty_in_wr_isize as *mut std::ffi::c_void,
            );
            let stdin = std::io::stdin();
            let mut buf = [0u8; 512];
            loop {
                match stdin.lock().read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if pty_in.write_all(&buf[..n]).is_err() { break; }
                        let _ = pty_in.flush();
                    }
                }
            }
            let _ = windows::Win32::System::Threading::TerminateProcess(
                HANDLE(shell_proc_isize as *mut std::ffi::c_void),
                0,
            );
        });

        // Thread 2: PTY output pipe → our stdout
        let pty_out_rd_isize = pty_out_rd.0 as isize;
        let output_thread = std::thread::spawn(move || {
            let mut pty_out = std::fs::File::from_raw_handle(
                pty_out_rd_isize as *mut std::ffi::c_void,
            );
            let stdout = std::io::stdout();
            let mut buf = [0u8; 4096];
            loop {
                match pty_out.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut out = stdout.lock();
                        if out.write_all(&buf[..n]).is_err() { break; }
                        let _ = out.flush();
                    }
                }
            }
        });

        // Wait for shell to exit
        log!(log, "[broker] waiting for shell...");
        windows::Win32::System::Threading::WaitForSingleObject(
            HANDLE(pi.hProcess.0 as *mut std::ffi::c_void),
            windows::Win32::System::Threading::INFINITE,
        );
        let mut exit_code = 0u32;
        let _ = windows::Win32::System::Threading::GetExitCodeProcess(
            HANDLE(pi.hProcess.0 as *mut std::ffi::c_void),
            &mut exit_code,
        );
        log!(log, "[broker] shell exited exit_code=0x{:x}", exit_code);
        let _ = CloseHandle(HANDLE(pi.hProcess.0 as *mut std::ffi::c_void));
        ClosePseudoConsole(HPCON(hpcon_val));

        let _ = output_thread.join();
        log!(log, "[broker] done");
    }
    Ok(())
}
