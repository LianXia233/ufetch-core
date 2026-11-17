# UFetch-Core 使用文档

本文覆盖构建、CLI、库 API、JSON 文件、OpenWrt 集成与排错。
项目概览见 [README.md](README.md)。

## 1. 构建

要求 Rust 1.88+（edition 2024）。本机调试：

```bash
cargo build --release
./target/release/ufetch --help
```

静态 musl（软路由 / 路由器）：

```bash
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

### 1.1 OpenWrt SDK 交叉编译

树莓派/路由器常见的 `aarch64` 目标不要用宿主 gcc 链接，否则会报
`unrecognized option '--fix-cortex-a53-843419'`。用 SDK 自带的工具链：

```bash
export PATH="$SDK/staging_dir/toolchain-aarch64_cortex-a53_gcc-*/bin:$PATH"
export CC_aarch64_unknown_linux_musl=$(which aarch64-openwrt-linux-musl-gcc)
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=$CC_aarch64_unknown_linux_musl
cargo build --release --target aarch64-unknown-linux-musl
```

`ring` 的汇编由 `CC_<target>` 指定的编译器生成；只换 linker 不换 `CC`，会得到
x86-64 目标文件与 aarch64 启动文件混链，仍然失败。

也可以固化到 `.cargo/config.toml`：

```toml
[target.aarch64-unknown-linux-musl]
linker = "aarch64-openwrt-linux-musl-gcc"

[env]
CC_aarch64_unknown_linux_musl = "aarch64-openwrt-linux-musl-gcc"
```

常用目标：

| 设备 | target |
| --- | --- |
| 主流 arm64 路由器 | `aarch64-unknown-linux-musl` |
| 32 位 ARM（armv7） | `armv7-unknown-linux-musleabihf` |
| x86_64 软路由 | `x86_64-unknown-linux-musl` |

release 模板：`opt-level="z"`、LTO、`panic=abort`、`strip`、`codegen-units=1`；
x86_64 静态产物约 2.7 MB。

## 2. CLI

```text
ufetch [OPTIONS] <URL>
ufetch pause|resume|cancel --control-file <PATH>
```

### 2.1 参数

| 参数 | 默认 | 说明 |
| --- | --- | --- |
| `-o, --output-dir <DIR>` | 缓存目录 | 保存目录 |
| `-f, --file-name <NAME>` | 自动推断 | 指定文件名 |
| `-n, --connections <N>` | `0` | 并发连接数；`0` = 自适应 |
| `-l, --limit <RATE>` | 不限 | 全局限速，`512K` / `2M` |
| `-m, --mirror <PREFIX>` | — | 镜像前缀或 `{url}` 模板，可重复 |
| `--checksum <ALGO=HEX>` | — | `sha256` / `sha1` / `md5` / `sha512` |
| `--sha256 <HEX>` | — | 等价于 `--checksum sha256=<HEX>` |
| `--force-split` | 关 | 小文件也分段 |
| `--overwrite` | 关 | 覆盖同名文件 |
| `--status-file <PATH>` | `<缓存>/<id>.status.json` | 进度 JSON |
| `--control-file <PATH>` | `<缓存>/<id>.ctl.json` | 轮询控制 JSON |
| `--proxy <URL>` | — | `http://` / `socks5://` |
| `--ua <UA>` | Chrome 131 | User-Agent |
| `--stall-secs <N>` | `30` | 停滞看门狗 |
| `--max-retries <N>` | `3` | 单段重试次数 |
| `--max-connections <N>` | `8` | 自适应上限（硬上限 32） |
| `--no-slow-guard` | — | 关闭 32 KB/s × 15 s 慢速保护 |
| `--no-keep-partial` | 关 | 失败时删除半成品与侧车 |
| `--insecure` | 关 | 忽略证书错误（仅调试） |
| `--json` | 关 | 进度按 JSON 行输出 |
| `-q, --quiet` | 关 | 只在结束时输出路径 |

日志：`UFETCH_LOG=off|error|warn|info|debug`，默认 `warn`，写 stderr。

