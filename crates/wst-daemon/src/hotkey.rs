//! Global hotkey handling for WST daemon

use crate::DaemonState;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::Duration;


// Hotkey modifier constants
const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const MOD_SHIFT: u32 = 0x0004;
const MOD_WIN: u32 = 0x0008;

/// Hotkey configuration
#[derive(Debug, Clone)]
pub struct HotkeyConfig {
    /// Virtual key code
    pub vk: u32,
    /// Modifiers (CTRL, ALT, SHIFT)
    pub modifiers: u32,
}

impl HotkeyConfig {
    /// Create default hotkey (Ctrl+Alt+F12) - F12 is less likely to conflict
    pub fn default_wst_hotkey() -> Self {
        Self {
            vk: 0x7B, // VK_F12
            modifiers: MOD_CONTROL | MOD_ALT,
        }
    }

    /// Parse from string (e.g., "Ctrl+Alt+F3")
    pub fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split('+').collect();
        let mut modifiers = 0u32;
        let mut vk = None;

        for part in parts {
            match part.trim().to_uppercase().as_str() {
                "CTRL" | "CONTROL" => modifiers |= MOD_CONTROL,
                "ALT" => modifiers |= MOD_ALT,
                "SHIFT" => modifiers |= MOD_SHIFT,
                "WIN" | "WINDOWS" => modifiers |= MOD_WIN,
                "SPACE" => vk = Some(0x20), // VK_SPACE
                "F1" => vk = Some(0x70), // VK_F1
                "F2" => vk = Some(0x71),
                "F3" => vk = Some(0x72), // VK_F3 - default WST hotkey
                "F4" => vk = Some(0x73),
                "F5" => vk = Some(0x74),
                "F6" => vk = Some(0x75),
                "F7" => vk = Some(0x76),
                "F8" => vk = Some(0x77),
                "F9" => vk = Some(0x78),
                "F10" => vk = Some(0x79),
                "F11" => vk = Some(0x7A),
                "F12" => vk = Some(0x7B), // VK_F12 - default WST hotkey
                _ => {
                    // Try to parse as single character
                    if part.len() == 1 {
                        let c = part.chars().next().unwrap() as u8;
                        if c.is_ascii_alphabetic() {
                            vk = Some(c.to_ascii_uppercase() as u32);
                        }
                    }
                }
            }
        }

        let vk = vk.ok_or_else(|| anyhow::anyhow!("No virtual key found in hotkey string"))?;

        Ok(Self { vk, modifiers })
    }

    /// Get the combined modifiers and vk
    pub fn as_modifiers_and_vk(&self) -> (u32, u32) {
        (self.modifiers, self.vk)
    }
}

/// Hotkey event sent to the daemon
#[derive(Debug, Clone)]
pub enum HotkeyEvent {
    /// Toggle frontend visibility
    ToggleFrontend,
    /// Show frontend
    ShowFrontend,
    /// Hide frontend
    HideFrontend,
    /// Custom hotkey with ID
    Custom(u32),
}

