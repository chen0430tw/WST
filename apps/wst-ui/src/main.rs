mod builtin;

use ansi_to_tui::IntoText;
use anyhow::Result;
use unicode_width::UnicodeWidthChar;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};

// Send F11 for Windows Terminal fullscreen (must be called BEFORE alternate screen)
#[cfg(windows)]
fn set_windows_terminal_fullscreen(_fullscreen: bool) {
    use windows::Win32::System::Console::GetConsoleWindow;
    use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
    use winput::Vk;
    use std::thread;
    use std::time::Duration;
    use std::fs::OpenOptions;
    use std::io::Write;

    unsafe {
        let hwnd = GetConsoleWindow();

        // Bring window to foreground first
        if !hwnd.is_invalid() {
            let _ = SetForegroundWindow(hwnd);
            thread::sleep(Duration::from_millis(300));
        }

        // Send F11 to toggle Windows Terminal fullscreen
        let _ = winput::send(Vk::F11);
        thread::sleep(Duration::from_millis(500));

        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open("C:\\Users\\Administrator\\WST\\wst_debug.log")
            .and_then(|mut f| writeln!(f, "set_windows_terminal_fullscreen: F11 sent"));
    }
}

// Send Alt+Enter for legacy console fullscreen (must be called AFTER alternate screen)
#[cfg(windows)]
fn set_legacy_console_fullscreen() {
    use winput::{Vk, Action, Input};
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    use std::thread;
    use std::time::Duration;
    use std::fs::OpenOptions;
    use std::io::Write;

    // Get parent process name to detect if running in Windows Terminal
    let parent_name = unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        let mut parent_name = String::from("Unknown");

        if let Ok(snapshot) = snapshot {
            let mut entry = PROCESSENTRY32::default();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;
            let current_pid = std::process::id();

            if Process32First(snapshot, &mut entry).is_ok() {
                while Process32Next(snapshot, &mut entry).is_ok() {
                    if entry.th32ProcessID == current_pid {
                        let parent_id = entry.th32ParentProcessID;
                        // Restart search to find parent
                        let mut entry2 = PROCESSENTRY32::default();
                        entry2.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;

                        if Process32First(snapshot, &mut entry2).is_ok() {
                            while Process32Next(snapshot, &mut entry2).is_ok() {
                                if entry2.th32ProcessID == parent_id {
                                    let name_bytes: Vec<u8> = entry2.szExeFile
                                        .iter()
                                        .take_while(|&&x| x != 0)
                                        .map(|&x| x as u8)
                                        .collect();
                                    parent_name = String::from_utf8_lossy(&name_bytes).to_string();
                                    break;
                                }
                            }
                        }
                        break;
                    }
                }
            }
        }
        parent_name
    };

    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open("C:\\Users\\Administrator\\WST\\wst_debug.log")
        .and_then(|mut f| writeln!(f, "Parent process: {}", parent_name));

    thread::sleep(Duration::from_millis(500));

    // Use F11 for Windows Terminal, Alt+Enter for native cmd
    if parent_name.contains("WindowsTerminal.exe") || parent_name.contains("wt.exe") {
        // Windows Terminal: F11 already sent before EnterAlternateScreen, do nothing here
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open("C:\\Users\\Administrator\\WST\\wst_debug.log")
            .and_then(|mut f| writeln!(f, "Windows Terminal detected - F11 already sent, skipping"));
        return;
    } else {
        // Native cmd: use Alt+Enter
        let inputs = [
            Input::from_vk(Vk::LeftMenu, Action::Press),   // Alt down
            Input::from_vk(Vk::Enter, Action::Press),      // Enter down
            Input::from_vk(Vk::Enter, Action::Release),    // Enter up
            Input::from_vk(Vk::LeftMenu, Action::Release), // Alt up
        ];

        let result = winput::send_inputs(&inputs);

        // Debug log
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open("C:\\Users\\Administrator\\WST\\wst_debug.log")
            .and_then(|mut f| {
                writeln!(f, "Alt+Enter sent, result: {:?}", result)?;
                writeln!(f, "Number of inputs: {}", inputs.len())
            });
    }
}
use ratatui::{
    backend::CrosstermBackend,
    style::{Color, Style},
    text::Span,
    widgets::Paragraph,
    Frame, Terminal,
};
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use wst_config::WstConfig;
use wst_core::WstCore;
use wst_protocol::{BackendKind, SessionEvent, TaskStatus};

#[allow(dead_code)]
const INPUT_PROMPT: &str = ">";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Console login banner in the style of a Linux getty/login on tty1:
///
/// ```text
/// WST 0.1.0 host tty1
///
/// host login: user
/// Welcome to WST 0.1.0 (Windows NT 10.0.19045 x86_64)
///
///  * Commands:  type :help
///
/// Last login: Fri Nov 20 03:40:12 2026 on tty1
/// ```
fn login_banner(hostname: &str, username: &str) -> VecDeque<OutputLine> {
    let mut lines = VecDeque::from([
        OutputLine::normal(format!("WST {} {} tty1", VERSION, hostname)),
        OutputLine::normal(""),
        OutputLine::normal(format!("{} login: {}", hostname, username)),
        OutputLine::normal(format!(
            "Welcome to WST {} ({} {})",
            VERSION,
            kernel_version(),
            std::env::consts::ARCH
        )),
        OutputLine::normal(""),
        OutputLine::normal(" * Commands:  type :help"),
        OutputLine::normal(""),
    ]);
    if let Some(last) = swap_last_login() {
        lines.push_back(OutputLine::normal(format!("Last login: {} on tty1", last)));
    }
    lines
}