### 2.2 子命令

```bash
ufetch pause  --control-file /tmp/dl/ctl.json
ufetch resume --control-file /tmp/dl/ctl.json
ufetch cancel --control-file /tmp/dl/ctl.json
```

等价于向控制文件写 `{"cmd":"pause"}` 等。也可以按任务 ID 操作，控制文件由任务表自动给出：

```bash
ufetch pause <ID>
ufetch resume <ID>
ufetch cancel <ID>
ufetch start <ID>
```

### 2.3 任务、缓存与诊断

| 命令 | 作用 |
| --- | --- |
| `ufetch list [--json]` | 列出任务（ID、状态、百分比、大小、文件名） |
| `ufetch status <ID> [--json]` | 查看单个任务详情与产物路径 |
| `ufetch delete <ID> [--keep-file]` | 删除任务记录与产物；`--keep-file` 只删记录 |
| `ufetch install-ok <ID>` | 插件安装成功后调用：删除产物、侧车、状态/控制文件与任务 |
| `ufetch cache [list|clear]` | 查看或清空环形缓存 |
| `ufetch config [--json]` | 查看 TOML 设置 |
| `ufetch config set <K=V>...` | 修改设置并落盘 |
| `ufetch log [--tail <N>]` | 查看日志尾部（默认 40 行） |

## 3. 常用场景

### 3.1 插件包升级（多连接 + 进度 + 控制）

```bash
ufetch https://github.com/owner/repo/releases/download/v1/app.apk \
  -o /tmp/upgrade -f app.apk -n 0 --force-split \
  --status-file /tmp/upgrade/status.json \
  --control-file /tmp/upgrade/ctl.json \
  --checksum sha256=<hex> -q
```

`--force-split` 让 2 MiB 以下的插件包也分段；否则默认单流（小文件单流更快）。

### 3.2 镜像回退

前缀形式：

```bash
ufetch https://github.com/owner/repo/releases/download/v1/app.apk \
  -m https://ghfast.top/ -m https://ghproxy.net/
```

实际顺序：原始 URL → `https://ghfast.top/https://github.com/...` → 第二个镜像。

模板形式（`{url}` 会被替换为原始 URL）：

```bash
ufetch https://github.com/owner/repo/releases/download/v1/app.apk \
  -m 'https://ghfast.top/{url}'
```

只有 DNS / 连接 / TLS / 超时 / 重置 / 停滞这类网络失败才换线路；
403/429 会先降并发，仍失败才尝试下一路。

### 3.3 断点续传

同一保存目录、同一文件名再跑一次即可：

```bash
ufetch https://example.com/big.img -o /tmp/dl -f big.img
# 中断后
ufetch https://example.com/big.img -o /tmp/dl -f big.img
```

续传依赖两个文件：

- `big.img.ufetch`：临时文件（已下载内容）
- `big.img.ufetch.parts.json`：每段 `[start, pos, end)`

删除其中任意一个都会从头开始。失败时默认保留；要清理传 `--no-keep-partial`。

### 3.4 限速、代理与自签证书

```bash
ufetch https://example.com/pkg.apk -o /tmp/dl -l 512K
ufetch https://example.com/pkg.apk -o /tmp/dl --proxy socks5://127.0.0.1:1080
ufetch https://router.local/pkg.apk -o /tmp/dl --insecure
```

限速是全局令牌桶，所有连接共享；运行中要调整请用库 API `set_speed_limit`。

## 4. 作为库