/// Run the hotkey listener
pub async fn run_hotkey_listener(
    state: Arc<DaemonState>,
    mut event_rx: mpsc::Receiver<HotkeyEvent>,
) -> Result<()> {
    tracing::info!("Hotkey listener starting");

    // Track the PID of the launched wst-ui process
    let mut ui_pid: Option<u32> = None;

    while !state.is_shutting_down().await {
        tokio::select! {
            Some(event) = event_rx.recv() => {
                match event {
                    HotkeyEvent::ToggleFrontend => {
                        let visible = state.toggle_frontend().await;
                        tracing::info!("Hotkey: Frontend toggled (now visible: {})", visible);

                        if visible {
                            // Show/launch UI
                            launch_or_focus_ui(&mut ui_pid).await?;
                        } else {
                            // Hide UI (close it)
                            close_ui(&mut ui_pid).await?;
                        }
                    }
                    HotkeyEvent::ShowFrontend => {
                        state.set_frontend_visible(true).await;
                        tracing::info!("Hotkey: Frontend shown");
                        launch_or_focus_ui(&mut ui_pid).await?;
                    }
                    HotkeyEvent::HideFrontend => {
                        state.set_frontend_visible(false).await;
                        tracing::info!("Hotkey: Frontend hidden");
                        close_ui(&mut ui_pid).await?;
                    }
                    HotkeyEvent::Custom(id) => {
                        tracing::debug!("Hotkey: Custom event {}", id);
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                // Continue
            }
        }
    }

    // Clean up UI process on exit
    if let Some(pid) = ui_pid {
        kill_ui_pid(pid);
    }

    tracing::info!("Hotkey listener stopped");
    Ok(())
}

/// Launch or focus the WST UI
async fn launch_or_focus_ui(ui_pid: &mut Option<u32>) -> Result<()> {
    tracing::info!("=== launch_or_focus_ui() called ===");

    // If we have a tracked PID and it's still alive, re-show the window.
    // It was hidden while fullscreen, so SW_SHOW restores it directly (first_show=false).
    if let Some(pid) = *ui_pid {
        tracing::info!("Checking existing UI process with PID: {}", pid);
        if is_pid_alive(pid) {
            tracing::info!("UI process still running, re-showing window");
            show_ui_window(false)?;
            return Ok(());
        } else {
            tracing::info!("UI process {} has exited, will relaunch", pid);
            *ui_pid = None;
        }
    } else {
        tracing::info!("No existing UI process tracked");
    }

    // Delete stale HWND file before launching so we know when the new instance writes it
    let hwnd_path = std::env::temp_dir().join("wst_ui_hwnd.txt");
    let _ = std::fs::remove_file(&hwnd_path);

    let exe_path = find_wst_ui_executable();
    tracing::info!("UI executable path: {:?}", exe_path);

    let exe_abs = std::path::Path::new(&exe_path)
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Failed to resolve UI path: {}", e))?;

    let project_root = exe_abs.parent()
        .and_then(|p| p.parent())
        .unwrap_or_else(|| std::path::Path::new("."));

    tracing::info!("Working directory: {:?}", project_root);

    // Launch wst-ui hidden (STARTF_USESHOWWINDOW + SW_HIDE + CREATE_NEW_CONSOLE)
    // so there is no visible console window flash before we go fullscreen.
    #[cfg(windows)]
    let launched_pid = {
        use windows::Win32::System::Threading::{
            CreateProcessW, PROCESS_CREATION_FLAGS, STARTUPINFOW, STARTF_USESHOWWINDOW,
        };
        use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
        use windows::core::PWSTR;
        use std::os::windows::ffi::OsStrExt;

        let exe_wide: Vec<u16> = exe_abs.as_os_str()
            .encode_wide().chain(std::iter::once(0)).collect();
        let dir_wide: Vec<u16> = project_root.as_os_str()
            .encode_wide().chain(std::iter::once(0)).collect();

        let mut si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            dwFlags: STARTF_USESHOWWINDOW,
            wShowWindow: SW_HIDE.0 as u16,
            ..Default::default()
        };
        let mut pi = windows::Win32::System::Threading::PROCESS_INFORMATION::default();

        const CREATE_NEW_CONSOLE: u32 = 0x00000010;

        unsafe {
            CreateProcessW(
                windows::core::PCWSTR(exe_wide.as_ptr()),
                PWSTR::null(),
                None, None, false,
                PROCESS_CREATION_FLAGS(CREATE_NEW_CONSOLE),
                None,
                windows::core::PCWSTR(dir_wide.as_ptr()),
                &mut si,
                &mut pi,
            ).map_err(|e| anyhow::anyhow!("Failed to launch UI: {}", e))?;

            let pid = pi.dwProcessId;
            let _ = windows::Win32::Foundation::CloseHandle(pi.hThread);
            let _ = windows::Win32::Foundation::CloseHandle(pi.hProcess);
            pid
        }
    };
    #[cfg(not(windows))]
    let launched_pid = {
        use std::process::Command;
        Command::new(&exe_path)
            .current_dir(project_root)
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to launch UI: {}", e))?
            .id()
    };

    tracing::info!("WST UI launched with PID: {}", launched_pid);
    *ui_pid = Some(launched_pid);

    // Wait for wst-ui to write its HWND (up to 5 seconds)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !hwnd_path.exists() {
        if std::time::Instant::now() > deadline {
            tracing::warn!("Timed out waiting for wst-ui HWND file");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    if hwnd_path.exists() {
        tracing::info!("HWND file ready, showing wst-ui fullscreen (first launch)");
        show_ui_window(true)?;
    }

    Ok(())
}

/// Close the WST UI (hide window, keep process alive)
async fn close_ui(ui_pid: &mut Option<u32>) -> Result<()> {
    tracing::info!("=== close_ui() called ===");

    if let Some(pid) = *ui_pid {
        tracing::info!("UI process PID: {}", pid);

        #[cfg(windows)]
        {
            tracing::info!("Attempting to hide WST UI window (keeping process alive)");
            match hide_ui_window() {
                Ok(()) => {
                    tracing::info!("UI window hidden successfully");
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!("Hide window failed: {}, killing process", e);
                    kill_ui_pid(pid);
                    *ui_pid = None;
                }
            }
        }
        #[cfg(not(windows))]
        {
            kill_ui_pid(pid);
            *ui_pid = None;
        }
    }
    Ok(())
}

/// Show the WST UI window using Windows API.
/// `first_show`: true when window is freshly launched (needs fullscreen setup),
///               false when re-showing a previously hidden window (already fullscreen).
#[cfg(windows)]
fn show_ui_window(first_show: bool) -> Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{
        ShowWindow, SetForegroundWindow, IsWindow, BringWindowToTop,
        SetWindowPos, SWP_NOSIZE, SWP_NOZORDER, SWP_NOACTIVATE, SW_SHOW,
        PostMessageW, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };
    use windows::Win32::Foundation::{WPARAM, LPARAM};
    use std::thread;
    use std::time::Duration;

    tracing::info!("=== show_ui_window(first_show={}) called ===", first_show);

    let hwnd = read_ui_hwnd()?;
    tracing::info!("wst-ui HWND from file: {:?}", hwnd);

    unsafe {
        if !IsWindow(hwnd).as_bool() {
            return Err(anyhow::anyhow!("wst-ui window is no longer valid"));
        }

        // Hide from taskbar and Alt+Tab by removing WS_EX_APPWINDOW and adding WS_EX_TOOLWINDOW
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowLongW, SetWindowLongW, GWL_EXSTYLE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
        };
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        SetWindowLongW(hwnd, GWL_EXSTYLE,
            (ex_style & !(WS_EX_APPWINDOW.0 as i32)) | WS_EX_TOOLWINDOW.0 as i32);

        if first_show {
            // First launch: window is hidden and not yet fullscreen.
            // Move off-screen first so the user doesn't see the normal-size flash,
            // then show it, wait for conhost to be ready, and toggle fullscreen.
            let _ = SetWindowPos(hwnd, None, -32000, -32000, 0, 0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = BringWindowToTop(hwnd);
            let _ = SetForegroundWindow(hwnd);

            thread::sleep(Duration::from_millis(300));

            let _ = PostMessageW(hwnd, WM_KEYDOWN,    WPARAM(0x7A), LPARAM(0x00570001));
            let _ = PostMessageW(hwnd, WM_KEYUP,      WPARAM(0x7A), LPARAM(0xC0570001u64 as _));
            thread::sleep(Duration::from_millis(200));
            let _ = PostMessageW(hwnd, WM_SYSKEYDOWN, WPARAM(0x0D), LPARAM(0x201C0001));
            let _ = PostMessageW(hwnd, WM_SYSKEYUP,   WPARAM(0x0D), LPARAM(0xC01C0001u64 as _));
        } else {
            // Re-show: window was hidden while already fullscreen (SW_HIDE preserves state).
            // Just show it — it comes back fullscreen directly.
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = BringWindowToTop(hwnd);
            let _ = SetForegroundWindow(hwnd);
        }
    }

    tracing::info!("=== show_ui_window() completed successfully ===");
    Ok(())
}

/// Read the wst-ui HWND from the temp file written by wst-ui at startup
#[cfg(windows)]
fn read_ui_hwnd() -> Result<windows::Win32::Foundation::HWND> {
    let path = std::env::temp_dir().join("wst_ui_hwnd.txt");
    let raw: usize = std::fs::read_to_string(&path)
        .map_err(|_| anyhow::anyhow!("wst-ui HWND file not found (is wst-ui running?)"))?
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("wst-ui HWND file is invalid"))?;
    Ok(windows::Win32::Foundation::HWND(raw as *mut core::ffi::c_void))
}

