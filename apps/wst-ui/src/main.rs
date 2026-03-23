mod builtin;

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
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame, Terminal,
};
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
#[allow(dead_code)]
const CP_UTF8: u32 = 65001;

#[derive(Clone)]
struct OutputLine {
    text: String,
    is_error: bool,
    is_system: bool,
}

impl OutputLine {
    fn normal(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            is_system: false,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            is_system: false,
        }
    }

    fn system(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            is_system: true,
        }
    }
}

struct AppState {
    core: Arc<tokio::sync::Mutex<WstCore>>,
    input: String,
    cursor_position: usize,
    output: Vec<OutputLine>,
    running: bool,
    session_id: Option<u64>,
    scroll_offset: usize,
    current_task_id: Option<u64>,
    command_in_progress: bool,
    current_dir: String,
    debug_mode: bool,
    // Debug stats
    lines_received: usize,
    last_command: String,
    backend_encoding: String,
}

impl AppState {
    fn new(config: WstConfig) -> Self {
        let core = Arc::new(tokio::sync::Mutex::new(WstCore::new(config)));
        let current_dir = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("C:\\Users\\Administrator"))
            .to_string_lossy()
            .to_string();

        Self {
            core,
            input: String::new(),
            cursor_position: 0,
            output: vec![
                OutputLine::system(format!("WST v{} - Windows Subsystem for TTY", VERSION)),
                OutputLine::normal("Type :help for available commands"),
                OutputLine::normal(""),
            ],
            running: true,
            session_id: None,
            scroll_offset: 0,
            current_task_id: None,
            command_in_progress: false,
            current_dir,
            debug_mode: false,
            lines_received: 0,
            last_command: String::new(),
            backend_encoding: "UTF-8".to_string(),
        }
    }

    fn handle_input(&mut self, key: KeyEvent) {
        match key.code {
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
                    self.input.insert(self.cursor_position, c);
                    self.move_cursor_right();
                }
            }
            KeyCode::Backspace => {
                if self.cursor_position > 0 {
                    self.input.remove(self.cursor_position - 1);
                    self.move_cursor_left();
                }
            }
            KeyCode::Delete => {
                if self.cursor_position < self.input.len() {
                    self.input.remove(self.cursor_position);
                }
            }
            KeyCode::Left => self.move_cursor_left(),
            KeyCode::Right => self.move_cursor_right(),
            KeyCode::Home => self.cursor_position = 0,
            KeyCode::End => self.cursor_position = self.input.len(),
            // Up/Down now scroll output (WT converts mouse wheel to these keys)
            KeyCode::Up => self.scroll_output_up(3),
            KeyCode::Down => self.scroll_output_down(3),
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
        self.cursor_position = self.cursor_position.saturating_sub(1);
    }

    fn move_cursor_right(&mut self) {
        if self.cursor_position < self.input.len() {
            self.cursor_position += 1;
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

        self.output.push(OutputLine::normal(format!("{}> {}", self.current_dir, command)));
        self.scroll_to_bottom();

        // Track command and reset stats
        self.last_command = command.clone();
        self.lines_received = 0;

        if command.starts_with(':') {
            self.handle_builtin(&command);
        } else {
            if let Ok(mut core) = self.core.try_lock() {
                // Reset session if backend was switched
                if let Ok(sid) = core.ensure_session() {
                    self.session_id = Some(sid);
                }
                match core.exec(command.clone()) {
                    Ok(_tid) => {
                        self.current_task_id = Some(_tid);
                    }
                    Err(e) => {
                        self.output.push(OutputLine::error(format!("Error: {}", e)));
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
                self.output.push(OutputLine::system("Builtin commands:"));
                self.output.push(OutputLine::normal("  :help        - Show this help"));
                self.output.push(OutputLine::normal("  :status      - Show current status"));
                self.output.push(OutputLine::normal("  :clear       - Clear output"));
                self.output.push(OutputLine::normal("  :history     - Show command history"));
                self.output.push(OutputLine::normal("  :backend     - Switch backend (Cygctl|Pwsh|Cmd)"));
                self.output.push(OutputLine::normal("  :debug       - Toggle debug mode"));
                self.output.push(OutputLine::normal("  :exit / :q   - Exit WST"));
                self.output.push(OutputLine::normal(""));
                self.output.push(OutputLine::system("Keyboard shortcuts:"));
                self.output.push(OutputLine::normal("  PageUp/Down  - Scroll output"));
                self.output.push(OutputLine::normal("  Mouse wheel  - Scroll output"));
                self.output.push(OutputLine::normal("  Shift+Drag   - Select text (Windows Terminal)"));
            }
            ":status" => {
                if let Ok(core) = self.core.try_lock() {
                    self.output.push(OutputLine::system(format!(
                        "WST v{} - Windows Subsystem for TTY",
                        VERSION
                    )));
                    self.output
                        .push(OutputLine::normal(format!("Backend: {:?}", core.default_backend())));
                    self.output
                        .push(OutputLine::normal(format!("Session: {:?}", self.session_id)));
                    self.output.push(OutputLine::normal(format!(
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
                        self.output
                            .push(OutputLine::normal(format!("  {}: {}", i + 1, entry.command)));
                    }
                }
            }
            ":backend" => {
                if parts.len() < 2 {
                    if let Ok(core) = self.core.try_lock() {
                        self.output.push(OutputLine::normal(format!(
                            "Current backend: {:?}",
                            core.default_backend()
                        )));
                        self.output
                            .push(OutputLine::normal("Usage: :backend <Cygctl|Pwsh|Cmd>"));
                    }
                } else {
                    let new_backend = match parts[1].to_lowercase().as_str() {
                        "cygctl" => Some(BackendKind::Cygctl),
                        "pwsh" => Some(BackendKind::Pwsh),
                        "cmd" => Some(BackendKind::Cmd),
                        _ => None,
                    };

                    if let Some(kind) = new_backend {
                        if let Ok(mut core) = self.core.try_lock() {
                            match core.switch_backend(kind) {
                                Ok(()) => {
                                    self.output.push(OutputLine::system(format!(
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
                                    }
                                }
                                Err(e) => {
                                    self.output.push(OutputLine::error(format!("Failed to switch: {}", e)));
                                }
                            }
                        }
                    } else {
                        self.output
                            .push(OutputLine::error("Unknown backend. Use: Cygctl, Pwsh, or Cmd"));
                    }
                }
            }
            ":debug" => {
                self.debug_mode = !self.debug_mode;
                self.output.push(OutputLine::system(format!(
                    "Debug mode: {}",
                    if self.debug_mode { "ON" } else { "OFF" }
                )));
            }
            ":exit" | ":q" => self.running = false,
            _ => {
                self.output.push(OutputLine::error(format!("Unknown builtin: {}", cmd)));
                self.output.push(OutputLine::normal("Type :help for available commands"));
            }
        }
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    fn add_output(&mut self, line: OutputLine) {
        self.output.push(line);
        // Auto-scroll to bottom so new output is always visible
        self.scroll_to_bottom();
    }
}

fn draw_ui(f: &mut Frame, state: &mut AppState) {
    let size = f.size();

    // Layout: status bar (top) | main content (output + input)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Status bar at top
            Constraint::Min(1),    // Main content
        ])
        .split(size);

    // Draw status bar at top
    let backend_name = if let Ok(core) = state.core.try_lock() {
        format!("{:?}", core.default_backend())
    } else {
        "Unknown".to_string()
    };

    let mut status_line = vec![
        Span::styled(" WST v", Style::default().fg(Color::Black).bg(Color::Cyan)),
        Span::styled(VERSION, Style::default().fg(Color::Black).bg(Color::Cyan)),
        Span::styled(" | ", Style::default().fg(Color::Cyan).bg(Color::Black)),
        Span::styled(
            format!("{} ", backend_name),
            Style::default().fg(Color::Green).bg(Color::Black),
        ),
    ];

    // Show DEBUG indicator when debug mode is on
    if state.debug_mode {
        status_line.push(Span::styled(
            "DEBUG ",
            Style::default().fg(Color::Red).bg(Color::Black),
        ));
    }

    status_line.push(Span::styled(
        format!("Sess:{:?} Hist:{} ",
            state.session_id,
            if let Ok(core) = state.core.try_lock() { core.history().len() } else { 0 }
        ),
        Style::default().fg(Color::DarkGray).bg(Color::Black),
    ));

    let status_paragraph = Paragraph::new(Line::from(status_line))
        .style(Style::default().bg(Color::Rgb(20, 20, 30)));
    f.render_widget(status_paragraph, chunks[0]);

    // Draw output + input in main area
    let area = chunks[1];
    let buf = f.buffer_mut();

    let text_width = area.width;
    let reserved = if state.debug_mode { 2u16 } else { 1u16 };
    let output_rows = area.height.saturating_sub(reserved);

    // Calculate which output lines to show
    let output_len = state.output.len();
    let visible_count = output_rows as usize;
    let (start, count) = if output_len <= visible_count {
        (0, output_len)
    } else {
        let offset = state.scroll_offset.min(output_len - visible_count);
        (output_len - visible_count - offset, visible_count)
    };

    // Render output lines; y tracks the next free row after output
    let output_area_bottom = area.y + output_rows.saturating_sub(1);
    let text_right = area.x + text_width;
    let mut y = area.y;
    for line in state.output.iter().skip(start).take(count) {
        if y > output_area_bottom {
            break;
        }
        let style = if line.is_error {
            Style::default().fg(Color::Red)
        } else if line.is_system {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::Reset)
        };
        let text = line.text.replace('\t', "        ");
        let mut col = area.x;
        for ch in text.chars() {
            let w = ch.width().unwrap_or(1) as u16;
            if col + w > text_right {
                break;
            }
            buf.get_mut(col, y).set_char(ch).set_style(style);
            col += w;
        }
        y += 1;
    }

    // Place debug and input right after last output line (clamped to area bounds)
    let (debug_y, input_y) = if state.debug_mode {
        let d = y.min(area.y + area.height - 2);
        let i = (y + 1).min(area.y + area.height - 1);
        (d, i)
    } else {
        (0, y.min(area.y + area.height - 1))
    };

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

    // Input prompt at the last line
    let cursor_pos = state.cursor_position.min(state.input.len());
    let before_cursor = &state.input[..cursor_pos];
    let after_cursor = if cursor_pos < state.input.len() {
        &state.input[cursor_pos..]
    } else {
        ""
    };

    let prompt_text = format!("{}>", state.current_dir);
    let input_line = format!("{}{}{}", prompt_text, before_cursor, after_cursor);

    // Write input line
    let mut col = area.x;
    for ch in input_line.chars() {
        if col < area.x + area.width {
            buf.get_mut(col, input_y)
                .set_char(ch)
                .set_style(Style::default().fg(Color::White));
            col += ch.width().unwrap_or(1) as u16;
        }
    }

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
        terminal.draw(|f| draw_ui(f, &mut state))?;

        // Process ALL available backend events
        loop {
            match rx.try_recv() {
                Ok(AppEvent::Backend(SessionEvent::Output(chunk))) => {
                    state.lines_received += 1;
                    if chunk.is_stderr {
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
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            state.running = false;
                        }
                        _ => state.handle_input(key),
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