/// "Windows NT 10.0.19045" — the counterpart of "GNU/Linux 5.15.0-91-generic".
/// RtlGetVersion is used because GetVersionEx lies without an app manifest.
fn kernel_version() -> String {
    #[cfg(windows)]
    {
        #[repr(C)]
        struct OsVersionInfoW {
            size: u32,
            major: u32,
            minor: u32,
            build: u32,
            platform: u32,
            csd: [u16; 128],
        }
        #[link(name = "ntdll")]
        extern "system" {
            fn RtlGetVersion(info: *mut OsVersionInfoW) -> i32;
        }
        let mut info = OsVersionInfoW {
            size: std::mem::size_of::<OsVersionInfoW>() as u32,
            major: 0,
            minor: 0,
            build: 0,
            platform: 0,
            csd: [0; 128],
        };
        if unsafe { RtlGetVersion(&mut info) } == 0 {
            return format!("Windows NT {}.{}.{}", info.major, info.minor, info.build);
        }
    }
    "Windows NT".to_string()
}

/// Returns the previous login time (if any) and records the current one,
/// like pam_lastlog. Stored in ~/.wst/lastlog.
fn swap_last_login() -> Option<String> {
    let home = std::env::var("USERPROFILE").ok()?;
    let path = std::path::Path::new(&home).join(".wst").join("lastlog");
    let previous = std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let now = chrono::Local::now().format("%a %b %e %H:%M:%S %Y").to_string();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, now);
    previous
}
const MAX_SCROLLBACK: usize = 10_000;
#[allow(dead_code)]
const CP_UTF8: u32 = 65001;

struct OutputLine {
    text: String,
    is_error: bool,
    is_system: bool,
    /// Cached parsed ANSI lines — populated lazily on first draw, reused every frame.
    cached_lines: Option<Vec<ratatui::text::Line<'static>>>,
}

impl OutputLine {
    fn normal(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: false, is_system: false, cached_lines: None }
    }
    fn error(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true, is_system: false, cached_lines: None }
    }
    fn system(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: false, is_system: true, cached_lines: None }
    }
}

struct AppState {
    core: Arc<tokio::sync::Mutex<WstCore>>,
    input: String,
    cursor_position: usize,
    output: VecDeque<OutputLine>,
    running: bool,
    session_id: Option<u64>,
    scroll_offset: usize,
    current_task_id: Option<u64>,
    command_in_progress: bool,
    current_dir: String,
    home_dir: String,
    hostname: String,
    username: String,
    prompt_char: String,
    debug_mode: bool,
    needs_redraw: bool,
    insert_mode: bool,
    last_key_was_tab: bool,
    // Debug stats
    lines_received: usize,
    last_command: String,
    backend_encoding: String,
    // Root mode: commands are wrapped with `sudo su root` so they run as
    // SYSTEM (rewritten to "root" by cygctl su). The prompt shows `#`.
    root_mode: bool,
    pre_root_user: String,
    pre_root_prompt: String,
}

impl AppState {
    fn new(config: WstConfig) -> Self {
        let core = Arc::new(tokio::sync::Mutex::new(WstCore::new(config)));
        let current_dir = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("C:\\Users"))
            .to_string_lossy()
            .to_string();
        // Strip Windows NT namespace prefix that appears in some launch contexts
        let current_dir = current_dir
            .strip_prefix("\\\\?\\")
            .unwrap_or(&current_dir)
            .to_string();

