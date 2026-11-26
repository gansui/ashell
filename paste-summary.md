# RDP 剪贴板双向复制粘贴（copy & paste）排查总结与最终方案


本文记录 ashell 通过自研纯 Rust 客户端 `rdp-rs` 连接 Windows RDP 服务器时，
实现**本机（A）↔ 远程（B）双向剪贴板文本复制粘贴**的完整排查过程、每个失败方向、
最终根因与修复。结论均来自线缆字节（`rdp_clip_debug.log`）与权威实现
（FreeRDP、IronRDP、MS-RDPECLIP/MS-RDPBCGR）交叉验证。

---

## 一、结论先行

双向剪贴板最终卡在 **6 个相互独立的问题**上，从底层到上层依次是：

| 层 | 问题 | 位置 | 症状 |
|----|------|------|------|
| 连接 | 静态通道名未填充到固定 8 字节 | `rdp-rs/src/core/gcc.rs:280` | 服务器直接 RST，连接失败 |
| 封装 | 发送静态通道数据未加 `CHANNEL_PDU_HEADER` | `rdp-rs/src/core/client.rs:104` | 服务器把我们数据前 8 字节当头剥走，PDU 全错 |
| 封装 | 头里的 `CHANNEL_FLAG_SHOW_PROTOCOL(0x10)` 缺失（决定性） | `rdp-rs/src/core/client.rs:120` | 服务器**丢弃所有上行** CLIPRDR PDU，下行正常 |
| 时序 | 客户端错误地主动发 `CB_MONITOR_READY` / 初始化 PDU | `src/backend/rdp.rs` | 打乱服务器剪贴板状态机 |
| 编码 | `CB_FORMAT_LIST` 带了不该有的 `cFormats/pad` 头 | `src/backend/rdp.rs:515` | rdpclip 误解析，不接管 `CF_UNICODETEXT` |
| 编解码 | `CB_FORMAT_DATA_RESPONSE` body 多了/少了 4 字节 `u32` 长度 | `src/backend/rdp.rs:545` `:557` | B→A 文本截断、A→B 文本乱码 |

其中 **`CHANNEL_FLAG_SHOW_PROTOCOL` 缺失** 是最隐蔽、最关键的一环：
它完美解释了"**能显示远程桌面、鼠标能用（走 global 通道）、下行剪贴板数据能收，
但所有上行剪贴板数据服务器一律无视**"的现象。

---

## 二、最终数据流

```
A（本机）                                              B（远程 Windows）
────────────────────────────────────────────────────────────────────────
用户 Ctrl+C / 点 RDP 窗口
   │
   ├─ UI 读系统剪贴板  src/app/ui.rs:667 (on_key_down) / :5641 (on_mouse_down)
   │      → BackendCommand::RdpSetClipboard(text)
   │
   ├─ 后端缓存 clipboard_text，仅在内容变化时
   │      发 CB_FORMAT_LIST（宣布 CF_UNICODETEXT）      src/backend/rdp.rs:184
   │      ↓（MCS 静态通道 cliprdr，带 CHANNEL_PDU_HEADER）
   │                                            ───────────────►  rdpclip
   │                                          收到 → 回 CB_FORMAT_LIST_RESPONSE
   │                                          粘贴时 → 回 CB_FORMAT_DATA_REQUEST(13)
   │      ◄─────────────────────────────────────────────────────
   ├─ 收到 CB_FORMAT_DATA_REQUEST
   │      → 直接同步读 OS 剪贴板 read_clipboard_text()
   │        （兜底用缓存）                              src/backend/rdp.rs:291
   │      → 回 CB_FORMAT_DATA_RESPONSE（裸 UTF-16LE）   src/backend/rdp.rs:557
   │                                            ───────────────►  rdpclip
   │
B 内部复制文本
   │      ◄──────────── CB_FORMAT_LIST ──────────────────────────
   ├─ 回 CB_FORMAT_LIST_RESPONSE + CB_FORMAT_DATA_REQUEST(13)
   │      ───────────────────────────────────────────────►
   │      ◄──────────── CB_FORMAT_DATA_RESPONSE（裸 UTF-16LE）──
   └─ 解码（从第 8 字节起）→ BackendEvent::RemoteClipboard
          → 写入 A 系统剪贴板                      src/app/mod.rs:1815
```

---

## 三、逐层问题与修复

### 第 0 层：静态通道（cliprdr）声明

- **通道选项**：`rdp-rs/src/core/mcs.rs:194`
  ```rust
  channel_def(&"cliprdr".to_string(), 0xC0A00000)
  ```
  与 FreeRDP cliprdr 客户端一致：
  `CHANNEL_OPTION_INITIALIZED | ENCRYPT_RDP | COMPRESS_RDP | SHOW_PROTOCOL`。
  注意这里声明了 `SHOW_PROTOCOL`，直接引出第 2 层的坑（声明了却发送不带标志）。

