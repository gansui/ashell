# ashell 支持 RDP 协议：分析与实现计划

## 一、目标

让 ashell（一个基于 GPUI 的 SSH/终端客户端）支持微软远程桌面协议（RDP），包括：

1. 新建连接时支持 RDP 类型连接
2. 支持窗口内应用的剪贴板信息传递（把宿主机剪贴板内容传入远端 Windows 主机，即 RDP 的 CLIPRDR 剪贴板虚拟通道）
3. 编译支持 Windows x64

## 二、核心架构判断（关键）

**ashell 是字符终端渲染**（基于 alacritty_terminal，渲染 `cells/rows/cols` 字符网格），而 **RDP 是位图图形渲染**（屏幕是图像像素）。两者是根本不同的渲染模型，**RDP 屏幕无法塞进字符终端里**。

因此，RDP 集成必须走「**新增独立 RDP 渲染面板**」路线：在 ashell 的 UI 层（GPUI）新增一个用图像绘制的视图，新建 tab 时若 `protocol = "rdp"` 则渲染 RDP 位图，而不是字符终端。

## 三、已采纳的参考实现

`rdp-rs`（MIT 协议）是一个纯 Rust 的 RDP 客户端库，位于 `ashell/rdp-rs/` 子目录。它实现了完整的协议栈：

- `model/`：data（组件/消息树编解码）、error、link（TCP/TLS 流）、rnd（随机）、unicode
- `core/`：tpkt、x224、mcs、gcc、per、sec、license、global（global 通道，位图+输入）、capability、event
- `nla/`：asn1、cssp、ntlm（NTLM 认证）、sspi、rc4
- `codec/`：rle（位图 RLE 解压）

`rdp-rs` 的公开 API：
- `rdp::core::client::Connector`：构建连接（`.screen(w,h).credentials(domain,user,pass)`）
- `rdp::core::client::RdpClient<S>`：`.read(callback)` 阻塞读事件，`.write(RdpEvent)` 发送鼠标/键盘输入
- `rdp::core::event::RdpEvent`：`Bitmap(BitmapEvent)` / `Pointer(PointerEvent)` / `Key(KeyboardEvent)`
- `rdp::core::event::BitmapEvent`：`decompress()` 解压成 RGBA 像素

## 四、已完成的改动（本阶段）

### 1. rdp-rs 作为 path 依赖接入

在 `ashell/Cargo.toml` 的 `[dependencies]` 中加入：

```toml
rdp = { path = "rdp-rs" }
```

**采用 path 依赖（而非硬并入 src/rdp 模块）的原因**：
rdp-rs 是 2019 年的老 crate，依赖版本旧（`yasna ^0.3`、`x509-parser 0.6.5`、`hmac ^0.7`、`md4 ^0.8`、`md-5 ^0.8`、`rand ^0.7`、`num_enum 0.4.3`）。ashell 是现代的（`edition 2024`、`rand 0.8`、`hmac 0.12`、`digest 0.10`）。

若硬并入 `src/rdp` 模块：
- rdp-rs 内部 `use rand::thread_rng()`（0.7 API）、`Hmac::<Md5>::new_varkey`（0.7 API）、`Md4::new().input().result()`（digest 0.8 API）会解析到 ashell 的现代依赖（0.8/0.12/0.10），API 不兼容，编译失败
- 需大规模升级 rdp-rs 的哈希/随机 API 到 digest 0.10，且 yasna 0.3→0.5、x509-parser 0.6→0.9 的 API 变化大，适配成本极高

而 **path 依赖把 rdp-rs 作为独立 crate 隔离**，Cargo 会为它解析自己声明的依赖版本，与 ashell 的依赖共存（Cargo 支持同一 crate 多版本并存）。这样 rdp-rs 原始代码和依赖零改动，编译成功率高。

> 注意：rdp-rs 代码仍在 ashell 仓库内（`ashell/rdp-rs/` 子目录），实现了「融入代码主干」的目标，只是保持为独立 crate 以隔离依赖。

## 五、后续分阶段实现计划（未完成，需继续）