        let hostname = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "windows".to_string()).to_lowercase();
        let username = std::env::var("USERNAME").unwrap_or_else(|_| "user".to_string()).to_lowercase();
        let home_dir = std::env::var("USERPROFILE").unwrap_or_else(|_| current_dir.clone());
        let current_dir = home_dir.clone();
        // Set the actual process CWD so backend processes inherit the correct directory.
        let _ = std::env::set_current_dir(&current_dir);

        // Check if running as admin (for # vs $ prompt)
        #[cfg(windows)]
        let is_admin = {
            use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
            use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
            use windows::Win32::Foundation::HANDLE;
            unsafe {
                let mut token = HANDLE::default();
                let mut elevated = false;
                if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_ok() {
                    let mut elevation = TOKEN_ELEVATION::default();
                    let mut size = std::mem::size_of::<TOKEN_ELEVATION>() as u32;
                    if GetTokenInformation(token, TokenElevation,
                        Some(&mut elevation as *mut _ as *mut _), size, &mut size).is_ok() {
                        elevated = elevation.TokenIsElevated != 0;
                    }
                    let _ = windows::Win32::Foundation::CloseHandle(token);
                }
                elevated
            }
        };
        #[cfg(not(windows))]
        let is_admin = false;

        let prompt_char = if is_admin { "#" } else { "$" };

        Self {
            core,
            input: String::new(),
            cursor_position: 0,
            output: login_banner(&hostname, &username),
            running: true,
            session_id: None,
            scroll_offset: 0,
            current_task_id: None,
            command_in_progress: false,
            current_dir,
            home_dir,
            hostname,
            username,
            prompt_char: prompt_char.to_string(),
            debug_mode: false,
            needs_redraw: true,
            insert_mode: true,
            last_key_was_tab: false,
            lines_received: 0,
            last_command: String::new(),
            backend_encoding: "UTF-8".to_string(),
            root_mode: false,
            pre_root_user: String::new(),
            pre_root_prompt: String::new(),
        }
    }

    /// Classifies a typed line as a request to become root (`su`, `sudo -i`, …)
    /// so WST can enter root mode instead of forwarding it to the backend.
    fn is_become_root(cmd: &str) -> bool {
        matches!(
            cmd.split_whitespace().collect::<Vec<_>>().as_slice(),
            ["su"]
                | ["su", "root"]
                | ["su", "-"]
                | ["su", "-", "root"]
                | ["su", "-l"]
                | ["su", "-l", "root"]
                | ["sudo", "-i"]
                | ["sudo", "-s"]
                | ["sudo", "su"]
                | ["sudo", "su", "root"]
                | ["sudo", "-i", "root"]
        )
    }

    /// Enter root mode: remember the current identity, switch the prompt to `#`
    /// and the displayed user to `root`. Commands are elevated lazily, so the
    /// first real command (not this one) is what may trigger the single UAC
    /// prompt for the cygsec elevation daemon.
    fn enter_root_mode(&mut self) {
        if self.root_mode {
            return;
        }
        self.pre_root_user = self.username.clone();
        self.pre_root_prompt = self.prompt_char.clone();
        self.root_mode = true;
        self.username = "root".to_string();
        self.prompt_char = "#".to_string();
    }

    /// Leave root mode (triggered by `exit`/`logout`), restoring the prompt.
    fn leave_root_mode(&mut self) {
        if !self.root_mode {
            return;
        }
        self.root_mode = false;
        self.username = std::mem::take(&mut self.pre_root_user);
        self.prompt_char = std::mem::take(&mut self.pre_root_prompt);
    }

    fn handle_input(&mut self, key: KeyEvent) {
        self.needs_redraw = true;
        let prev_tab = self.last_key_was_tab;
        self.last_key_was_tab = false;

        match key.code {
            KeyCode::Tab => {
                self.handle_tab(prev_tab);
                self.last_key_was_tab = true;
                return;
            }
            KeyCode::Insert => {
                self.insert_mode = !self.insert_mode;
                return;
            }
            KeyCode::Enter => self.execute_command(),
            // Ctrl+P/N for command history (must come before generic Char)
            KeyCode::Char('p') | KeyCode::Char('P') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Ok(mut core) = self.core.try_lock() {
                    if let Some(cmd) = core.history_prev() {
                        self.input = cmd;
                        self.cursor_position = self.input.len();
                    }
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Ok(mut core) = self.core.try_lock() {
                    if let Some(cmd) = core.history_next() {
                        self.input = cmd;
                        self.cursor_position = self.input.len();
                    } else {
                        self.input.clear();
                        self.cursor_position = 0;
                    }
                }
            }
            KeyCode::Char(c) => {
                if !c.is_control() {
                    if self.insert_mode || self.cursor_position >= self.input.len() {
                        self.input.insert(self.cursor_position, c);
                    } else {
                        // Overwrite mode: replace char at cursor
                        self.input.remove(self.cursor_position);
                        self.input.insert(self.cursor_position, c);
                    }
                    self.move_cursor_right();
                }
            }
            KeyCode::Backspace => {
                if self.cursor_position > 0 {
                    // Find previous char boundary and remove that char
                    let prev = self.input[..self.cursor_position]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.remove(prev);
                    self.cursor_position = prev;
                }
            }
            KeyCode::Delete => {
                if self.cursor_position < self.input.len() {
                    self.input.remove(self.cursor_position);
                }
            }
            KeyCode::Left => self.move_cursor_left(),
            KeyCode::Right => {
                if self.cursor_position < self.input.len() {
                    self.move_cursor_right();
                } else {
                    // At end of line: peek-history fill (ReactOS PeekHistory pattern)
                    let peek = if let Ok(core) = self.core.try_lock() {
                        core.history_peek_prev()
                    } else {
                        None
                    };
                    if let Some(hist) = peek {
                        // cursor_position is a byte offset; get next char at that offset
                        if let Some(ch) = hist.get(self.cursor_position..).and_then(|s| s.chars().next()) {
                            self.input.push(ch);
                            self.cursor_position += ch.len_utf8();
                        }
                    }
                }
            }
            KeyCode::Home => self.cursor_position = 0,
            KeyCode::End => self.cursor_position = self.input.len(),
            KeyCode::Up => {
                if self.scroll_offset == 0 {
                    // At bottom → cycle history backwards
                    if let Ok(mut core) = self.core.try_lock() {
                        if let Some(cmd) = core.history_prev() {
                            self.input = cmd;
                            self.cursor_position = self.input.len();
                        }
                    }
                } else {
                    self.scroll_output_up(3);
                }
            }
            KeyCode::Down => {
                if self.scroll_offset == 0 {
                    // At bottom → cycle history forwards
                    if let Ok(mut core) = self.core.try_lock() {
                        match core.history_next() {
                            Some(cmd) => {
                                self.input = cmd;
                                self.cursor_position = self.input.len();
                            }
                            None => {
                                self.input.clear();
                                self.cursor_position = 0;
                                core.history_reset();
                            }
                        }
                    }
                } else {
                    self.scroll_output_down(3);
                }
            }
            KeyCode::Esc => {
                self.input.clear();
                self.cursor_position = 0;
                if let Ok(mut core) = self.core.try_lock() {
                    core.history_reset();
                }
            }
            KeyCode::PageUp => self.scroll_output_up(10),
            KeyCode::PageDown => self.scroll_output_down(10),
            _ => {}
        }
    }

    fn scroll_output_up(&mut self, amount: usize) {
        // Scroll up means showing older content (increase offset from bottom)
        let max_offset = self.output.len().saturating_sub(1);
        self.scroll_offset = self.scroll_offset.saturating_add(amount);
        if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
    }

    fn scroll_output_down(&mut self, amount: usize) {
        // Scroll down means showing newer content (decrease offset toward bottom)
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    fn move_cursor_left(&mut self) {
        if self.cursor_position > 0 {
            // Retreat to previous char boundary (handles multi-byte UTF-8)
            self.cursor_position = self.input[..self.cursor_position]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    fn move_cursor_right(&mut self) {
        if self.cursor_position < self.input.len() {
            // Advance by byte length of next char (handles multi-byte UTF-8)
            let ch = self.input[self.cursor_position..].chars().next().unwrap();
            self.cursor_position += ch.len_utf8();
        }
    }

    fn execute_command(&mut self) {
        let command = self.input.trim().to_string();
        if command.is_empty() {
            self.input.clear();
            self.cursor_position = 0;
            return;
        }

        // Alias cls/clear to :clear
        if command.eq_ignore_ascii_case("cls") || command.eq_ignore_ascii_case("clear") {
            self.output.clear();
            self.scroll_offset = 0;
            self.input.clear();
            self.cursor_position = 0;
            return;
        }

        // Echo command with ANSI colors so history scrollback looks like the live prompt
        let echoed = format!(
            "\x1b[32m{}@{}\x1b[0m:\x1b[36m{}\x1b[0m{} {}",
            self.username, self.hostname, self.display_dir(), self.prompt_char, command
        );
        self.output.push_back(OutputLine::normal(echoed));
        self.scroll_to_bottom();

        // Track command and reset stats
        self.last_command = command.clone();
        self.lines_received = 0;

        // Root-mode transitions are handled by WST itself, not the backend.
        if Self::is_become_root(&command) {
            self.enter_root_mode();
            self.input.clear();
            self.cursor_position = 0;
            return;
        }
        if self.root_mode
            && (command.eq_ignore_ascii_case("exit") || command.eq_ignore_ascii_case("logout"))
        {
            self.leave_root_mode();
            self.input.clear();
            self.cursor_position = 0;
            return;
        }

        if command.starts_with(':') {
            self.handle_builtin(&command);
        } else {
            // Use blocking_lock (not try_lock): poll_events is now non-blocking,
            // so the tick task holds `core` only briefly. try_lock would silently
            // drop the command if it happened to collide with that tick.
            // Safe here because run_app runs on the main (non-async) thread.
            let session_err: Option<String> = {
                let mut core = self.core.blocking_lock();
                match core.ensure_session() {
                    Ok(sid) => { self.session_id = Some(sid); None }
                    Err(e) => Some(e.to_string())
                }
            };
            if let Some(err) = session_err {
                self.add_output(OutputLine::error(format!("Session error: {}", err)));
                return;
            }
            // In root mode, run every command as SYSTEM via `sudo su root`.
            // History and the echoed line keep the command the user typed.
            let actual = if self.root_mode {
                format!("sudo su root {}", command)
            } else {
                command.clone()
            };
            {
                let mut core = self.core.blocking_lock();
                core.push_history(command.clone());
                match core.exec_raw(actual) {
                    Ok(_tid) => {
                        self.current_task_id = Some(_tid);
                    }
                    Err(e) => {
                        self.output.push_back(OutputLine::error(format!("Error: {}", e)));
                    }
                }
            }
        }

        self.input.clear();
        self.cursor_position = 0;
    }

    fn handle_builtin(&mut self, command: &str) {
        let parts: Vec<&str> = command.split_whitespace().collect();
        let cmd = parts.first().map(|s| *s).unwrap_or(":");

        match cmd {
            ":help" => {
                self.output.push_back(OutputLine::system("Builtin commands:"));
                self.output.push_back(OutputLine::normal("  :help        - Show this help"));
                self.output.push_back(OutputLine::normal("  :status      - Show current status"));
                self.output.push_back(OutputLine::normal("  :clear       - Clear output"));
                self.output.push_back(OutputLine::normal("  :history     - Show command history"));
                self.output.push_back(OutputLine::normal("  :backend     - Switch backend (Cmd|ConPty|Pwsh|Cygctl)"));
                self.output.push_back(OutputLine::normal("  :debug       - Toggle debug mode"));
                self.output.push_back(OutputLine::normal("  :exit / :q   - Exit WST"));
                self.output.push_back(OutputLine::normal(""));
                self.output.push_back(OutputLine::system("Keyboard shortcuts:"));
                self.output.push_back(OutputLine::normal("  PageUp/Down  - Scroll output"));
                self.output.push_back(OutputLine::normal("  Mouse wheel  - Scroll output"));
                self.output.push_back(OutputLine::normal("  Shift+Drag   - Select text (Windows Terminal)"));
            }
            ":status" => {
                if let Ok(core) = self.core.try_lock() {
                    self.output.push_back(OutputLine::system(format!(
                        "WST v{} - Windows Subsystem for TTY",
                        VERSION
                    )));
                    self.output.push_back(OutputLine::normal(format!("Backend: {:?}", core.default_backend())));
                    self.output.push_back(OutputLine::normal(format!("Session: {:?}", self.session_id)));
                    self.output.push_back(OutputLine::normal(format!(
                        "History: {} commands",
                        core.history().len()
                    )));
                }
            }
            ":clear" => {
                self.output.clear();
                self.scroll_offset = 0;
            }
            ":history" => {
                if let Ok(core) = self.core.try_lock() {
                    for (i, entry) in core.history().iter().enumerate() {
                        self.output.push_back(OutputLine::normal(format!("  {}: {}", i + 1, entry.command)));
                    }
                }
            }
            ":backend" => {
                if parts.len() < 2 {
                    if let Ok(core) = self.core.try_lock() {
                        self.output.push_back(OutputLine::normal(format!(
                            "Current backend: {:?}",
                            core.default_backend()
                        )));
                        self.output.push_back(OutputLine::normal("Usage: :backend <Cmd|ConPty|Pwsh|Cygctl>"));
                    }
                } else {
                    let new_backend = match parts[1].to_lowercase().as_str() {
                        "cygctl" => Some(BackendKind::Cygctl),
                        "pwsh" => Some(BackendKind::Pwsh),
                        "cmd" => Some(BackendKind::Cmd),
                        "conpty" | "conpry" | "contty" | "pty" => Some(BackendKind::ConPty),
                        _ => None,
                    };

                    if let Some(kind) = new_backend {
                        if let Ok(mut core) = self.core.try_lock() {
                            match core.switch_backend(kind) {
                                Ok(()) => {
                                    self.output.push_back(OutputLine::system(format!(
                                        "Switched to {:?} backend",
                                        kind
                                    )));
                                    self.session_id = None;
                                    self.current_task_id = None;
                                    self.command_in_progress = false;
                                    // Update encoding info for debug
                                    match kind {
                                        BackendKind::Pwsh => self.backend_encoding = "Big5/UTF-16".to_string(),
                                        BackendKind::Cmd => self.backend_encoding = "CP936/GBK".to_string(),
                                        BackendKind::Cygctl => self.backend_encoding = "UTF-8".to_string(),
                                        BackendKind::ConPty => self.backend_encoding = "UTF-8/ANSI".to_string(),
                                    }
                                }
                                Err(e) => {
                                    self.output.push_back(OutputLine::error(format!("Failed to switch: {}", e)));
                                }
                            }
                        }
                    } else {
                        self.output.push_back(OutputLine::error("Unknown backend. Use: Cmd, ConPty, Pwsh, or Cygctl"));
                    }
                }
            }
            ":debug" => {
                self.debug_mode = !self.debug_mode;
                self.output.push_back(OutputLine::system(format!(
                    "Debug mode: {}",
                    if self.debug_mode { "ON" } else { "OFF" }
                )));
            }
            ":exit" | ":q" => self.running = false,
            _ => {
                self.output.push_back(OutputLine::error(format!("Unknown builtin: {}", cmd)));
                self.output.push_back(OutputLine::normal("Type :help for available commands"));
            }
        }
    }

    fn handle_tab(&mut self, double_tab: bool) {
        let input = self.input.clone();

        // split: everything before the last token, and the token being completed
        let (prefix_before, word) = match input.rfind(' ') {
            Some(pos) => (&input[..=pos], &input[pos + 1..]),
            None => ("", input.as_str()),
        };

        // split word into dir prefix and filename fragment
        let sep_pos = word.rfind(|c| c == '/' || c == '\\' || c == ':');
        let (dir_str, frag) = match sep_pos {
            Some(pos) => (&word[..=pos], &word[pos + 1..]),
            None => ("", word),
        };

        // resolve search directory
        let search_dir = if dir_str.is_empty() {
            self.current_dir.clone()
        } else {
            std::path::Path::new(&self.current_dir)
                .join(dir_str)
                .to_string_lossy()
                .to_string()
        };

        let frag_lower = frag.to_ascii_lowercase();
        let matches: Vec<String> = std::fs::read_dir(&search_dir)
            .map(|rd| {
                let mut v: Vec<String> = rd
                    .filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if name.to_ascii_lowercase().starts_with(&frag_lower) {
                            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                            Some(if is_dir { format!("{}\\", name) } else { name })
                        } else {
                            None
                        }
                    })
                    .collect();
                v.sort();
                v
            })
            .unwrap_or_default();

        if matches.is_empty() {
            return;
        }

        if double_tab && matches.len() > 1 {
            // Double Tab: echo current input then list all matches in columns
            let echoed = format!(
                "\x1b[32m{}@{}\x1b[0m:\x1b[36m{}\x1b[0m{} {}",
                self.username, self.hostname, self.display_dir(), self.prompt_char, input
            );
            self.output.push_back(OutputLine::normal(echoed));
            // Two-scan: find max width, then emit one line
            let max_w = matches.iter().map(|s| s.len()).max().unwrap_or(1) + 2;
            let cols = (80usize / max_w).max(1);
            let mut line = String::new();
            for (i, m) in matches.iter().enumerate() {
                line.push_str(m);
                if (i + 1) % cols == 0 {
                    self.output.push_back(OutputLine::normal(line.clone()));
                    line.clear();
                } else {
                    // pad to column width
                    for _ in m.len()..max_w {
                        line.push(' ');
                    }
                }
            }
            if !line.is_empty() {
                self.output.push_back(OutputLine::normal(line));
            }
            while self.output.len() > MAX_SCROLLBACK {
                self.output.pop_front();
            }
        } else {
            // Single Tab: LCP completion
            let lcp = matches.iter().skip(1).fold(matches[0].clone(), |acc, m| {
                acc.chars()
                    .zip(m.chars())
                    .take_while(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
                    .map(|(a, _)| a)
                    .collect()
            });
            let new_word = format!("{}{}", dir_str, lcp);
            self.input = format!("{}{}", prefix_before, new_word);
            self.cursor_position = self.input.len();
            // beep if multiple matches (no unique completion)
            if matches.len() > 1 {
                print!("\x07");
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
        }
        self.needs_redraw = true;
    }

    fn display_dir(&self) -> &str {
        if self.current_dir.eq_ignore_ascii_case(&self.home_dir) {
            "~"
        } else {
            &self.current_dir
        }
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    fn add_output(&mut self, line: OutputLine) {
        self.output.push_back(line);
        if self.output.len() > MAX_SCROLLBACK {
            self.output.pop_front();
        }
        self.scroll_to_bottom();
        self.needs_redraw = true;
    }

    /// tty-style Ctrl+C: echo the current prompt + aborted input with a trailing
    /// `^C` (mirrors the command-echo in execute_command, which renders), drop
    /// any pending input and clear the in-progress marker, returning to a fresh
    /// prompt. Does NOT close the terminal. The child, if any, still finishes in
    /// the background and its output is still shown.
    fn interrupt_line(&mut self) {
        let echoed = format!(
            "\x1b[32m{}@{}\x1b[0m:\x1b[36m{}\x1b[0m{} {}^C",
            self.username, self.hostname, self.display_dir(), self.prompt_char, self.input
        );
        self.add_output(OutputLine::normal(echoed));
        self.command_in_progress = false;
        self.current_task_id = None;
        self.input.clear();
        self.cursor_position = 0;
    }
}

/// Visual rows a ratatui Line occupies at the given terminal width (accounting for wrap).
fn line_vrows(line: &ratatui::text::Line, term_width: u16) -> u16 {
    if term_width == 0 {
        return 1;
    }
    let char_w: u16 = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars())
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(1) as u16)
        .sum();
    // An empty line still occupies 1 row.
    (char_w.max(1) + term_width - 1) / term_width
}

fn draw_ui(f: &mut Frame, state: &mut AppState) {
    let size = f.size();
    let area = size;
    let term_width = area.width;

    let reserved = if state.debug_mode { 2u16 } else { 1u16 };
    let output_rows = area.height.saturating_sub(reserved);

    // Build all parsed lines.
    let mut all_lines: Vec<ratatui::text::Line<'static>> = Vec::new();
    for idx in 0..state.output.len() {
        let line = &mut state.output[idx];
        if line.is_system {
            all_lines.push(ratatui::text::Line::from(
                Span::styled(line.text.clone(), Style::default().fg(Color::Cyan)),
            ));
        } else if line.is_error {
            all_lines.push(ratatui::text::Line::from(
                Span::styled(line.text.clone(), Style::default().fg(Color::Red)),
            ));
        } else {
            if line.cached_lines.is_none() {
                line.cached_lines = Some(match line.text.as_str().into_text() {
                    Ok(text) => text.lines,
                    Err(_) => vec![ratatui::text::Line::from(line.text.clone())],
                });
            }
            all_lines.extend(line.cached_lines.as_ref().unwrap().iter().cloned());
        }
    }

    // Apply scroll: walk backwards from (total - scroll_offset), accumulating
    // visual rows until we fill output_rows. This correctly handles wrapped lines.
    let base = all_lines.len().saturating_sub(state.scroll_offset);
    let mut accum: u16 = 0;
    let mut start_idx = base;
    for i in (0..base).rev() {
        let vr = line_vrows(&all_lines[i], term_width);
        if accum + vr > output_rows {
            start_idx = i + 1;
            break;
        }
        accum += vr;
        start_idx = i;
    }
    let output_lines: Vec<_> = all_lines[start_idx..base].to_vec();

    // Actual visual rows the selected lines occupy.
    let actual_vrows: u16 = output_lines.iter().map(|l| line_vrows(l, term_width)).sum();

    // Render output — height = actual_vrows so Paragraph doesn't add padding.
    let output_area = ratatui::layout::Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: actual_vrows.min(output_rows),
    };
    f.render_widget(Paragraph::new(output_lines), output_area);

    // Prompt sits directly below the last output line (TTY behaviour).
    // Clamp so it never falls off screen.
    let after_output = (area.y + actual_vrows).min(area.y + output_rows);
    let input_y = after_output.min(area.y + area.height - 1);
    let debug_y = input_y.saturating_sub(1);

    let buf = f.buffer_mut();

    // Add debug info on its row
    if state.debug_mode {
        let backend_name = if let Ok(core) = state.core.try_lock() {
            format!("{:?}", core.default_backend())
        } else {
            "Unknown".to_string()
        };
        let debug_info = format!(
            "[DEBUG] b:{} enc:{} l:{} s:{} t:{:?}",
            backend_name,
            state.backend_encoding,
            state.lines_received,
            state.scroll_offset,
            state.current_task_id
        );
        let mut col = area.x;
        for ch in debug_info.chars() {
            if col < area.x + area.width {
                buf.get_mut(col, debug_y)
                    .set_char(ch)
                    .set_style(Style::default().fg(Color::Yellow));
                col += ch.width().unwrap_or(1) as u16;
            }
        }
    }

    // Input prompt at the last line — colored like bash PS1
    let cursor_pos = state.cursor_position.min(state.input.len());
    let before_cursor = &state.input[..cursor_pos];
    let after_cursor = if cursor_pos < state.input.len() {
        &state.input[cursor_pos..]
    } else {
        ""
    };

    // Prompt segments: [user@host] [:] [path] [$] [input]
    let user_host = format!("{}@{}", state.username, state.hostname);
    let path_part = state.display_dir().to_string();
    let suffix = format!("{} ", state.prompt_char);
    let prompt_text = format!("{}:{}{}", user_host, path_part, suffix);

    // Write colored prompt + input char-by-char
    let mut col = area.x;
    let write_str = |buf: &mut ratatui::buffer::Buffer, col: &mut u16, s: &str, style: Style| {
        for ch in s.chars() {
            if *col < area.x + area.width {
                buf.get_mut(*col, input_y).set_char(ch).set_style(style);
                *col += ch.width().unwrap_or(1) as u16;
            }
        }
    };

    write_str(buf, &mut col, &user_host, Style::default().fg(Color::Green));
    write_str(buf, &mut col, ":", Style::default().fg(Color::White));
    write_str(buf, &mut col, &path_part, Style::default().fg(Color::Cyan));
    write_str(buf, &mut col, &suffix, Style::default().fg(Color::White));
    write_str(buf, &mut col, before_cursor, Style::default().fg(Color::White));
    write_str(buf, &mut col, after_cursor, Style::default().fg(Color::White));

    // Set cursor position
    let prompt_width: u16 = prompt_text.chars().map(|c: char| c.width().unwrap_or(1) as u16).sum();
    let before_cursor_width: u16 = before_cursor.chars().map(|c: char| c.width().unwrap_or(1) as u16).sum();
    let cursor_x = prompt_width + before_cursor_width;
    let _ = f.set_cursor(area.x + cursor_x, input_y);
}