```toml
[dependencies]
ufetch-core = { path = "…" }   # 或发布后的版本号
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust
use std::time::Duration;
use ufetch_core::{DownloadManager, DownloadRequest, EngineConfig, ManagerConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manager = DownloadManager::try_new(ManagerConfig {
        engine: EngineConfig::openwrt(),
        max_concurrent: 2,
        progress_interval: Duration::from_millis(500),
    })?;

    let task = manager.download(
        DownloadRequest::new("https://example.com/pkg.apk", "/tmp/dl")
            .file_name("pkg.apk")
            .connections(6)
            .mirror("https://ghfast.top/")
            .checksum("sha256=…")
            .status_file("/tmp/dl/status.json")
            .control_file("/tmp/dl/ctl.json"),
    );

    let mut progress = task.subscribe();
    while progress.changed().await.is_ok() {
        let snapshot = progress.borrow().clone();
        println!("{:.1}% {}/{}", snapshot.percent, snapshot.active_connections, snapshot.connections);
    }

    let outcome = task.wait().await?;
    println!("{} {} bytes", outcome.path.display(), outcome.total_bytes);
    Ok(())
}
```

### 4.1 `DownloadRequest` 构造器

`new(url, save_dir)` 之后可链式调用：

| 方法 | 作用 |
| --- | --- |
| `file_name` | 指定文件名 |
| `connections` | 覆盖自动连接数 |
| `mirror` / `mirrors` | 镜像前缀或模板 |
| `checksum` | `algo=hex` |
| `header` | 附加请求头 |
| `overwrite` | 覆盖已有文件 |
| `force_split` | 小文件也分段 |
| `status_file` / `control_file` | JSON 接口 |
| `keep_partial` | 失败/取消时是否保留侧车 |

### 4.2 任务控制与进度

- `task.id()`：任务 ID
- `task.control().pause()` / `.resume()` / `.cancel()`：进程内控制
- `task.run_state()`：当前状态
- `task.subscribe()`：`watch::Receiver<Progress>`，适合 UI 轮询
- `task.wait().await`：等完成，返回 `DownloadOutcome`

`DownloadOutcome`：`task_id`、`path`、`file_name`、`total_bytes`、`resumed_bytes`、
`connections`、`elapsed`、`checksum`、`average_bps`。

`manager.set_speed_limit(bps)` 可在运行中改全局限速。

### 4.3 `EngineConfig` 主要字段

| 字段 | 默认（`openwrt()`） |
| --- | --- |
| `connections` | `0`（自适应） |
| `max_connections` | `8` |
| `chunk_size` | `4 MiB` |
| `min_split_piece` | `64 KiB` |
| `max_retries` | `3` |
| `stall_timeout` | `30 s` |
| `connect_timeout` | `20 s` |
| `speed_limit_bps` | `0` |
| `slow_guard` | `32 KB/s × 15 s`，宽限 `10 s` |
| `user_agent` | Chrome 131 |
| `proxy` | `None` |
| `allow_invalid_certs` | `false` |

## 5. JSON 文件

### 5.1 进度文件

每 500 ms 原子写一次（tmp + rename）：

```json
{
  "task_id": "1794942750618-0",
  "state": "running",
  "downloaded": 1048576,
  "total": 4194304,
  "speed_bps": 734003,
  "connections": 6,
  "active_connections": 4,
  "segments_total": 12,
  "segments_done": 3,
  "percent": 25.0,
  "file_name": "pkg.apk",
  "elapsed_secs": 3
}
```

| 字段 | 含义 |
| --- | --- |
| `state` | `queued` / `probing` / `running` / `paused` / `verifying` / `completed` / `failed` / `cancelled` |
| `connections` | 当前目标并发 |
| `active_connections` | 当前在传连接数 |
| `percent` | 0–100；总大小未知时为 0 |
| `error` | 仅失败时出现 |
| `path` | 仅完成时出现，落盘绝对路径 |

### 5.2 控制文件

```json
{ "cmd": "pause" }
```

接受 `cmd` / `action` / `command` 三个键；值 `pause`、`resume`、`cancel`；
`stop`、`abort` 等同 `cancel`。命令处理后会清空文件，避免重复执行。

### 5.3 侧车文件

`<临时文件>.parts.json`：

```json
{
  "version": 2,
  "url": "https://example.com/pkg.apk",
  "etag": "\"abc\"",
  "last_modified": "Tue, 01 Jan 2026 00:00:00 GMT",
  "total": 4194304,
  "segments": [{ "start": 0, "pos": 1048576, "end": 2097152 }]
}
```

