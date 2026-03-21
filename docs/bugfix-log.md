# WST Bug 修复记录

## Bug #1: Backend 切换无效

**描述**: 执行 `:backend cmd` 后，实际使用的仍是默认 backend

**原因**: `switch_backend` 没有正确重置会话状态

**修复位置**: `apps/wst-ui/src/main.rs`

**修复代码**:
```rust
":backend" => {
    // ...
    if let Ok(mut core) = self.core.try_lock() {
        match core.switch_backend(kind) {
            Ok(()) => {
                self.session_id = None;           // 重置会话
                self.current_task_id = None;
                self.command_in_progress = false;
            }
            // ...
        }
    }
}
```

---

## Bug #2: dir 命令输出乱码/不完整

**描述**: 切换到 Cmd backend 后执行 `dir`，输出不完整或有乱码

**原因**:
- 输出流读取不完整
- 未正确处理 Windows 命令行的编码

**修复位置**: `crates/wst-backend/src/lib.rs`

---

## Bug #3: 输出逐行延迟显示

**描述**: 命令输出一行一行慢慢显示，而不是一次性全部输出

**原因**: 事件循环每次只处理一个事件，导致输出分散

**修复位置**: `apps/wst-ui/src/main.rs` - `run_app()` 函数

**修复代码**:
```rust
// 循环处理所有可用事件，而不是只处理一个
loop {
    match rx.try_recv() {
        Ok(AppEvent::Backend(SessionEvent::Output(chunk))) => {
            // 处理输出
        }
        // ... 其他事件
        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
            break; // 没有更多事件才退出
        }
    }
}
```

---

## Bug #4: 提示符显示路径不完整

**描述**: 提示符只显示 `>` 而不是完整路径如 `C:\Users\Administrator\>`

**原因**: 未实现当前目录跟踪

**修复位置**: `apps/wst-ui/src/main.rs`

**修复内容**:
1. `AppState` 新增 `current_dir: String` 字段
2. 初始化时获取当前目录
3. 渲染时使用 `format!("{}>", state.current_dir)` 作为提示符

---

## Bug #5: Tab 对齐问题

**描述**: `dir` 输出中的 Tab 字符导致列对齐错乱

**原因**: 终端 Tab 显示为固定宽度，需要展开为空格

**修复位置**: `apps/wst-ui/src/main.rs` - `draw_ui()`

**修复代码**:
```rust
let expanded_text: String = line.text.replace('\t', "        ").trim_end().to_string();
```

---

## Bug #6: 回车符 `\r` 导致显示问题

**描述**: 输出中出现 `\r` 字符或异常换行

**原因**: Windows 使用 `\r\n` 行结束，后端读取时只去掉了 `\n`

**修复位置**: `crates/wst-backend/src/lib.rs`

**修复代码**:
```rust
let line = buf.trim_end().trim_end_matches('\r').to_string();
```

---

## Bug #7: 复制文本出现阶梯效果

**描述**: 从 Windows Terminal 复制输出后粘贴，文本呈阶梯状排列

**表现**:
```
第一行正常
  第二行缩进
    第三行更缩进
```

**根本原因**:
1. ratatui 的 `Paragraph` Widget 填充整个分配区域（包括空白单元格）
2. Windows Terminal 复制时将填充的背景色单元格当作空格
3. 每行长度不同 → 空格数不同 → 阶梯效果

**修复位置**: `apps/wst-ui/src/main.rs` - `draw_ui()`

**解决方案**: 绕过 Widget，直接操作终端 Buffer

**修复代码**:
```rust
// 直接操作 buffer，只写入实际字符
let buf = f.buffer_mut();

for line in state.output.iter() {
    let expanded_text = line.text.replace('\t', "        ").trim_end();
    let mut col = area.x;
    for ch in expanded_text.chars() {
        buf.get_mut(col, area.y + y_offset)
            .set_char(ch)
            .set_style(style.clone());
        col += 1;
    }
    y_offset += 1;
}
// 不填充行尾空白
```

**测试命令**:
```bash
:backend cmd
dir
# 复制输出粘贴到文本文件验证
```

---

## 修复统计

| Bug # | 问题 | 严重程度 | 状态 |
|-------|------|---------|------|
| 1 | Backend 切换无效 | 高 | ✅ 已修复 |
| 2 | dir 输出乱码 | 高 | ✅ 已修复 |
| 3 | 输出逐行延迟 | 中 | ✅ 已修复 |
| 4 | 提示符路径不完整 | 低 | ✅ 已修复 |
| 5 | Tab 对齐问题 | 中 | ✅ 已修复 |
| 6 | 回车符处理 | 低 | ✅ 已修复 |
| 7 | 复制阶梯效果 | 高 | ✅ 已修复 |

## 相关文件

- `apps/wst-ui/src/main.rs` - 主 UI 逻辑
- `crates/wst-backend/src/lib.rs` - 后端实现
- `docs/terminal-copy-fix.md` - Bug #7 详细分析