enum AppEvent {
    Backend(SessionEvent),
    MouseScroll(i32), // positive = scroll up (older content), negative = scroll down
}

/// Install a WH_MOUSE_LL global hook that sends wheel events to `tx`.
/// Calls CallNextHookEx so text selection and all other mouse behaviour is unaffected.
#[cfg(windows)]
fn install_mouse_hook(tx: mpsc::UnboundedSender<AppEvent>) {
    use std::sync::OnceLock;
    use windows::Win32::Foundation::{HWND, LRESULT, LPARAM, WPARAM};
    use windows::Win32::System::Console::GetConsoleWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, SetWindowsHookExW,
        UnhookWindowsHookEx, MSG, MSLLHOOKSTRUCT, HHOOK,
        WH_MOUSE_LL, WM_MOUSEWHEEL,
    };

    static TX: OnceLock<mpsc::UnboundedSender<AppEvent>> = OnceLock::new();
    static CONSOLE_HWND: OnceLock<isize> = OnceLock::new();

    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 && wparam.0 as u32 == WM_MOUSEWHEEL {
            let ms = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            // Filter: only handle wheel events on our console window
            use windows::Win32::UI::WindowsAndMessaging::WindowFromPoint;
            let hwnd_at = WindowFromPoint(ms.pt);
            if let Some(&console_hwnd) = CONSOLE_HWND.get() {
                if hwnd_at.0 as isize == console_hwnd {
                    let delta = (ms.mouseData >> 16) as i16;
                    if let Some(tx) = TX.get() {
                        let _ = tx.send(AppEvent::MouseScroll(delta as i32));
                    }
                }
            }
        }
        CallNextHookEx(HHOOK::default(), code, wparam, lparam)
    }

    let _ = TX.set(tx);
    unsafe {
        let hwnd = GetConsoleWindow();
        let _ = CONSOLE_HWND.set(hwnd.0 as isize);
    }

    std::thread::spawn(|| unsafe {
        let hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0) {
            Ok(h) => h,
            Err(_) => return,
        };
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            // No TranslateMessage/DispatchMessage needed — hook callbacks fire during GetMessageW
        }
        let _ = UnhookWindowsHookEx(hook);
    });
}

