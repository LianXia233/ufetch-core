// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! 端到端测试：本地 Range 服务端 + 真实 HTTP 下载。
//!
//! 覆盖：多段并发、单流降级、暂停/继续、取消后续传、镜像回退、校验失败、
//! 状态文件、重定向、小文件强制分段。

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ufetch_core::{
    DownloadError, DownloadManager, DownloadRequest, EngineConfig, ManagerConfig, TaskState,
    apply_mirror, human_bytes,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    NoRange,
    /// 每 64 KiB 睡 20 ms（约 3 MB/s），用于暂停 / 取消测试。
    Slow,
    /// 一律 500。
    Broken,
}

fn spawn_hang_server(data: Arc<Vec<u8>>) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    listener.set_nonblocking(true).expect("nonblocking");
    let hits = Arc::new(AtomicUsize::new(0));
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let hits = Arc::clone(&hits);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let data = Arc::clone(&data);
                        let hits = Arc::clone(&hits);
                        std::thread::spawn(move || {
                            use std::io::{Read, Write};
                            let mut stream = stream;
                            let mut buffer = Vec::new();
                            let mut chunk = [0u8; 1024];
                            loop {
                                match stream.read(&mut chunk) {
                                    Ok(0) => return,
                                    Ok(n) => {
                                        buffer.extend_from_slice(&chunk[..n]);
                                        if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                                            break;
                                        }
                                    }
                                    Err(_) => return,
                                }
                            }
                            hits.fetch_add(1, Ordering::Relaxed);
                            let total = data.len() as u64;
                            let head = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nETag: \"hang\"\r\nContent-Range: bytes 0-{}/{total}\r\nConnection: close\r\n\r\n",
                                total.saturating_sub(1)
                            );
                            let _ = stream.write_all(head.as_bytes());
                            let _ = stream.write_all(&data[..100]);
                            let _ = stream.flush();
                            std::thread::sleep(Duration::from_secs(8));
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
    }
    TestServer {
        addr,
        hits,
        ranges,
        stop,
    }
}

/// 支持 keep-alive 的服务端：用于验证分段之间的 TCP 连接复用。
struct KeepAliveServer {
    addr: SocketAddr,
    requests: Arc<AtomicUsize>,
    accepts: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl KeepAliveServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
    fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }
    fn accepts(&self) -> usize {
        self.accepts.load(Ordering::Relaxed)
    }
}

impl Drop for KeepAliveServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
    }
}

fn spawn_keepalive_server(data: Arc<Vec<u8>>) -> KeepAliveServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    listener.set_nonblocking(true).expect("nonblocking");
    let requests = Arc::new(AtomicUsize::new(0));
    let accepts = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let requests = Arc::clone(&requests);
        let accepts = Arc::clone(&accepts);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let data = Arc::clone(&data);
                        let requests = Arc::clone(&requests);
                        accepts.fetch_add(1, Ordering::Relaxed);
                        std::thread::spawn(move || {
                            use std::io::{Read, Write};
                            let mut stream = stream;
                            stream
                                .set_read_timeout(Some(Duration::from_secs(5)))
                                .expect("timeout");
                            loop {
                                let mut buffer = Vec::new();
                                let mut chunk = [0u8; 1024];
                                loop {
                                    match stream.read(&mut chunk) {
                                        Ok(0) => return,
                                        Ok(n) => {
                                            buffer.extend_from_slice(&chunk[..n]);
                                            if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                                                break;
                                            }
                                        }
                                        Err(_) => return,
                                    }
                                }
                                requests.fetch_add(1, Ordering::Relaxed);
                                let text = String::from_utf8_lossy(&buffer);
                                let mut range = None;
                                for line in text.lines() {
                                    if let Some((_, value)) = line.split_once(':')
                                        && line.to_ascii_lowercase().starts_with("range:")
                                        && let Some(value) = value.trim().strip_prefix("bytes=")
                                        && let Some((start, end)) = value.split_once('-')
                                        && let (Ok(start), Ok(end)) =
                                            (start.parse::<u64>(), end.parse::<u64>())
                                    {
                                        range = Some((start, end));
                                    }
                                }
                                let total = data.len() as u64;
                                let (status, start, end) = match range {
                                    Some((start, end)) => {
                                        let start = start.min(total.saturating_sub(1));
                                        let end = end.min(total.saturating_sub(1));
                                        ("206 Partial Content", start, end)
                                    }
                                    None => ("200 OK", 0, total.saturating_sub(1)),
                                };
                                let body = &data[start as usize..=end as usize];
                                let head = format!(
                                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"keepalive\"\r\nContent-Type: application/octet-stream\r\nContent-Range: bytes {start}-{end}/{total}\r\nConnection: keep-alive\r\n\r\n",
                                    body.len()
                                );
                                if stream.write_all(head.as_bytes()).is_err()
                                    || stream.write_all(body).is_err()
                                    || stream.flush().is_err()
                                {
                                    return;
                                }
                            }
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
    }
    KeepAliveServer {
        addr,
        requests,
        accepts,
        stop,
    }
}

