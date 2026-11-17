# UFetch-Core

面向 **OpenWrt 插件自更新**（opkg / apk / LuCI 在线升级）的纯 HTTP 下载内核，Rust + Tokio。
小体积、无数据库、可续传，支持自适应多连接、慢段接管、镜像回退、限速与校验；
默认把完成包落在内存环形缓存里，提供任务列表、暂停/继续/取消/删除与安装后自动清理。

- crate：`ufetch-core`
- 库：`ufetch_core`
- 二进制：`ufetch`
- 许可证：LGPL-3.0-or-later

## 为什么不是"又一个下载器"

路由器上的插件升级有三个硬约束：存储小、网络差、必须能被打断后继续。
UFetch-Core 只围绕这三件事设计，不引入 BT / HLS / 数据库 / 插件系统：

- **按需切分**：连接数从 4 起步，按实测带宽逐秒爬升，失败一次退两档；
  服务器扛得住才加码，不会一上来就开满连接把路由器打爆。
- **互助抢速**：收尾阶段队列空了，空闲连接把最慢的在传分段对半拆开、接管后半截；
  `reserved` 位置保证拆分不会切进正在写的字节。
- **连接复用**：HTTP/1.1 + 连接池，分段之间复用同一条 TCP/TLS 连接，省掉重复握手。
- **断点续传**：`.parts.json` 侧车记录每段 `[start, pos, end)`，进程重启、路由器重启后继续；
  换镜像线路也能续。
- **镜像回退**：前缀（`https://ghfast.top/`）或 `{url}` 模板，只有网络类失败才换线路。
- **看门狗与慢速保护**：30 s 无新字节切断重派；32 KB/s × 15 s 判慢速失败，不挂死任务。
- **可被 LuCI 控制**：状态 JSON + 控制 JSON（`pause` / `resume` / `cancel`），无需 IPC。
- **内存环形缓存**：默认 `<tmp>/ufetch`（OpenWrt 上是 tmpfs），按总字节与条目数双上限淘汰最旧任务；
  `ufetch install <id>` 自动识别 OpenWrt 新版 `apk`（25.12+）与旧版 `opkg`（24.10 及更早），装完立即删除产物与任务记录；
  插件自己安装时可改用 `ufetch install-ok <id>` 只做清理。
- **任务表**：`tasks.json` 记录列表、进度、产物路径；`ufetch list` / `status` 可跨进程查看。
- **自更新收尾**：安装前写 `pending-install.json`；新包替换二进制或重启插件后，
  `ufetch install-ok`（可不带任务 ID）继续删除旧包与任务，重复调用幂等。
- **高级设置**：`settings.toml`（TOML + Serde），`ufetch config set key=value` 即时生效。
- **日志**：`<缓存目录>/ufetch.log`，512 KiB 环形保留最新内容，`ufetch log` 直接看尾部。

## 快速开始

```bash
cargo build --release
./target/release/ufetch https://example.com/pkg.apk -o /tmp/dl
```

多连接 + 镜像 + 校验 + 进度/控制：

```bash
./target/release/ufetch https://github.com/owner/repo/releases/download/v1/pkg.apk \
  -o /tmp/dl -n 0 --force-split \
  --mirror https://ghfast.top/ \
  --checksum sha256=<hex> \
  --status-file /tmp/dl/status.json \
  --control-file /tmp/dl/ctl.json
```

任务列表、暂停/继续、安装后清理：

```bash
ufetch list
ufetch pause <id>          # 或 resume / cancel / start
ufetch status <id>
ufetch install <id>        # 自动调用 apk/opkg 安装，成功后删除任务
ufetch install-ok <id>     # 插件自己安装完成后，仅做清理
ufetch install-ok          # 自更新替换二进制后，用待清理标记收尾
```

也可以直接写控制文件：

```bash
echo '{"cmd":"pause"}'  > /tmp/ufetch/<id>.ctl.json
echo '{"cmd":"resume"}' > /tmp/ufetch/<id>.ctl.json
echo '{"cmd":"cancel"}' > /tmp/ufetch/<id>.ctl.json
```

作为库使用：

```rust
use ufetch_core::{DownloadManager, DownloadRequest};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let manager = DownloadManager::new();
let outcome = manager
    .download_and_wait(
        DownloadRequest::new("https://example.com/pkg.apk", "/tmp/dl")
            .checksum("sha256=…")
            .mirror("https://ghfast.top/")
            .status_file("/tmp/dl/status.json"),
    )
    .await?;
println!("{} ({} bytes)", outcome.path.display(), outcome.total_bytes);
# Ok(()) }
```

## 构建

| 目标 | 产物 | 说明 |
| --- | --- | --- |
| 本机 | `target/release/ufetch` | 开发调试 |
| `x86_64-unknown-linux-musl` | 静态二进制 | 软路由 |
| `aarch64-unknown-linux-musl` | 静态二进制 | 主流 arm64 路由器，建议用 OpenWrt SDK 的 gcc |

release 模板默认 `opt-level="z"`、LTO、`panic=abort`、`strip`、`codegen-units=1`，
x86_64 静态产物约 2.7 MB。交叉编译细节见 [USAGE.md](USAGE.md)。

## 目录

```
src/            引擎、任务表、环形缓存、设置、续传、命名、限速、校验、日志
src/bin/        ufetch CLI
tests/          自建 HTTP 服务端的端到端测试
openwrt/        OpenWrt 包定义（预编译二进制 + SDK 打包）
USAGE.md        完整使用文档
```

## 文档

完整参数、库 API、JSON 字段、OpenWrt 打包与排错见 **[USAGE.md](USAGE.md)**。

## 许可证

LGPL-3.0-or-later（SPDX：`LGPL-3.0-or-later`）。文本见 [LICENSE](LICENSE) 与
[LICENSE.GPL-3](LICENSE.GPL-3)（LGPLv3 引用的 GPLv3 条款）。

作为库动态链接时按 LGPL 正常使用；若把 `ufetch` 静态链接进单一二进制分发，
请按 LGPLv3 第 4 节提供可重新链接的方式（目标文件或对应源码），并保留版权与许可证声明。