- **通道名必须填充到 8 字节**：`rdp-rs/src/core/gcc.rs:280` `channel_def`
  ```rust
  let mut name_bytes = name.as_bytes().to_vec();
  if name_bytes.len() < 8 { name_bytes.resize(8, 0); }
  ```
  `CHANNEL_DEF.name` 是**定长 8 字节** `UCHAR[8]`（MS-RDPBCGR 2.2.1.3.4.1）。
  `"cliprdr"` 是 7 字节，若不补 NUL 到 8 字节，服务器解析 `ConnectInitial` 越界，
  直接 RST —— 早期"连不上/ConnectionReset"就栽在这里。

### 第 1 层：MCS 通道映射

日志确认通道 join 与 id 映射正确（无需改动，仅记录供排查）：
```
MCS: join channel 1003 -> true   ← global
MCS: join channel 1006 -> true   ← user (RDPSND/输入等)
MCS: join channel 1004 -> true   ← cliprdr（服务器分配的静态通道 id）
channel_ids={"global":1003,"user":1006,"cliprdr":1004}
```
`cliprdr` 的 id 取服务器 `channelIdArray` 的**第一个**（`mcs.rs:245`）。
`user` 通道用发起者 id + 1001，二者不会撞号。

### 第 2 层：CHANNEL_PDU_HEADER 封装（MS-RDPBCGR 2.2.6.1）

静态虚拟通道的数据在 MCS 之上必须用 **8 字节 `CHANNEL_PDU_HEADER`** 包裹：
- `Length`：`u32` 小端，= 载荷长度（**不含**本头）
- `Flags`：`u32` 小端，单段 = `CHANNEL_FLAG_FIRST(0x01) | CHANNEL_FLAG_LAST(0x02)`，
  再加上 `CHANNEL_FLAG_SHOW_PROTOCOL(0x10)`

**读取端**：服务器发来的数据**带头**，接收时必须剥掉 8 字节
（`rdp-rs/src/core/client.rs` 读路径 `buf.drain(..8)`），否则 msgType 判断全错。

**发送端**：最初 `write_clipboard` **直接发裸 PDU 不带头**，导致服务器把我们的
CLIPRDR_HEADER 前 8 字节当成 `CHANNEL_PDU_HEADER` 剥走，从错误偏移解析 msgType，
**整个 PDU 作废**。修复见 `rdp-rs/src/core/client.rs:104`：
```rust
let mut pdu = Vec::with_capacity(data.len() + 8);
pdu.extend_from_slice(&(data.len() as u32).to_le_bytes()); // Length = 载荷长度
pdu.extend_from_slice(&0x13u32.to_le_bytes());             // Flags = FIRST|LAST|SHOW_PROTOCOL
pdu.extend_from_slice(&data);
self.mcs.write(&"cliprdr".to_string(), pdu)
```

### 第 3 层：CHANNEL_FLAG_SHOW_PROTOCOL（决定性根因）

`Flags` 最初写成 `0x03`（只有 FIRST|LAST）。但通道在 `mcs.rs:194` 声明了
`CHANNEL_OPTION_SHOW_PROTOCOL(0x00200000)`。FreeRDP 的规则（`channels.c` / `peer.c`）：

> 当客户端在通道选项里声明了 `SHOW_PROTOCOL`，则**每个发送的 chunk**
> 的 `CHANNEL_PDU_HEADER.Flags` 都必须带上 `CHANNEL_FLAG_SHOW_PROTOCOL(0x10)`。

服务器（ServerMode，不设该位）因此有理由**丢弃所有缺少该标志的上行 PDU**。
这解释了此前所有"编码看起来完全正确却零响应"的假象：

- 服务器对 `CB_FORMAT_LIST` **零响应**（不是我们的字节错，是服务器根本没收到/接受）
- 下行一切正常（服务器发的头是 `03 00 00 00` 只有 FIRST|LAST，我们照收）
- 鼠标正常（走 global 通道，不是静态通道，不带这个标志）

把 `Flags` 从 `0x03` 改为 **`0x13`** 后，服务器**立刻开始应答**：
```
recv 8 bytes:  03 00 01 00 ...   ← CB_FORMAT_LIST_RESPONSE, flags=OK(0x01)
recv 12 bytes: 04 00 00 00 0d 00 00 00   ← CB_FORMAT_DATA_REQUEST(13)
```

### 第 4 层：CLIPRDR 初始化时序与 PDU 编码

**时序必须由服务器发起**（对照 IronRDP `lib.rs` / FreeRDP `wf_cliprdr.c`）：