struct TestServer {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    ranges: Arc<Mutex<Vec<(u64, u64)>>>,
    stop: Arc<AtomicBool>,
}

impl TestServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }

    fn ranges(&self) -> Vec<(u64, u64)> {
        self.ranges.lock().expect("ranges").clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
    }
}

fn spawn_server(data: Arc<Vec<u8>>, mode: Mode) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    listener.set_nonblocking(true).expect("nonblocking");
    let hits = Arc::new(AtomicUsize::new(0));
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));

    {
        let hits = Arc::clone(&hits);
        let ranges = Arc::clone(&ranges);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let data = Arc::clone(&data);
                        let hits = Arc::clone(&hits);
                        let ranges = Arc::clone(&ranges);
                        let mode = mode;
                        std::thread::spawn(move || {
                            handle(stream, &data, mode, &hits, &ranges);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
    }

    TestServer {
        addr,
        hits,
        ranges,
        stop,
    }
}

fn handle(
    mut stream: TcpStream,
    data: &[u8],
    mode: Mode,
    hits: &AtomicUsize,
    ranges: &Mutex<Vec<(u64, u64)>>,
) {
    use std::io::{Read, Write};
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                if buffer.len() > 16 * 1024 {
                    return;
                }
            }
            Err(_) => return,
        }
    }
    hits.fetch_add(1, Ordering::Relaxed);
    let request = String::from_utf8_lossy(&buffer);
    let mut lines = request.lines();
    let request_line = lines.next().unwrap_or_default();
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let mut range = None;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("range:") {
            let value = line
                .split_once(':')
                .map(|(_, value)| value)
                .unwrap_or("")
                .trim();
            if let Some(value) = value.strip_prefix("bytes=") {
                let pair = value.split('-').collect::<Vec<_>>();
                if pair.len() == 2
                    && let (Ok(start), Ok(end)) = (pair[0].parse::<u64>(), pair[1].parse::<u64>())
                {
                    range = Some((start, end));
                }
            }
        }
    }

    if mode == Mode::Broken {
        let _ = stream.write_all(
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    }
    if path.ends_with("/redirect") {
        let _ = stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: /file.bin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }

    let total = data.len() as u64;
    let (status, start, end, body): (&str, u64, u64, Vec<u8>) = match (mode, range) {
        (Mode::NoRange, _) | (_, None) => ("200 OK", 0, total.saturating_sub(1), data.to_vec()),
        (_, Some((start, end))) => {
            let start = start.min(total.saturating_sub(1));
            let end = end.min(total.saturating_sub(1));
            ranges.lock().expect("ranges").push((start, end));
            (
                "206 Partial Content",
                start,
                end,
                data[start as usize..=end as usize].to_vec(),
            )
        }
    };

    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"test-etag\"\r\nContent-Disposition: attachment; filename=\"test.bin\"\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n",
        body.len()
    );
    let headers = if status.starts_with("206") {
        format!("{headers}Content-Range: bytes {start}-{end}/{total}\r\n\r\n")
    } else {
        format!("{headers}\r\n")
    };
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(&body);
    if mode == Mode::Slow {
        // 已经一次性写完，慢速模式改为分片发送。
    }
    let _ = stream.flush();
}

