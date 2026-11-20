//! Smoke test for ConPtyBackend (same code path wst-ui uses).
//! Requires wst-conpty-broker.exe next to this example binary.
use std::time::{Duration, Instant};
use wst_backend::{Backend, ConPtyBackend};
use wst_protocol::{ExecRequest, SessionEvent};

fn drain(b: &mut ConPtyBackend, sid: u64, ms: u64, out: &mut String) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        for ev in b.poll_events(sid as _).unwrap() {
            if let SessionEvent::Output(c) = ev {
                out.push_str(&c.text);
                out.push('\n');
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn main() {
    let shell = std::env::args().nth(1).unwrap_or_else(|| "powershell.exe".into());
    let mut b = ConPtyBackend::new(&shell);
    let sid = b.spawn_session().expect("spawn_session");
    let mut out = String::new();
    drain(&mut b, sid as _, 3000, &mut out);
    for cmd in ["echo WST_MARK_42", "echo 中文测试", "cd C:\\Windows; pwd"] {
        b.exec(sid, ExecRequest { command_line: cmd.into(), cwd: None, env: vec![] }).unwrap();
        drain(&mut b, sid as _, 2000, &mut out);
    }
    // Long output: every line must arrive whole (no splits, no U+FFFD).
    let n = 3000;
    let long_cmd = if shell.contains("cmd") {
        format!("for /L %i in (1,1,{n}) do @echo 行%i 中文测试长输出")
    } else {
        format!("1..{n} | % {{ \"行$_ 中文测试长输出\" }}")
    };
    let mut long_out = String::new();
    b.exec(sid, ExecRequest { command_line: long_cmd, cwd: None, env: vec![] }).unwrap();
    drain(&mut b, sid as _, 8000, &mut long_out);
    let long_clean = strip_ansi(&long_out);
    let whole: std::collections::HashSet<&str> = long_clean.lines().map(|l| l.trim_end()).collect();
    let missing: Vec<usize> = (1..=n).filter(|i| !whole.contains(format!("行{i} 中文测试长输出").as_str())).collect();
    println!("LONG: {}/{} lines intact, missing sample {:?}", n - missing.len(), n, &missing[..missing.len().min(5)]);
    println!("LONG REPLACEMENT CHARS: {}", long_clean.matches('\u{FFFD}').count());
    println!("PROMPT after long output: {:?}", long_clean.lines().rev().find(|l| !l.trim().is_empty()));

    let clean: String = strip_ansi(&out);
    println!("MARK: {}", clean.matches("WST_MARK_42").count());
    println!("CJK : {}", clean.matches("中文测试").count());
    println!("CWD : {}", clean.contains("C:\\Windows"));
    println!("REPLACEMENT CHARS: {}", clean.matches('\u{FFFD}').count());
    std::fs::write(std::env::temp_dir().join("wst_conpty_smoke.txt"), &clean).unwrap();
}

fn strip_ansi(s: &str) -> String {
    let mut r = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            match it.next() {
                Some('[') => { while let Some(x) = it.next() { if x.is_ascii_alphabetic() { break; } } }
                Some(']') => { while let Some(x) = it.next() { if x == '\x07' { break; } } }
                _ => {}
            }
        } else { r.push(c); }
    }
    r
}