1. 客户端**连接后不发任何 CLIPRDR PDU**（`CB_MONITOR_READY` 是**服务器专用**）。
2. 服务器发 `CB_CLIP_CAPS` + `CB_MONITOR_READY`。
3. 客户端收到 `CB_MONITOR_READY` 后，**一次性回三件套**：
   `CB_CLIP_CAPS` + `CB_TEMP_DIRECTORY` + `CB_FORMAT_LIST`
   （`src/backend/rdp.rs:331` monitor_ready 分支）。

> 失败方向（弯路）：曾让客户端在启动时主动发 `caps + monitor_ready`，甚至发
> `CB_MONITOR_READY`。`MONITOR_READY` 是服务器发起的信号，客户端抢发会打乱
> rdpclip 状态机。**客户端永远不发 `CB_MONITOR_READY`。**

各 PDU 的最终编码（全部已按线缆字节核对）：

- **`CB_CLIP_CAPS` (0x0007)** `rdp.rs:443`
  `msgFlags = 0`（**不带** `CB_RESPONSE(0x01)`）
  data = `cCapabilitiesSets=1(u16) + pad=0(u16) + [type=0x0001, len=12, version=2, flags=0x2]`
  - `generalFlags = 0x2 = CB_USE_LONG_FORMAT_NAMES`（不是 0x10，0x10 是 CAN_LOCK_CLIPDATA）
  - 服务器声明 `0x3e`（含 0x2），双方都用长格式名

- **`CB_TEMP_DIRECTORY` (0x0006)** `rdp.rs:466`
  data = 520 字节 UTF-16LE 路径缓冲（本文用 `.cliprdr` + NUL），`msgFlags = 0`

- **`CB_FORMAT_LIST` (0x0002)** `rdp.rs:515`
  **裸条目序列，无 `cFormats/pad` 头**（FreeRDP `cliprdr_packet_format_list_new`、
  IronRDP `FormatList::encode` 都如此）。一项 = `formatId u32(13) + 空名 UTF-16 NUL(2 字节)`
  = 6 字节，`dataLen = 6`，`msgFlags = 0`。
  > 失败方向：早期多加了 MS-RDPECLIP 名义上的 `cFormats(u16)+pad(u16)` 头，
  > 导致 rdpclip 把 offset0 的 `01 00 00 00` 当成 formatId=1，永不接管
  > `CF_UNICODETEXT`，也就不发数据请求。

- **`CB_FORMAT_DATA_REQUEST` (0x0004)** `rdp.rs:535`
  data = `formatId u32(13)`，`msgFlags = 0`

### 第 5 层：请求/应答状态机

- **`CB_FORMAT_DATA_REQUEST` 必须应答，哪怕没数据**：`rdp.rs:286`
  服务器常在登录时就发一次请求（那时本机剪贴板可能为空）。如果**静默不回**，
  rdpclip 的"请求→应答"状态机会**卡死**，之后你填好剪贴板它**也不再发新请求**——
  这就是"复制了却毫无反应"的原因之一。
  - 有数据：`CB_FORMAT_DATA_RESPONSE`（OK）
  - 无数据：`CB_FORMAT_DATA_RESPONSE`（`CB_RESPONSE_FAIL(0x0002)`，空体，
    FreeRDP `cliprdr_send_error_response` 同款）

- **优先实时读 OS 剪贴板**：`rdp.rs:291`
  ```rust
  let text = read_clipboard_text().or_else(|| clipboard.clone());
  ```
  UI 推送（`RdpSetClipboard`）与服务器请求之间存在竞态，直接同步读 Windows 剪贴板
  （`OpenClipboard` → `GetClipboardData(CF_UNICODETEXT)`，`rdp.rs:395`）最稳，缓存只兜底。

- **只在内容变化时宣布 `CB_FORMAT_LIST`**：`rdp.rs:184`
  早期每按一键都重发相同 `CB_FORMAT_LIST`（20+ 次），rdpclip 视作"已知"而忽略；
  现在用 `last_announced` 去重，内容真正变化才发一次。

### 第 6 层：数据编解码格式（最后一环：乱码/截断）

`CB_FORMAT_DATA_RESPONSE` 对 `CF_UNICODETEXT` 的 body **就是裸 UTF-16LE 字符串
（NUL 结尾），紧接在 8 字节 CLIPRDR 头之后，没有任何 `u32` 长度前缀。**
线缆证据（服务器下行，`dataLen=12` 对应 `"aaaaa\0"`）：
```
05 00 01 00 | 0c 00 00 00 | 61 00 61 00 61 00 61 00 61 00 00 00
 header 8B    dataLen=12    └──── "aaaaa" 的 UTF-16LE + NUL ────┘
```
若 offset8-11 被当成 `u32` 长度，则 = `0x00610061` = 6357104，显然不可能是长度。