`pos` 是该段已写到的位置。URL 或 `total` 变化、ETag 变化、区间返回 416 时，
会清空重下一次。

## 6. 缓存、任务表与日志

### 6.1 内存环形缓存

默认目录：`${UFETCH_HOME:-/tmp/ufetch}`。OpenWrt 的 `/tmp` 是 tmpfs，文件本身就在内存里；
普通机器上则是临时目录。默认双上限：

| 项 | 默认 |
| --- | --- |
| 总字节 | 64 MiB |
| 条目数 | 16 |

每完成一个任务就登记一条；超过任一上限时删除最旧的产物。索引是 `<缓存>/cache.json`。
下载中的临时文件仍是 `<文件名>.ufetch` 与 `.parts.json`，都放在缓存目录里。

### 6.2 任务表

`<缓存>/tasks.json` 保存任务记录，字段包括：

```json
{
  "id": "1794946554892-0",
  "url": "https://example.com/pkg.apk",
  "file_name": "pkg.apk",
  "state": "completed",
  "downloaded": 4194304,
  "total": 4194304,
  "percent": 100.0,
  "speed_bps": 0,
  "connections": 6,
  "active_connections": 0,
  "created_at": 1794946554,
  "updated_at": 1794946559,
  "status_file": "/tmp/ufetch/<id>.status.json",
  "control_file": "/tmp/ufetch/<id>.ctl.json",
  "artifact": "/tmp/ufetch/pkg.apk",
  "auto_remove": true
}
```

- 下载进程在启动、每次进度 tick、终态时更新它；`ufetch list` 每次重新读盘。
- `install-ok` 在 `auto_remove=true` 时删除记录与产物；设为 `false` 则保留，便于排查。
- `delete` 是手动清理；`cancel` 只停任务，默认保留半成品便于续传。

### 6.3 日志

默认 `<缓存目录>/ufetch.log`，只保留最新 **512 KiB**：每次追加后若超限，就截掉旧内容，
保留尾部 512 KiB。级别由 `UFETCH_LOG` 控制：

```bash
UFETCH_LOG=debug ufetch https://example.com/pkg.apk
ufetch log --tail 80
UFETCH_LOG_FILE=/tmp/my.log ufetch ...   # 换日志路径
```

## 7. 高级设置（settings.toml）

配置文件：`<UFETCH_HOME>/settings.toml`，TOML + Serde。缺失时使用内置默认值；
`ufetch config` 打印当前值，`ufetch config set key=value` 修改并原子写回。

```toml
cache_dir = "/tmp/ufetch"
cache_capacity_mb = 64
cache_max_items = 16
max_concurrent = 3
connections = 0
max_connections = 8
chunk_mb = 4
speed_limit = "512K"
stall_secs = 30
max_retries = 3
connect_timeout_secs = 20
slow_guard = true
auto_remove = true
```

| 键 | 默认 | 说明 |
| --- | --- | --- |
| `cache_dir` | `<tmp>/ufetch` | 缓存、任务表、日志目录 |
| `cache_capacity_mb` | `64` | 环形缓存字节上限 |
| `cache_max_items` | `16` | 环形缓存条目上限 |
| `max_concurrent` | `3` | 同时下载任务数 |
| `connections` | `0` | 单任务连接数，0 = 自适应 |
| `max_connections` | `8` | 自适应上限 |
| `chunk_mb` | `4` | 分块大小 |
| `speed_limit` | 空 | `512K` / `2M`，空 = 不限 |
| `stall_secs` | `30` | 停滞看门狗 |
| `max_retries` | `3` | 单段重试 |
| `connect_timeout_secs` | `20` | 建连超时 |
| `slow_guard` | `true` | 32 KB/s × 15 s 慢速保护 |
| `auto_remove` | `true` | 安装成功后自动删除任务 |

命令行参数（如 `-n`、`-l`、`--stall-secs`）优先于配置文件。

## 8. OpenWrt 集成