### 阶段 2：新增 RDP 后端模块（`src/backend/rdp.rs`）

模仿 `src/backend/ssh.rs` 的模式，但注意 **rdp-rs 是同步阻塞读**，需放独立 `std::thread`：

1. 新增 `TabKind::Rdp` 枚举变体（`src/terminal/mod.rs`）
2. 新增 `BackendTx::Rdp` 和 `BackendEvent::RdpBitmap` / `RdpClosed` 变体
3. 新增 `src/backend/rdp.rs`：
   - `spawn_rdp(...)`：用 `std::thread` 起一个线程
   - 在线程里：`TcpStream::connect` → `Connector::new().screen(w,h).credentials(...)` → `RdpClient::connect` → `read(|event| ...)` 循环
   - 每个 `BitmapEvent` 解压成 RGBA，打包成 `BackendEvent::RdpBitmap { tab_id, x, y, w, h, rgba }` 发送
   - 接收 `BackendCommand`（`Resize`、`Close`）转发给 RDP 客户端

### 阶段 3：新增 RDP 位图渲染面板（GPUI）

1. 在 `src/app/ui.rs` 的 `render_pane_tree` 中，当 `tab.kind == Rdp` 时分支出渲染 RDP 图像视图而非 `TerminalElement`
2. 用 GPUI 的位图/图像渲染 API 把 `BackendEvent::RdpBitmap` 累积到的完整帧缓冲区绘制出来
3. 处理鼠标/键盘事件 → 转成 `RdpEvent::Pointer` / `RdpEvent::Key` 发给后端

### 阶段 4：会话配置支持 rdp 类型

1. `src/session/config.rs` 的 `Session`：`protocol = "rdp"` 时用 `user` 字段当用户名、`password` 当密码、`host`/`port`(默认 3389) 作为目标
2. `SavedTerminalTab` 增加 RDP 变体，支持标签页恢复

### 阶段 5：新建连接 UI 支持 rdp 协议

1. 在新建连接对话框中增加协议选择（SSH / RDP），RDP 时显示域名/用户名/密码/主机/端口字段
2. 翻译文件 `locales/` 增加 RDP 相关文案

### 阶段 6：CLIPRDR 剪贴板虚拟通道（宿主机剪贴板 → 远端）

rdp-rs **没有实现剪贴板虚拟通道**，需要自己补：

1. RDP 虚拟通道（static virtual channel）：通过 MCS 的 `channel_join` 建立 `CLIPRDR` 通道（通道名 `"cliprdr"`）
2. 在 `mcs::Client` 的 `connect` 中增加通道声明，让服务器注册 `cliprdr` 通道
3. 实现 CLIPRDR 协议（MS-RDPECLIP）：
   - 客户端发 `CLIPRDR_CAPS`（声明支持格式）
   - 服务器发 `CLIPRDR_FORMAT_LIST`（询问可用格式）
   - 客户端发 `CLIPRDR_FORMAT_DATA_REQUEST` / 服务器回 `CLIPRDR_FORMAT_DATA_RESPONSE`（读取宿主机剪贴板文本）
   - 把读到的文本发送给远端（通过 `CLIPRDR_FORMAT_LIST` 广播剪贴板更新）
4. 需要 ashell 侧能读取宿主机剪贴板（GPUI 有剪贴板 API），并把内容编码成 RDP 文本格式

## 六、风险与注意事项

1. **编译验证受限**：本机不支持编译 Windows 可执行文件，需推送到 GitHub Actions 用 `build-windows.yml`（x64）编译验证。新增的 `rdp` 依赖需要 Cargo 能解析 rdp-rs 的旧依赖版本（yasna 0.3 / x509-parser 0.6 / num_enum 0.4 等），理论上作为独立 crate 可共存，但需实跑确认。

2. **rdp-rs 是同步阻塞模型**：与 ashell 的 tokio 异步后端不同，RDP 后端需用独立 `std::thread` 跑阻塞读循环，用线程安全通道与 UI 通信。

