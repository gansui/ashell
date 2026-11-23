# gpui 补丁说明（RDP 动态画面支持）

## 为什么需要这个补丁

ashell 的 RDP 画面是**动态图像**（每帧更新）。GPUI 原生的 sprite atlas
（位图集）对图像 tile 只增不减：

- `RenderImage::new()` 每次生成新的 `ImageId`
- atlas 以 `(ImageId, frame_index)` 为键分配 GPU 纹理区域，**从不清理旧区域**

结果：RDP 每刷新一帧就泄漏一块 1280×720×4 ≈ 3.7MB 的显存，
几分钟就会把显存耗尽（`failed to allocate`）。

## 补丁做了什么（共 4 个文件，均在 zed 仓库内）

1. `crates/gpui/src/assets.rs`
   - 新增 `RenderImage::with_id(id, data)`：用**已有 id** 重建图像
     （ashell 每帧复用同一 id，atlas 键保持不变）。

2. `crates/gpui/src/platform.rs`（共享 atlas 核心）
   - `AtlasBackend` trait 新增 `update_tile(&mut self, tile, bytes)`，
     默认空实现。
   - `AtlasState` 新增 `image_hashes` 哈希表，记录已上传 tile 的像素哈希。
   - `get_or_insert_with` 命中已有 tile 且键为 `AtlasKey::Image` 时：
     重新构建像素数据，哈希比较，**变了就在原 tile 上重新上传**，
     不变则什么都不做（静止画面零开销）。

3. `crates/gpui_windows/src/directx_atlas.rs`（Windows 后端）
4. `crates/gpui_wgpu/src/wgpu_atlas.rs`（Linux 后端）
   - 实现 `update_tile`：原地重传 tile 的 GPU 纹理。

哈希表只记录"已上传内容的哈希"，用于跳过未变化帧的 GPU 上传，
不改变任何既有语义（字形/SVG tile 走原路径，不受影响）。

## 重要：为什么不用 `git checkout` 旧提交

**补丁基于 zed 官方最新 `main` 分支（HEAD）制作。** 请不要 checkout 到
Cargo.lock 里锁定的旧提交——你之前遇到
`致命错误：无法读取树（969a67fc...）`，就是因为 `--filter=blob:none`
部分克隆下，旧提交的树对象不在可自动获取范围内，`git checkout` 会失败。

ashell 的 Cargo 依赖用的是**分支最新版**（不带 rev），所以：

- 你的 fork 只要保持 `main` 分支最新（= 官方 main + 本补丁），
- 补丁基于 `main` 制作，直接 `git apply` 即可，无需 checkout。

## 操作步骤（爸爸手工执行）

```bash
# 0) 在浏览器打开 https://github.com/zed-industries/zed 点 "Fork"
#    （选到自己账号下，得到 https://github.com/gansui/zed）

# 1) 访问 GitHub 走代理（按本机约定）
export HTTPS_PROXY=http://192.168.0.200:30000
export HTTP_PROXY=http://192.168.0.200:30000

# 2) 部分克隆 fork（--filter=blob:none 避免一次性下载全部文件）
#    保持默认分支 = main，不要 checkout 任何旧提交
git clone --filter=blob:none https://github.com/gansui/zed.git
cd zed

# 3) 应用补丁（把路径换成 ashell 仓库的实际位置）
git apply /lxc-5500T/1.2T/soft/ashell/gpui-patch/gpui-rdp.patch
git diff --stat     # 应显示 4 个文件被修改

# 4) 提交并推送到 fork 的 main 分支
git commit -am "gpui: re-upload dynamic image tiles in place (RDP live frames)"
git push origin HEAD:main

# 5) 把下面这个完整提交哈希告诉我
git rev-parse HEAD
```

## 补丁打完之后

告诉我第 5 步的提交哈希（完整 40 位），我会把 ashell `Cargo.toml` 里
三个 git 依赖（`gpui`、`gpui_platform`、`menu`）改为指向你的 fork：

```toml
gpui          = { git = "https://github.com/gansui/zed", rev = "<新哈希>" }
gpui_platform = { git = "https://github.com/gansui/zed", rev = "<新哈希>", features = [...] }
menu          = { git = "https://github.com/gansui/zed", rev = "<新哈希>" }
```

然后照旧上传 ashell 仓库，走 GitHub Actions 编译即可。

## 回滚方法

如果将来想回到官方 zed：把上面三个依赖改回
`https://github.com/zed-industries/zed`（不带 rev 或原 rev）即可，
ashell 侧的 `RenderImage::with_id` 调用会编译失败，
同时需要把 `src/app/mod.rs` 的 `rebuild_image` 改回
`RenderImage::new(vec![frame])`（但那样 RDP 画面会泄漏显存，不建议）。