`openwrt/Makefile` 采用“预编译二进制 + SDK 只打包”的方式：

```bash
# 1. 交叉编译
cargo build --release --target aarch64-unknown-linux-musl

# 2. 放进包目录
mkdir -p openwrt/root/usr/bin
cp target/aarch64-unknown-linux-musl/release/ufetch openwrt/root/usr/bin/

# 3. 在 OpenWrt SDK 根目录
cp -r /path/to/ufetch-core/openwrt package/ufetch-core
make package/ufetch-core/compile V=s
```

包内会安装 `/usr/bin/ufetch`，依赖 `ca-bundle`。若固件没有 CA 包，可在构建时
启用 crate 的 `webpki-roots` 特性（默认已启用内置根证书）。

### 6.1 procd 启动示例

```sh
#!/bin/sh /etc/rc.common
START=99

start_service() {
  procd_open_instance
  procd_set_param command /usr/bin/ufetch \
    https://example.com/app.apk \
    -o /tmp/ufetch -f app.apk -n 0 \
    --status-file /tmp/ufetch/status.json \
    --control-file /tmp/ufetch/ctl.json
  procd_set_param respawn
  procd_close_instance
}
```

### 6.2 LuCI 读取与下发

- 读 `/tmp/ufetch/status.json` 显示百分比、速度、`active/connections`；
- 向 `/tmp/ufetch/ctl.json` 写 `{"cmd":"pause"}` 即可暂停，无需 ubus 接口；
- 升级成功后由插件脚本校验 `.apk` / `.ipk` 并调用 opkg。

## 9. 行为与限制

- **探测**：`GET` + `Range: bytes=0-0`；206 取总大小，200 则复用该响应走单流。
- **分段阈值**：≤ 2 MiB 默认单流；`--force-split` 可强制分段（每段 ≥ 64 KiB）。
- **并发爬升**：从 4 起步，逐秒最多加差额的一半，区间失败退两档，上限 8（硬上限 32）。
- **慢段接管**：队列为空且有空闲连接时，把最慢在传段对半拆开；不切进已写区域。
- **无 Range**：只能单流；暂停后继续会从 0 重下（没有区间可续）。
- **取消**：默认保留半成品与侧车，便于下次续传；`--no-keep-partial` 才删除。
- **服务器换文件**：ETag 变化、总大小变小、416 时清空临时文件，最多重下一次。
- **校验失败**：删除临时文件并报错，不会把坏文件改名为目标文件。

## 10. 排错

| 现象 | 处理 |
| --- | --- |
| `certificate verify failed` | 安装 `ca-bundle`，或 `--insecure` 临时绕过 |
| 403 / 429 | 降低 `-n`，换镜像；持续 403 通常是 UA 或防盗链 |
| 一直 0 B/s | 检查 `--proxy`、DNS、`UFETCH_LOG=debug` |
| `unrecognized option '--fix-cortex-a53-843419'` | 用 SDK 的 `CC_<target>` 与 linker（见 1.1） |
| 磁盘不足 | 预留 ≈ 文件大小 + 侧车空间；`/tmp` 是 tmpfs 时注意内存 |
| 续传后校验失败 | 删除 `.ufetch` 与 `.parts.json` 重下 |
| 小文件只有 1 条连接 | 正常；需要演示多连接加 `--force-split` |

## 11. 性能建议

- 路由器上 `-n 0`（自适应）+ 默认上限 8 足够；再高只会互相抢带宽。
- 大文件用 4 MiB 分块；插件包用 `--force-split`，每段至少 64 KiB。
- 状态文件放 `/tmp` 减少 flash 写入；需要跨重启保留才放 `/etc`。
- 限速按出口带宽设置，不要低于 64 KB/s，否则慢速保护可能误判。

## 12. 许可证

LGPL-3.0-or-later。文本见 [LICENSE](LICENSE) 与 [LICENSE.GPL-3](LICENSE.GPL-3)。
静态链接分发时按 LGPLv3 第 4 节提供可重新链接的方式，并保留版权与许可证声明。