async fn run_event_loop(
    core: Arc<tokio::sync::Mutex<WstCore>>,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    loop {
        interval.tick().await;
        let mut core_guard = core.lock().await;
        if let Ok(events) = core_guard.tick() {
            for event in events {
                let _ = tx.send(AppEvent::Backend(event));
            }
        }
    }
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut state: AppState,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let core_clone = state.core.clone();

    // Install low-level mouse hook for scroll wheel support in conhost.
    // Uses CallNextHookEx so text selection is completely unaffected.
    #[cfg(windows)]
    install_mouse_hook(tx.clone());

    let rt = tokio::runtime::Runtime::new()?;
    rt.spawn(run_event_loop(core_clone, tx));

    let tick_rate = Duration::from_millis(100);

    while state.running {
        // Ctrl+C that arrived as a console control event (signal path) sets this
        // from the handler thread; surface it as a tty-style line interrupt.
        #[cfg(windows)]
        if CTRL_C_PENDING.swap(false, std::sync::atomic::Ordering::Relaxed) {
            state.interrupt_line();
        }

        if state.needs_redraw {
            terminal.draw(|f| draw_ui(f, &mut state))?;
            state.needs_redraw = false;
        }

        // Process ALL available backend events
        loop {
            match rx.try_recv() {
                Ok(AppEvent::Backend(SessionEvent::Output(chunk))) => {
                    state.lines_received += 1;
                    // Suppress cygctl su's "[su] Acquiring SYSTEM token…" status
                    // line, which would otherwise precede every root-mode command.
                    if state.root_mode && chunk.text.trim_start().starts_with("[su] ") {
                        // swallow
                    } else if chunk.is_stderr {
                        state.add_output(OutputLine::error(chunk.text));
                    } else {
                        state.add_output(OutputLine::normal(chunk.text));
                    }
                }
                Ok(AppEvent::Backend(SessionEvent::TaskUpdated { task_id, status })) => {
                    if let TaskStatus::Exited(code) = status {
                        if state.current_task_id == Some(task_id) {
                            state.command_in_progress = false;
                            if code != 0 {
                                state.add_output(OutputLine::system(format!(
                                    "Process exited with code {}",
                                    code
                                )));
                            }
                            state.needs_redraw = true;
                        }
                    }
                }
                Ok(AppEvent::Backend(SessionEvent::SessionStarted(id))) => {
                    state.session_id = Some(id);
                    state.add_output(OutputLine::system(format!("Session {} started", id)));
                }
                Ok(AppEvent::Backend(SessionEvent::Debug { message })) => {
                    if state.debug_mode {
                        state.add_output(OutputLine::system(format!("[DEBUG] {}", message)));
                    }
                }
                Ok(AppEvent::MouseScroll(delta)) => {
                    if delta > 0 {
                        state.scroll_output_up(3);
                    } else {
                        state.scroll_output_down(3);
                    }
                    state.needs_redraw = true;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    break; // No more events, exit loop
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    break;
                }
            }
        }

        if crossterm::event::poll(tick_rate)? {
            match event::read()? {
                Event::Key(key) => {
                    // Fix duplicate key events on Windows - only handle Press events
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    // tty semantics: Ctrl+C interrupts the current line / running
                    // command and returns to a fresh prompt showing `^C`; it does
                    // NOT close the terminal. Terminals encode Ctrl+C either as
                    // Char('c'/'C')+CONTROL or as the ETX control char '\u{3}',
                    // so match both.
                    let is_ctrl_c = (matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                        || key.code == KeyCode::Char('\u{3}');
                    if is_ctrl_c {
                        state.interrupt_line();
                    } else {
                        state.handle_input(key);
                    }
                }
                _ => {}
            }
        }
    }

    // execute!(terminal.backend_mut(), crossterm::event::DisableMouseCapture)?;
    Ok(())
}

fn init_utf8_console() -> Result<()> {
    // Set console to UTF-8 via chcp
    let _ = std::process::Command::new("chcp")
        .args(["65001"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    Ok(())
}

/// Set when the console delivers Ctrl+C / Ctrl+Break as a control event (the
/// signal path). The main loop polls this and turns it into a line interrupt.
#[cfg(windows)]
static CTRL_C_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Console control handler: swallow Ctrl+C / Ctrl+Break so the OS does NOT
/// terminate wst-ui (the "Ctrl+C closes the terminal" bug), recording the event
/// for the main loop. All other control events (close/logoff/shutdown) fall
/// through to default handling so the process can still be shut down normally.
#[cfg(windows)]
unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> windows::Win32::Foundation::BOOL {
    use windows::Win32::System::Console::{CTRL_C_EVENT, CTRL_BREAK_EVENT};
    use windows::Win32::Foundation::BOOL;
    if ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT {
        CTRL_C_PENDING.store(true, std::sync::atomic::Ordering::Relaxed);
        BOOL(1) // handled — do not terminate
    } else {
        BOOL(0) // not handled — run the default handler
    }
}

fn main() -> Result<()> {
    // Initialize UTF-8 console
    let _ = init_utf8_console();

    // Write our console HWND to a temp file so the daemon can find this window
    #[cfg(windows)]
    {
        use windows::Win32::System::Console::GetConsoleWindow;
        unsafe {
            let hwnd = GetConsoleWindow();
            if !hwnd.is_invalid() {
                let path = std::env::temp_dir().join("wst_ui_hwnd.txt");
                let _ = std::fs::write(&path, (hwnd.0 as usize).to_string());
            }
        }
    }

    let config = WstConfig::load_default()?;
    let fullscreen_enabled = config.fullscreen;
    let alternate_screen = config.alternate_screen;

    // When started hidden by the daemon (SW_HIDE), the window is not visible yet.
    // The daemon will handle fullscreen via PostMessage after showing the window.
    // Skip self-managed fullscreen to avoid conflicting key presses.
    #[cfg(windows)]
    let self_manage_fullscreen = {
        use windows::Win32::System::Console::GetConsoleWindow;
        use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
        unsafe {
            let hwnd = GetConsoleWindow();
            !hwnd.is_invalid() && IsWindowVisible(hwnd).as_bool()
        }
    };
    #[cfg(not(windows))]
    let self_manage_fullscreen = true;

    // Enable F11 fullscreen BEFORE entering alternate screen
    // Windows Terminal's F11 works at window level, not terminal level
    #[cfg(windows)]
    if fullscreen_enabled && self_manage_fullscreen {
        set_windows_terminal_fullscreen(true);
    }

    enable_raw_mode()?;

    // Stop the Windows console from killing wst-ui on Ctrl+C / Ctrl+Break:
    // install a control handler that swallows them, and clear
    // ENABLE_PROCESSED_INPUT so Ctrl+C is delivered as a key event (handled by
    // AppState::interrupt_line to echo `^C` and return to a fresh prompt).
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Console::{
            SetConsoleCtrlHandler, GetStdHandle, GetConsoleMode, SetConsoleMode,
            STD_INPUT_HANDLE, ENABLE_PROCESSED_INPUT, CONSOLE_MODE,
        };
        use windows::Win32::Foundation::BOOL;
        let _ = SetConsoleCtrlHandler(Some(console_ctrl_handler), BOOL(1));
        if let Ok(h) = GetStdHandle(STD_INPUT_HANDLE) {
            let mut mode = CONSOLE_MODE(0);
            if GetConsoleMode(h, &mut mode).is_ok() {
                let _ = SetConsoleMode(h, mode & !ENABLE_PROCESSED_INPUT);
            }
        }
    }

    let mut stdout = io::stdout();
    if alternate_screen {
        execute!(stdout, EnterAlternateScreen)?;
    }
    // Enable alternate scroll mode for Windows Terminal (converts wheel to Up/Down)
    print!("\x1b[?1007h");
    use std::io::Write;
    io::stdout().flush()?;

    // Try Alt+Enter for legacy console fullscreen (after alternate screen)
    #[cfg(windows)]
    if fullscreen_enabled && self_manage_fullscreen {
        set_legacy_console_fullscreen();
    }

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let state = AppState::new(config);
    let result = run_app(&mut terminal, state);

    // Disable alternate scroll mode before leaving
    print!("\x1b[?1007l");
    io::stdout().flush()?;

    disable_raw_mode()?;

    if alternate_screen {
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    }

    result?;

    #[cfg(windows)]
    let _ = std::fs::remove_file(std::env::temp_dir().join("wst_ui_hwnd.txt"));

    println!("WST exited. Goodbye!");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::AppState;

    #[test]
    fn become_root_patterns_are_recognized() {
        for c in [
            "su", "su root", "su -", "su - root", "su -l", "su -l root",
            "sudo -i", "sudo -s", "sudo su", "sudo su root", "sudo -i root",
        ] {
            assert!(AppState::is_become_root(c), "should enter root: {c:?}");
        }
    }

    #[test]
    fn ordinary_commands_do_not_enter_root() {
        for c in [
            "whoami", "sudo apt update", "su someoneelse", "sudo su bob",
            "ls -la", "sudoku", "super", "", "root",
        ] {
            assert!(!AppState::is_become_root(c), "should NOT enter root: {c:?}");
        }
    }

    #[test]
    fn become_root_tolerates_extra_whitespace() {
        assert!(AppState::is_become_root("  sudo   su   root  "));
        assert!(AppState::is_become_root("su\troot"));
    }
}