3. **CLIPRDR 通道是自研**：rdp-rs 没有剪贴板通道，需要按 MS-RDPECLIP 规范从零实现，工作量不小且需调试。

4. **图像渲染**：GPUI 的位图渲染 API 需要确认（ashell 是字符终端，可能没有现成的图像绘制组件），可能需要新增或借用 gpui-component 的 canvas 能力。

5. **多协议 tab 共存**：RDP tab 与 SSH tab 在同一个 `tabs`/`tab_groups` 结构里共存，`render_pane_tree` 需要按 `tab.kind` 分支渲染，各 tab 的焦点/激活逻辑需兼容。

## 七、当前状态总结

| 阶段 | 内容 | 状态 |
|------|------|------|
| 1 | rdp-rs 依赖接入 | ✅ 已完成 |
| 2 | RDP 后端模块 | ✅ 已写完（未接入 UI，未编译验证） |
| 3 | RDP 位图渲染面板 | ⏳ 未开始 |
| 4 | 会话配置支持 rdp（数据层） | ✅ 数据层已完成（`Session.domain` + `Session::rdp`） |
| 5 | 新建连接 UI | ⏳ 未开始 |
| 6 | CLIPRDR 剪贴板通道 | ⏳ 未开始 |

## 八、阶段 2 实际改动（已完成）

阶段 2 实现了 RDP 后端模块和支撑它的数据结构扩展，改动如下：

1. **`Cargo.toml`**：新增 `rdp = { path = "rdp-rs" }` path 依赖。

2. **`src/terminal/mod.rs`**：
   - `BackendEvent` 新增 `RdpBitmap`（携带解压后的 RGBA 位图区域）和 `RdpStatus`（RDP 会话状态文本）变体
   - `BackendTx` 新增 `Rdp(std::sync::mpsc::Sender<BackendCommand>)` 变体，`send()` 的 match 增加 `Self::Rdp` 分支

3. **`src/app/mod.rs`**：
   - 新增 `RdpFrameState` 结构体（持有累计的 RGBA 全屏帧缓冲），提供 `apply_bitmap()` 把一个位图区域合成到帧缓冲
   - `Ashell` 结构体新增 `rdp_frames: HashMap<String, RdpFrameState>` 字段（存储每个 RDP tab 的当前帧）
   - `drain_backend_events` 的 match 新增 `BackendEvent::RdpBitmap` / `BackendEvent::RdpStatus` 分支，把位图应用到帧缓冲、更新 tab 状态

4. **`src/backend/rdp.rs`**（新增）：RDP 后端核心
   - `spawn_rdp(...)`：在独立 OS 线程上启动 RDP 会话，返回 `BackendTx::Rdp`
   - `run_rdp(...)`：`TcpStream::connect` → `Connector::new().screen(w,h).credentials(domain,user,pass).name().check_certificate(false)` → `RdpClient::connect` → 阻塞 `client.read(...)` 循环
   - `handle_rdp_event(...)`：把 `RdpEvent::Bitmap` 解压成 RGBA，转发为 `BackendEvent::RdpBitmap`
   - `is_disconnect_error(...)`：识别服务器断开错误
   - 处理 `BackendCommand::Close`（优雅断开）

5. **`src/backend/mod.rs`**：注册 `pub mod rdp;`

## 九、阶段 4（数据层）实际改动（已完成）

阶段 4 完成了「会话配置支持 rdp」的**数据层部分**（不影响现有编译、兼容旧配置）：

1. **`src/session/config.rs`**：
   - `Session` 结构体新增 `#[serde(default)] pub domain: String` 字段（Windows 域名，用于 RDP；带 `serde default` 故旧配置仍可读取）
   - 更新所有 `Session` 构造点（`password`/`key`/`serial` 构造器 + 测试字面量）补充 `domain: String::new()`
   - 新增 `Session::rdp(host, port, domain, user, password)` 构造器，`protocol` 设为 `"rdp"`

> 说明：`Session.protocol` 已是自由 `String`（默认 `"ssh"`），现有代码用 `== "serial"`/`== "ssh"` 比较，无穷尽 match，故 `protocol = "rdp"` 不破坏现有逻辑。