/// Hide the WST UI window — just SW_HIDE, preserving the fullscreen state.
/// When shown again via SW_SHOW, the window returns to fullscreen directly.
#[cfg(windows)]
fn hide_ui_window() -> Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, IsWindow};

    tracing::info!("=== hide_ui_window() called ===");

    let hwnd = read_ui_hwnd()?;
    tracing::info!("wst-ui HWND from file: {:?}", hwnd);

    unsafe {
        if !IsWindow(hwnd).as_bool() {
            return Err(anyhow::anyhow!("wst-ui window is no longer valid"));
        }
        let _ = ShowWindow(hwnd, SW_HIDE);
    }

    tracing::info!("=== hide_ui_window() completed successfully ===");
    Ok(())
}

/// Find window by title and show/hide it (non-Windows stub)
#[cfg(not(windows))]
fn show_ui_window() -> Result<()> {
    Err(anyhow::anyhow!("Window show/hide not supported on this platform"))
}

#[cfg(not(windows))]
fn hide_ui_window() -> Result<()> {
    Err(anyhow::anyhow!("Window show/hide not supported on this platform"))
}

/// Check if a process with the given PID is still alive
#[cfg(windows)]
fn is_pid_alive(pid: u32) -> bool {
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows::Win32::System::Threading::GetExitCodeProcess;
    use windows::Win32::Foundation::STILL_ACTIVE;

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
        match handle {
            Ok(h) => {
                let mut code: u32 = 0;
                let alive = GetExitCodeProcess(h, &mut code).is_ok()
                    && code == STILL_ACTIVE.0 as u32;
                let _ = windows::Win32::Foundation::CloseHandle(h);
                alive
            }
            Err(_) => false,
        }
    }
}