/// 慢速发送版本：覆盖 handle 的整体写入，按 64 KiB 分片 + 延迟。
fn spawn_slow_server(data: Arc<Vec<u8>>) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    listener.set_nonblocking(true).expect("nonblocking");
    let hits = Arc::new(AtomicUsize::new(0));
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let hits = Arc::clone(&hits);
        let ranges = Arc::clone(&ranges);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let data = Arc::clone(&data);
                        let hits = Arc::clone(&hits);
                        let ranges = Arc::clone(&ranges);
                        std::thread::spawn(move || {
                            slow_handle(stream, &data, &hits, &ranges);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
    }
    TestServer {
        addr,
        hits,
        ranges,
        stop,
    }
}

fn slow_handle(
    mut stream: TcpStream,
    data: &[u8],
    hits: &AtomicUsize,
    ranges: &Mutex<Vec<(u64, u64)>>,
) {
    use std::io::{Read, Write};
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    hits.fetch_add(1, Ordering::Relaxed);
    let request = String::from_utf8_lossy(&buffer);
    let mut range = None;
    for line in request.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("range:")
            && let Some((_, value)) = line.split_once(':')
            && let Some(value) = value.trim().strip_prefix("bytes=")
        {
            let pair = value.split('-').collect::<Vec<_>>();
            if pair.len() == 2
                && let (Ok(start), Ok(end)) = (pair[0].parse::<u64>(), pair[1].parse::<u64>())
            {
                range = Some((start, end));
            }
        }
    }
    let total = data.len() as u64;
    let (status, start, end) = match range {
        Some((start, end)) => {
            let start = start.min(total.saturating_sub(1));
            let end = end.min(total.saturating_sub(1));
            ranges.lock().expect("ranges").push((start, end));
            ("206 Partial Content", start, end)
        }
        None => ("200 OK", 0, total.saturating_sub(1)),
    };
    let body = &data[start as usize..=end as usize];
    let head = if status.starts_with("206") {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"slow-etag\"\r\nContent-Disposition: attachment; filename=\"slow.bin\"\r\nContent-Range: bytes {start}-{end}/{total}\r\nConnection: close\r\n\r\n",
            body.len()
        )
    } else {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"slow-etag\"\r\nContent-Disposition: attachment; filename=\"slow.bin\"\r\nConnection: close\r\n\r\n",
            body.len()
        )
    };
    let _ = stream.write_all(head.as_bytes());
    for piece in body.chunks(64 * 1024) {
        if stream.write_all(piece).is_err() {
            return;
        }
        let _ = stream.flush();
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn payload(size: usize) -> Arc<Vec<u8>> {
    let mut data = Vec::with_capacity(size);
    let mut state: u64 = 0x1234_5678_9abc_def0;
    for _ in 0..size {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        data.push((state >> 33) as u8);
    }
    Arc::new(data)
}

fn manager(connections: u32) -> DownloadManager {
    let engine = EngineConfig {
        connections,
        max_connections: 8,
        ..EngineConfig::openwrt()
    };
    DownloadManager::try_new(ManagerConfig {
        engine,
        max_concurrent: 2,
        progress_interval: Duration::from_millis(100),
        ..ManagerConfig::default()
    })
    .expect("manager")
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ufetch-e2e-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create dir");
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_segment_download_is_byte_exact() {
    let data = payload(8 * 1024 * 1024);
    let server = spawn_server(Arc::clone(&data), Mode::Normal);
    let dir = temp_dir("multi");
    let outcome = manager(4)
        .download_and_wait(DownloadRequest::new(server.url("/file.bin"), &dir).connections(4))
        .await
        .expect("download");
    assert_eq!(outcome.total_bytes as usize, data.len());
    let written = std::fs::read(&outcome.path).expect("read");
    assert_eq!(written, data.as_slice(), "content must match");
    assert_eq!(outcome.file_name, "test.bin");
    let ranges = server.ranges();
    assert!(
        ranges.len() >= 3,
        "expected several range requests, got {ranges:?}"
    );
    assert!(
        outcome.connections >= 2,
        "expected parallelism, got {}",
        outcome.connections
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_without_range_support_falls_back_to_single_stream() {
    let data = payload(3 * 1024 * 1024);
    let server = spawn_server(Arc::clone(&data), Mode::NoRange);
    let dir = temp_dir("norange");
    let outcome = manager(4)
        .download_and_wait(DownloadRequest::new(server.url("/file.bin"), &dir).connections(4))
        .await
        .expect("download");
    assert_eq!(std::fs::read(&outcome.path).expect("read"), data.as_slice());
    assert_eq!(
        server.hits(),
        1,
        "probe response should be reused as the data stream"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redirect_is_followed_and_named_from_disposition() {
    let data = payload(1024 * 1024);
    let server = spawn_server(Arc::clone(&data), Mode::Normal);
    let dir = temp_dir("redirect");
    let outcome = manager(2)
        .download_and_wait(DownloadRequest::new(server.url("/redirect"), &dir))
        .await
        .expect("download");
    assert_eq!(outcome.file_name, "test.bin");
    assert_eq!(std::fs::read(&outcome.path).expect("read"), data.as_slice());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_then_resume_continues_from_sidecar() {
    let data = payload(4 * 1024 * 1024);
    let server = spawn_slow_server(Arc::clone(&data));
    let dir = temp_dir("pause");
    let task =
        manager(3).download(DownloadRequest::new(server.url("/slow.bin"), &dir).connections(3));
    let mut progress = task.subscribe();
    loop {
        if progress.borrow().downloaded >= 512 * 1024 {
            break;
        }
        if progress.changed().await.is_err() {
            break;
        }
    }
    task.control().pause();
    // 等待进入 Paused。
    let mut deadline = 0u32;
    while progress.borrow().state != TaskState::Paused {
        deadline += 1;
        if deadline > 300 {
            panic!("task did not pause: {:?}", progress.borrow().state);
        }
        let _ = tokio::time::timeout(Duration::from_millis(100), progress.changed()).await;
    }
    let partial = std::fs::read_dir(&dir)
        .expect("dir")
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().ends_with(".ufetch"))
        .expect("partial file");
    assert!(partial.metadata().expect("meta").len() > 0);

    task.control().resume();
    let outcome = task.wait().await.expect("resume download");
    assert!(
        outcome.resumed_bytes >= 512 * 1024,
        "resume should keep progress, got {}",
        human_bytes(outcome.resumed_bytes)
    );
    assert_eq!(std::fs::read(&outcome.path).expect("read"), data.as_slice());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_then_new_task_resumes() {
    let data = payload(4 * 1024 * 1024);
    let server = spawn_slow_server(Arc::clone(&data));
    let dir = temp_dir("cancel");
    let manager = manager(3);
    let task = manager.download(DownloadRequest::new(server.url("/slow.bin"), &dir).connections(3));
    let mut progress = task.subscribe();
    loop {
        if progress.borrow().downloaded >= 256 * 1024 {
            break;
        }
        if progress.changed().await.is_err() {
            break;
        }
    }
    task.control().cancel();
    let error = task.wait().await.expect_err("must be cancelled");
    assert!(matches!(error, DownloadError::Cancelled), "got {error}");

    let outcome = manager
        .download_and_wait(DownloadRequest::new(server.url("/slow.bin"), &dir).connections(3))
        .await
        .expect("resume");
    assert!(outcome.resumed_bytes > 0, "second run should resume");
    assert_eq!(std::fs::read(&outcome.path).expect("read"), data.as_slice());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checksum_mismatch_is_reported() {
    let data = payload(512 * 1024);
    let server = spawn_server(Arc::clone(&data), Mode::Normal);
    let dir = temp_dir("checksum");
    let error = manager(2)
        .download_and_wait(
            DownloadRequest::new(server.url("/file.bin"), &dir).checksum(
                "sha256=0000000000000000000000000000000000000000000000000000000000000000",
            ),
        )
        .await
        .expect_err("must fail");
    assert!(
        matches!(error, DownloadError::ChecksumMismatch(_)),
        "got {error}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mirror_route_is_used_when_primary_is_unreachable() {
    let data = payload(1024 * 1024);
    let server = spawn_server(Arc::clone(&data), Mode::Normal);
    let dir = temp_dir("mirror");
    let dead = "http://127.0.0.1:9/dead.bin";
    let mirror = format!("http://{}/{{url}}", server.addr);
    let outcome = manager(2)
        .download_and_wait(
            DownloadRequest::new(dead, &dir)
                .mirror(mirror)
                .connections(2),
        )
        .await
        .expect("mirror fallback");
    assert_eq!(std::fs::read(&outcome.path).expect("read"), data.as_slice());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_file_reports_completion() {
    let data = payload(256 * 1024);
    let server = spawn_server(Arc::clone(&data), Mode::Normal);
    let dir = temp_dir("status");
    let status = dir.join("status.json");
    manager(2)
        .download_and_wait(DownloadRequest::new(server.url("/file.bin"), &dir).status_file(&status))
        .await
        .expect("download");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let text = std::fs::read_to_string(&status).expect("status file");
    assert!(text.contains("\"completed\""), "status was {text}");
    assert!(text.contains("test.bin"), "status was {text}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pooled_connections_are_reused_across_segments() {
    let data = payload(2 * 1024 * 1024);
    let server = spawn_keepalive_server(Arc::clone(&data));
    let dir = temp_dir("keepalive");
    let outcome = manager(2)
        .download_and_wait(
            DownloadRequest::new(server.url("/pkg.apk"), &dir)
                .connections(2)
                .force_split(true)
                .file_name("pkg.apk"),
        )
        .await
        .expect("download");
    assert_eq!(std::fs::read(&outcome.path).expect("read"), data.as_slice());
    let requests = server.requests();
    let accepts = server.accepts();
    assert!(
        requests >= 4,
        "expected several ranged requests, got {requests}"
    );
    assert!(
        accepts < requests,
        "connections should be pooled: {accepts} TCP accepts for {requests} requests"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_split_parallelises_a_small_package() {
    let data = payload(512 * 1024);
    let server = spawn_server(Arc::clone(&data), Mode::Normal);
    let dir = temp_dir("forcesplit");
    let outcome = manager(4)
        .download_and_wait(
            DownloadRequest::new(server.url("/pkg.apk"), &dir)
                .connections(4)
                .force_split(true)
                .file_name("pkg.apk"),
        )
        .await
        .expect("download");
    assert_eq!(std::fs::read(&outcome.path).expect("read"), data.as_slice());
    assert!(
        server.ranges().len() >= 3,
        "force-split should issue several ranges, got {:?}",
        server.ranges()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn successful_checksum_is_reported() {
    use sha2::{Digest, Sha256};
    let data = payload(300 * 1024);
    let expected = format!("sha256={:x}", Sha256::digest(data.as_slice()));
    let server = spawn_server(Arc::clone(&data), Mode::Normal);
    let dir = temp_dir("checksum-ok");
    let outcome = manager(2)
        .download_and_wait(DownloadRequest::new(server.url("/file.bin"), &dir).checksum(&expected))
        .await
        .expect("download");
    assert_eq!(outcome.checksum.as_deref(), Some(expected.as_str()));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stall_watchdog_aborts_a_silent_connection() {
    let data = payload(1024 * 1024);
    let server = spawn_hang_server(Arc::clone(&data));
    let dir = temp_dir("stall");
    let engine = EngineConfig {
        stall_timeout: Duration::from_secs(1),
        max_retries: 0,
        connections: 2,
        ..EngineConfig::openwrt()
    };
    let manager = DownloadManager::try_new(ManagerConfig {
        engine,
        max_concurrent: 1,
        progress_interval: Duration::from_millis(100),
        ..ManagerConfig::default()
    })
    .expect("manager");
    let error = manager
        .download_and_wait(DownloadRequest::new(server.url("/file.bin"), &dir).connections(2))
        .await
        .expect_err("stalled connection must fail");
    assert!(
        matches!(error, DownloadError::Stall { .. } | DownloadError::Other(_)),
        "got {error}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn mirror_templates_are_expanded() {
    assert_eq!(
        apply_mirror("https://ghfast.top/", "https://github.com/a/b"),
        "https://ghfast.top/https://github.com/a/b"
    );
    assert_eq!(
        apply_mirror("https://x/{url}", "https://github.com/a/b"),
        "https://x/https://github.com/a/b"
    );
}
