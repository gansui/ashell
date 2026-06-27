# Change Log

## 1. 修复 less 搜索高亮文字不可见问题

### 问题描述
在 ashell 中通过 SSH 连接 Linux 服务器后，使用 `less` 查看文件并执行搜索（如搜索 "ASR"），匹配到的文字与背景色相同，无法看见。在其他 SSH 终端软件中无此问题。

### 根因分析
`less` 的搜索高亮使用 ANSI 反显模式（`\e[7m`），alacritty_terminal 会给匹配的单元格设置 `INVERSE` 标志。

渲染流程中的问题：
1. `element.rs` 的 `layout_grid` 方法中，背景矩形（`LayoutRect`）的绘制条件为 `selected || !is_default_bg(cell.bg)`
2. `less` 反显时，单元格的 `cell.bg` 仍然是默认背景色（`NamedColor::Background`），因此 `is_default_bg()` 返回 `true`，背景矩形被跳过不绘制
3. 同时 `cell_run_style` 方法中，`INVERSE` 标志交换了前景/背景色，文本颜色变成了主题背景色（深色）
4. 结果：深色文字在深色终端背景上完全不可见

### 修复方案
**文件**: `src/terminal/element.rs:378`

在背景矩形绘制条件中增加 `INVERSE` 标志检查：

```rust
// 修改前
if selected || !is_default_bg(cell.bg) {

// 修改后
if selected || !is_default_bg(cell.bg) || cell.flags.contains(Flags::INVERSE) {
```

这样当 `INVERSE` 标志存在时，也会绘制背景矩形（使用原始前景色），使反显文字正常显示。

---

## 2. 新增括号粘贴模式（Bracketed Paste Mode）支持

### 问题描述
在 ashell 中粘贴多行文本到 TUI 程序（如 opencode cli）时，程序把每个换行符都当作 Enter 键执行，导致每一行都被当作独立命令执行。而其他 SSH 终端软件无此问题。

### 根因分析
ashell 的 `paste_text` 方法在发送粘贴内容时，没有使用括号粘贴模式。该模式是现代终端的一项功能，启用后终端会在发送粘贴文本前后各加上一组特殊控制字符（`\x1b[200~` 和 `\x1b[201~`），让 TUI 程序能区分粘贴内容和手动输入。

没有开启此模式时，TUI 程序无法区分哪些是粘贴的、哪些是手动输入的，会把粘贴内容中的换行符当作 Enter 键处理。

### 修复方案
1. 在 `src/session/config.rs` 的 `ConfigFile` 中新增 `bracketed_paste: bool` 配置字段，默认值为 `true`
2. 在 `ConfigStore` 中添加对应的 getter/setter 方法
3. 修改 `src/terminal/mod.rs` 的 `paste_text` 方法，根据配置决定是否在粘贴内容前后包裹 `\x1b[200~` ... `\x1b[201~` 转义序列
4. 在设置 UI（`src/app/dialogs.rs`）中添加开关，参考 `right_click_copy_paste` 的实现方式