#[cfg(not(windows))]
fn is_pid_alive(_pid: u32) -> bool {
    false
}

/// Kill the wst-ui process by PID
#[cfg(windows)]
fn kill_ui_pid(pid: u32) {
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let _ = TerminateProcess(h, 1);
            let _ = windows::Win32::Foundation::CloseHandle(h);
        }
    }
}

#[cfg(not(windows))]
fn kill_ui_pid(_pid: u32) {}

/// Find the wst-ui executable (prefer debug builds during development)
fn find_wst_ui_executable() -> String {
    let paths = vec![
        // Debug builds first
        "target/debug/wst-ui.exe",
        "../target/debug/wst-ui.exe",
        "../../target/debug/wst-ui.exe",
        // Release builds fallback
        "target/release/wst-ui.exe",
        "../target/release/wst-ui.exe",
        "../../target/release/wst-ui.exe",
        "wst-ui.exe",
    ];

    for path in paths {
        if std::path::Path::new(path).exists() {
            return path.to_string();
        }
    }

    "target/debug/wst-ui.exe".to_string()
}

/// Start the hotkey manager in a background thread using win-hotkeys
#[cfg(windows)]
pub fn start_hotkey_thread(
    config: HotkeyConfig,
    event_tx: mpsc::Sender<HotkeyEvent>,
) -> Result<std::thread::JoinHandle<()>> {
    use std::thread;
    use win_hotkeys::VKey;

    let (modifiers, vk) = config.as_modifiers_and_vk();

    tracing::info!("=== Starting hotkey thread ===");
    tracing::info!("Config: modifiers={:#x}, vk={:#x}", modifiers, vk);

    // Convert our virtual key code to win-hotkeys VKey
    let trigger_key = vk_to_vkey(vk)?;
    tracing::info!("Trigger key: {:?}", trigger_key);

    // Convert our modifiers to VKey slice
    let mut mod_keys = Vec::new();
    if modifiers & MOD_CONTROL != 0 {
        mod_keys.push(VKey::Control);
        tracing::info!("Added Control modifier");
    }
    if modifiers & MOD_ALT != 0 {
        mod_keys.push(VKey::LMenu); // LMenu = Left Alt
        tracing::info!("Added Alt modifier (LMenu)");
    }
    if modifiers & MOD_SHIFT != 0 {
        mod_keys.push(VKey::Shift);
        tracing::info!("Added Shift modifier");
    }
    if modifiers & MOD_WIN != 0 {
        mod_keys.push(VKey::LWin);
        tracing::info!("Added Win modifier");
    }

    let handle = thread::spawn(move || {
        tracing::info!("Hotkey thread started, creating HotkeyManager");

        // Create hotkey manager
        let mut hm = win_hotkeys::HotkeyManager::new();

        // Register the hotkey
        tracing::info!("Attempting to register hotkey:");
        tracing::info!("  Trigger: {:?}", trigger_key);
        tracing::info!("  Modifiers: {:?}", mod_keys);

        match hm.register_hotkey(trigger_key, &mod_keys, move || {
            tracing::info!("*** HOTKEY TRIGGERED! ***");
            tracing::info!("Sending ToggleFrontend event...");

            match event_tx.try_send(HotkeyEvent::ToggleFrontend) {
                Ok(_) => tracing::info!("Event sent successfully"),
                Err(e) => tracing::error!("Failed to send event: {}", e),
            }
        }) {
            Ok(id) => {
                tracing::info!("Hotkey registered successfully!");
                tracing::info!("Hotkey ID: {:?}", id);
                tracing::info!("Press Ctrl+Alt+F12 to toggle WST UI...");
            }
            Err(e) => {
                tracing::error!("Failed to register hotkey: {}", e);
            }
        }

        // Run the event loop
        tracing::info!("Entering event loop...");
        hm.event_loop();

        tracing::info!("Hotkey event loop exited");
    });

    Ok(handle)
}