- **解码**（B→A）：从 **第 8 字节**起，去掉末尾 NUL，再按 `u16` 小端解码。
  `rdp.rs:545`
  ```rust
  let body = data.get(8..)?;          // 不是 get(12..)
  let bytes = body.strip_suffix(&[0,0]).unwrap_or(body);
  let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0],c[1]])).collect();
  String::from_utf16(&units).ok()
  ```
  > 失败方向：曾从 `get(12..)` 起（多跳了 4 字节 = 多丢 2 个字符），故 B→A 文本被截断。

- **编码**（A→B）：body 直接是 UTF-16LE + NUL，**不加 `u32` 长度**。`rdp.rs:557`
  ```rust
  let mut text_data = ...;                    // UTF-16LE
  text_data.extend_from_slice(&0u16.to_le_bytes());
  let mut msg = cliprdr_header(0x0005, 0x0001, text_data.len() as u32);
  msg.extend_from_slice(&text_data);          // 不再前置 u32 len
  ```
  > 失败方向：曾在 body 前加 `u32` 长度（如 `d2 00 00 00`），服务器把长度字节当首字符，A→B 乱码。

---

## 四、关键线缆字节对照（最终正确形态）

| PDU | 方向 | 关键字节 |
|-----|------|----------|
| CHANNEL_PDU_HEADER | 双向 | `length u32LE` + `flags u32LE=0x13`(S→C 为 `0x03`) |
| CB_CLIP_CAPS `07 00 00 00` | C→S | flags=0, data=`01 00 00 00` + `01 00 0c 00` + `02 00 00 00` + `02 00 00 00` |
| CB_MONITOR_READY `01 00 ...` | 仅 S→C | 8 字节空 PDU（客户端**不发**） |
| CB_TEMP_DIRECTORY `06 00 00 00` | C→S | dataLen=520，UTF-16LE 路径+NUL |
| CB_FORMAT_LIST `02 00 00 00` | 双向 | flags=0, data=`0d 00 00 00` + `00 00`（6 字节，无 cFormats） |
| CB_FORMAT_LIST_RESPONSE `03 00 01 00` | 双向 | flags=OK(0x1) |
| CB_FORMAT_DATA_REQUEST `04 00 00 00` | 双向 | data=`0d 00 00 00`(formatId=13) |
| CB_FORMAT_DATA_RESPONSE `05 00 01 00` | 双向 | body=UTF-16LE+NUL，**无 u32 len** |
| CB_FORMAT_DATA_RESPONSE(FAIL) `05 00 02 00` | C→S | flags=0x2，空体 |

---

## 五、调试方法（可复用）

- 诊断日志宏 `clip_log!`：`rdp-rs/src/lib.rs:29`（`#[macro_export]`，在 ashell 内可
  `rdp::clip_log!`），把每行追加写到工作目录 `rdp_clip_debug.log`（**写文件**，
  因为 Windows GUI 启动时常丢弃 stderr / 继承环境变量）。
  设置 `RDP_CLIP_DEBUG=0` 可关闭。
- 所有出站/入站剪贴板 PDU 都打印**带 `CHANNEL_PDU_HEADER` 前后的完整字节**（`WIRE` 行），
  逐字节对照 FreeRDP / IronRDP 编码，是定位每一层问题的唯一可靠手段。
- **关键教训**：不要凭猜测改字节。每一次都先用 `rdp_clip_debug.log` 的线缆字节与
  权威实现（FreeRDP `/FreeRDP/`、IronRDP `/IronRDP/`）的编码函数逐字节对照，
  否则会在"看起来对但服务器不响应"里反复兜圈。

---

## 六、验证结果

- Linux `cargo check` 与 `x86_64-pc-windows-gnu` 交叉检查均通过。
- 实机连接 Windows（192.168.0.121）：
  - 服务器正常应答 `CB_FORMAT_LIST_RESPONSE` / `CB_FORMAT_DATA_REQUEST`
  - **本机 → 远程**、**远程 → 本机** 文本复制粘贴均正确，无乱码、无截断
  - 中文/长文本（magnet 链接等）正常传输

---

## 七、相关提交（gansui/ashell main）

按修复顺序（部分）：
`ad66bed` 通道名填充 8 字节 → 后续修复 PDU 编码 / 初始化时序 → `be29b80`
补 `CHANNEL_FLAG_SHOW_PROTOCOL` → `ac65d88` 状态机与去重/FAIL 应答 →
`c374090` `CB_FORMAT_DATA_RESPONSE` 去掉 `u32` 长度前缀（定稿双向正确）。