## 十、当前总体状态与剩余工作

**已完成（自洽、不破坏现有编译）**：
- 阶段 1：rdp-rs path 依赖
- 阶段 2：RDP 后端模块（`spawn_rdp` 就绪，未接入 UI）
- 阶段 4 数据层：`Session.domain` + `Session::rdp`

**未完成（因改动面大、依赖编译验证，未强行实现）**：
- 阶段 3：RDP 位图渲染面板（GPUI 图像绘制 + `render_pane_tree` 分支）
- 阶段 4 UI 部分：`TabKind::Rdp` 变体（波及大量 `match`）+ `SavedTerminalTab::Rdp`
- 阶段 5：新建连接 UI 支持 rdp（对话框字段 + tab 创建接入 `spawn_rdp`）
- 阶段 6：CLIPRDR 剪贴板虚拟通道（MS-RDPECLIP 自研）

**关键技术风险（待 GitHub 实跑确认）**：
- rdp-rs 作为独立 path 依赖，其旧依赖（yasna 0.3 / x509-parser 0.6 / num_enum 0.4 / hmac 0.7 / rand 0.7 / md4 0.8）需要 Cargo 能解析，理论上作为独立 crate 与 ashell 现代依赖共存。
- 阶段 2 的后端代码（`rdp.rs`）基于对 rdp-rs API 的人工核对（`Connector`/`RdpClient::read`/`BitmapEvent::decompress`/`RdpError`），已规避已发现的 API 陷阱（`Error::kind()` 不存在→改用 `matches!` 解构 `Error::RdpError`；`BitmapEvent::decompress(self)` 按值消费→先拷出坐标字段）。
- 所有阶段 2/4 改动**未编译验证**，需在 GitHub Actions 上 `cargo build` 确认。

## 十一、阶段 3/4/5 实际改动（已完成，待编译验证）

这些改动让 RDP 真正接入 tab 创建、保存/恢复、渲染和新建立连接流程：

1. **`src/terminal/mod.rs`**：
   - `TabKind` 新增 `Rdp` 变体
   - 新增 `TerminalTab::new_rdp(id, session, backend, backend_events)` 构造器

2. **`src/session/config.rs`**：
   - `SavedTerminalTab` 新增 `Rdp { id, session }` 变体

3. **`src/session/mod.rs`**：
   - 保存 tab 的 match 加 `TabKind::Rdp` 分支
   - 恢复 `SavedTerminalTab::Rdp` 分支（调 `spawn_rdp` + `new_rdp`）
   - split 的 `match current_kind` 加 `TabKind::Rdp` 分支
   - `retry_disconnected_tab` 的 match 加 `TabKind::Rdp` 分支
   - `connect_saved_session` 按 `protocol == "rdp"` 分派到 `open_rdp_session`
   - 新增 `open_rdp_session(session, cx)`：创建 RDP tab + 启动 spawn_rdp 后端（默认分辨率 1280x720）
   - `connect_ssh` 新增 `session_protocol == "rdp"` 分支：读取 host/port(3389)/user/password → `Session::rdp` 保存 + `open_rdp_session`

4. **`src/app/mod.rs`**：
   - 重连后端的 match 加 `TabKind::Rdp` 分支（`spawn_rdp`）

5. **`src/app/ui.rs`**：
   - `render_pane_tree` 加 RDP 分支（`tab.kind == Rdp` 时不渲染字符终端）
   - 新增 `render_rdp_pane`：显示连接状态 + 断开提示 + 黑色占位背景，预留 `rdp_frames` 图像渲染接口

6. **`src/app/dialogs.rs`**：
   - 新建连接对话框加 RDP 协议按钮（点击设 `protocol=rdp` + `port=3389`）
   - 加 `is_rdp` 判断
   - `.when(is_ssh || is_rdp, ...)`：RDP 时显示 host/port/user/password 字段