/// Convert virtual key code to win-hotkeys VKey
#[cfg(windows)]
fn vk_to_vkey(vk: u32) -> Result<win_hotkeys::VKey> {
    use win_hotkeys::VKey;

    Ok(match vk {
        0x20 => VKey::Space,
        0x30 => VKey::Vk0,
        0x31 => VKey::Vk1,
        0x32 => VKey::Vk2,
        0x33 => VKey::Vk3,
        0x34 => VKey::Vk4,
        0x35 => VKey::Vk5,
        0x36 => VKey::Vk6,
        0x37 => VKey::Vk7,
        0x38 => VKey::Vk8,
        0x39 => VKey::Vk9,
        0x41 => VKey::A,
        0x42 => VKey::B,
        0x43 => VKey::C,
        0x44 => VKey::D,
        0x45 => VKey::E,
        0x46 => VKey::F,
        0x47 => VKey::G,
        0x48 => VKey::H,
        0x49 => VKey::I,
        0x4A => VKey::J,
        0x4B => VKey::K,
        0x4C => VKey::L,
        0x4D => VKey::M,
        0x4E => VKey::N,
        0x4F => VKey::O,
        0x50 => VKey::P,
        0x51 => VKey::Q,
        0x52 => VKey::R,
        0x53 => VKey::S,
        0x54 => VKey::T,
        0x55 => VKey::U,
        0x56 => VKey::V,
        0x57 => VKey::W,
        0x58 => VKey::X,
        0x59 => VKey::Y,
        0x5A => VKey::Z,
        0x70 => VKey::F1,
        0x71 => VKey::F2,
        0x72 => VKey::F3,
        0x73 => VKey::F4,
        0x74 => VKey::F5,
        0x75 => VKey::F6,
        0x76 => VKey::F7,
        0x77 => VKey::F8,
        0x78 => VKey::F9,
        0x79 => VKey::F10,
        0x7A => VKey::F11,
        0x7B => VKey::F12,
        _ => VKey::from_vk_code(vk as u16),
    })
}

/// Start the hotkey manager in a background thread (non-Windows stub)
#[cfg(not(windows))]
pub fn start_hotkey_thread(
    _config: HotkeyConfig,
    _event_tx: mpsc::Sender<HotkeyEvent>,
) -> Result<std::thread::JoinHandle<()>> {
    use std::thread;

    let handle = thread::spawn(move || {
        tracing::warn!("Hotkey support is only available on Windows");
        loop {
            thread::sleep(std::time::Duration::from_secs(1));
        }
    });

    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hotkey_parse() {
        let config = HotkeyConfig::parse("Ctrl+Alt+F12").unwrap();
        assert_eq!(config.vk, 0x7B); // VK_F12
        assert_eq!(config.modifiers, MOD_CONTROL | MOD_ALT);
    }

    #[test]
    fn test_hotkey_parse_f1() {
        let config = HotkeyConfig::parse("Ctrl+F1").unwrap();
        assert_eq!(config.vk, 0x70); // VK_F1
        assert_eq!(config.modifiers, MOD_CONTROL);
    }

    #[test]
    fn test_hotkey_parse_shift_ctrl_a() {
        let config = HotkeyConfig::parse("Shift+Ctrl+A").unwrap();
        assert_eq!(config.vk, b'A' as u32);
        assert_eq!(config.modifiers, MOD_SHIFT | MOD_CONTROL);
    }

    #[test]
    fn test_default_hotkey() {
        let config = HotkeyConfig::default_wst_hotkey();
        assert_eq!(config.vk, 0x7B); // VK_F12
        assert_eq!(config.modifiers, MOD_CONTROL | MOD_ALT);
    }

    #[test]
    fn test_hotkey_modifiers_and_vk() {
        let config = HotkeyConfig::default_wst_hotkey();
        let (modifiers, vk) = config.as_modifiers_and_vk();
        assert_eq!(modifiers, MOD_CONTROL | MOD_ALT);
        assert_eq!(vk, 0x7B); // VK_F12
    }
}
