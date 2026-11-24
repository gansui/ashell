# gpui 补丁说明（RDP 动态画面支持）

## 为什么需要这个补丁

ashell 的 RDP 画面是**动态图像**（每帧更新）。GPUI 原生的 sprite atlas
（位图集）对图像 tile 只增不减：

- `RenderImage::new()` 每次生成新的 `ImageId`
- atlas 以 `(ImageId, frame_index)` 为键分配 GPU 纹理区域，**从不清理旧区域**

结果：RDP 每刷新一帧就泄漏一块 1280×720×4 ≈ 3.7MB 的显存，
几分钟就会把显存耗尽（`failed to allocate`）。

## 关键背景：ashell 适配哪个 zed 版本？

**ashell 的全部代码适配 zed 提交 `969a67fcfbf799e68ab00854028228274be847e8`**
（旧版 gpui API，分散式 atlas：`WgpuAtlasState` 和 `DirectXAtlasState`
各自持有 `tiles_by_key` 与 `get_or_insert_with`）。

**补丁必须基于这个提交制作**，不能基于最新 main——最新 main 的 gpui
把 atlas 合并到了 `platform.rs` 的共享 `AtlasState<Backend>`，API 全变，
会导致 ashell 上千个编译错误。

## 补丁做了什么（3 个文件，均在 zed 仓库内）

1. `crates/gpui/src/assets.rs`
   - 新增 `RenderImage::with_id(id, data)`：用**已有 id** 重建图像
     （ashell 每帧复用同一 id，atlas 键保持不变）。

2. `crates/gpui_wgpu/src/wgpu_atlas.rs`（Linux 后端）
3. `crates/gpui_windows/src/directx_atlas.rs`（Windows 后端）
   - 各自新增 `image_hashes` 哈希表（记录已上传 tile 的像素哈希）。
   - `get_or_insert_with` 命中已有 tile 且键为 `AtlasKey::Image` 时：
     重新构建像素数据，哈希比较，**变了就在原 tile 上重新上传**，
     不变则什么都不做（静止画面零开销）。
   - `remove` / 清空 / 设备丢失时同步清理哈希表。

哈希表只记录"已上传内容的哈希"，用于跳过未变化帧的 GPU 上传，
不改变任何既有语义（字形/SVG tile 走原路径，不受影响）。

## 为什么不用 `git checkout` 之前也失败过？

你之前 `git checkout 969a67fc` 报 `致命错误：无法读取树`，是因为
`--filter=blob:none` 部分克隆下，该旧提交的 tree 对象不在可自动获取
范围内。现在你的 fork（gansui/zed）main 是完整历史，执行
`git checkout 969a67fc` 前先 `git fetch origin` 完整拉取即可。

## 操作步骤（爸爸手工执行）

```bash
# 1) 访问 GitHub 走代理（按本机约定）
export HTTPS_PROXY=http://192.168.0.200:30000
export HTTP_PROXY=http://192.168.0.200:30000

# 2) 在你的 fork 里完整拉取历史，然后切到 ashell 适配的旧提交
cd /lxc-5500T/1.2T/soft/ashell/zed
git fetch origin
git checkout 969a67fcfbf799e68ab00854028228274be847e8

# 3) 应用补丁（基于 969a67fc）
git apply /lxc-5500T/1.2T/soft/ashell/gpui-patch/gpui-rdp.patch
git diff --stat     # 应显示 3 个文件被修改

# 4) 提交并推送（新建分支或直接强推 main 均可）
git commit -am "gpui: re-upload dynamic image tiles in place (RDP live frames)"
git push origin HEAD:main

# 5) 把下面这个完整提交哈希告诉我
git rev-parse HEAD
```

## 补丁打完之后

告诉我第 5 步的提交哈希（完整 40 位），我会把 ashell `Cargo.toml` 和
`Cargo.lock` 里的 git 依赖改为指向你的 fork + 新哈希：

```toml
gpui          = { git = "https://github.com/gansui/zed", rev = "<新哈希>" }
gpui_platform = { git = "https://github.com/gansui/zed", rev = "<新哈希>", features = ["font-kit", "runtime_shaders", "wayland", "x11"] }
menu          = { git = "https://github.com/gansui/zed", rev = "<新哈希>" }
```

然后照旧上传 ashell 仓库，走 GitHub Actions 编译即可。

## 回滚方法

如果将来想回到官方 zed：把上面三个依赖改回
`https://github.com/zed-industries/zed`（不带 rev 或原 rev）即可，
ashell 侧的 `RenderImage::with_id` 调用会编译失败，
同时需要把 `src/app/mod.rs` 的 `rebuild_image` 改回
`RenderImage::new(vec![frame])`（但那样 RDP 画面会泄漏显存，不建议）。