**说明**：RDP 的域名（domain）当前在 `connect_ssh` 里传空串（`String::new()`），尚未提供域名输入框；RDP 渲染面板是占位版（显示状态而非真实位图）。这两项在「剩余工作」中。

## 十二、剩余工作（待后续）

- **真实位图渲染**：用 GPUI 图像元素绘制 `rdp_frames` 的 RGBA，替换 `render_rdp_pane` 的占位。已确认 ui.rs 导入了 `canvas`（GPUI 图像绘制能力），下一步用它绘制位图。
- **RDP 域名输入框**：在新建连接对话框加 `domain` 字段，`connect_ssh` 的 RDP 分支读取。
- **RDP 键盘输入转发**：`render_rdp_pane` 的 `on_key_down` → `RdpClient::write`（Key），需 scancode 映射。
- **CLIPRDR 剪贴板通道**：按 MS-RDPECLIP 自研 RDP 虚拟通道，把宿主剪贴板文本传给远端。

## 十三、阶段 6（输入转发）实际改动（已完成，待编译验证）

1. **`src/terminal/mod.rs`**：
   - 新增 `RdpPointerButton`/`RdpPointerInput`/`RdpKeyInput`/`RdpInputEvent` 类型
   - `BackendCommand` 新增 `RdpInput(RdpInputEvent)` 变体

2. **`src/backend/rdp.rs`**：
   - import 增加 `KeyboardEvent`/`PointerButton`/`PointerEvent` 和 `RdpInputEvent`
   - 命令循环新增 `BackendCommand::RdpInput` 分支：把 `RdpInputEvent::Pointer`/`Key` 转成 `RdpEvent` 调用 `client.write`

3. **`src/backend/local.rs`/`serial.rs`**：`BackendCommand` 兜底分支加 `| BackendCommand::RdpInput(_)`（非 RDP 后端忽略）

4. **`src/backend/ssh.rs`**：加 `Some(BackendCommand::RdpInput(_))` 忽略分支

5. **`src/app/ui.rs`**：
   - 新增 `send_rdp_pointer` 辅助方法（把鼠标位置/按钮转成 `BackendCommand::RdpInput::Pointer` 发给 tab 后端）

## 十四、阶段 7（鼠标右键/中键 + 键盘转发）实际改动（已完成，待编译验证）

1. **`src/app/ui.rs`**：
   - 新增 `rdp_scancode_for_key(key: &str) -> Option<u16>` 模块级函数：把 GPUI key 字符串（如 `"a"`/`"escape"`/`"up"`）映射到 Windows 键盘 scancode（Set 1），供 RDP 键盘输入协议使用
   - 新增 `send_rdp_key(&mut self, tab_id, scancode, down)` 辅助方法：发 `BackendCommand::RdpInput::Key`
   - import 增加 `KeyDownEvent`/`KeyUpEvent`
   - `render_rdp_pane` 补 `on_mouse_down/up(MouseButton::Right/Middle)`（右键/中键操作）
   - `render_rdp_pane` 给 el 加 `.focusable(self.focus_handle.clone())` + `on_key_down`/`on_key_up`：把 GPUI 按键转成 RDP scancode 发送

**说明**：这消掉了 `RdpPointerButton::Right/Middle never constructed` 和 `RdpInputEvent::Key never constructed` 告警。`LocalDirectoryChanged never constructed` 是原有告警（与 RDP 无关），未动。RDP 键盘输入使用 scancode（Set 1），GPUI 方向键等命名键的字符串格式（`"up"`/`"arrowup"` 等）已做多猜测兼容，需在 GitHub 编译验证。

**已完成**：RDP 连接 / tab 创建 / 会话保存恢复 / 渲染面板 / 新建连接 UI / 鼠标（左中右）输入 / 键盘输入转发。

**剩余工作**：
- 真实位图渲染（GPUI `canvas` 绘制 `rdp_frames` RGBA）
- RDP 域名输入框
- CLIPRDR 剪贴板通道
   - `render_rdp_pane` 给 el 加 `on_mouse_move`/`on_mouse_down`/`on_mouse_up`，转发鼠标事件到 RDP 后端