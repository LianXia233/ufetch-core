// SPDX-License-Identifier: LGPL-3.0-or-later
// Copyright (c) 2026 UFetch-Core contributors

//! `ufetch install` 的 apk/opkg 调用与安装后清理。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_root(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("ufetch-cli-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn fake_tool(path: &Path, log: &Path, exit_code: u8) {
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit {exit_code}\n",
        log.display()
    );
    fs::write(path, script).expect("write fake tool");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("chmod");
    }
}

fn write_task(cache: &Path, package: &Path) {
    fs::create_dir_all(cache).expect("cache dir");
    fs::write(package, b"apk").expect("package");
    let record = serde_json::json!([{
        "id": "apk-1",
        "url": "https://example.com/pkg.apk",
        "file_name": "pkg.apk",
        "state": "completed",
        "artifact": package,
        "auto_remove": true
    }]);
    fs::write(cache.join("tasks.json"), record.to_string()).expect("tasks");
}

fn run_install(cache: &Path, fake: &Path) -> std::process::ExitStatus {
    Command::new(env!("CARGO_BIN_EXE_ufetch"))
        .arg("install")
        .arg("apk-1")
        .env("UFETCH_HOME", cache)
        .env("UFETCH_APK", fake)
        .env("UFETCH_LOG", "off")
        .status()
        .expect("run ufetch install")
}

fn run_prepare(cache: &Path) -> std::process::ExitStatus {
    Command::new(env!("CARGO_BIN_EXE_ufetch"))
        .arg("prepare-install")
        .arg("apk-1")
        .env("UFETCH_HOME", cache)
        .env("UFETCH_LOG", "off")
        .status()
        .expect("run ufetch prepare-install")
}

fn run_install_ok(cache: &Path, args: &[&str]) -> std::process::ExitStatus {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ufetch"));
    command.arg("install-ok").args(args);
    command
        .env("UFETCH_HOME", cache)
        .env("UFETCH_LOG", "off")
        .status()
        .expect("run ufetch install-ok")
}

#[test]
#[cfg(unix)]
fn apk_install_uses_allow_untrusted_and_removes_task() {
    let root = temp_root("apk-ok");
    let cache = root.join("cache");
    let package = cache.join("pkg.apk");
    write_task(&cache, &package);
    let fake = root.join("apk");
    let log = root.join("apk.log");
    fake_tool(&fake, &log, 0);

    let status = run_install(&cache, &fake);
    assert!(status.success());
    assert!(!package.exists(), "package should be removed after install");
    assert_eq!(
        fs::read_to_string(cache.join("tasks.json")).expect("tasks"),
        "[]"
    );
    assert!(
        !cache.join("pending-install.json").exists(),
        "pending marker must be cleared"
    );
    let args = fs::read_to_string(&log).expect("log");
    assert!(args.contains("add"));
    assert!(args.contains("--allow-untrusted"));
    assert!(args.contains("pkg.apk"));
}

#[test]
#[cfg(unix)]
fn failed_apk_install_keeps_task_and_package() {
    let root = temp_root("apk-fail");
    let cache = root.join("cache");
    let package = cache.join("pkg.apk");
    write_task(&cache, &package);
    let fake = root.join("apk");
    let log = root.join("apk.log");
    fake_tool(&fake, &log, 1);

    let status = run_install(&cache, &fake);
    assert!(!status.success());
    assert!(package.exists(), "failed install must keep package");
    let tasks = fs::read_to_string(cache.join("tasks.json")).expect("tasks");
    assert!(tasks.contains("apk-1"));
}

#[test]
#[cfg(unix)]
fn install_ok_without_id_uses_pending_marker_and_is_idempotent() {
    let root = temp_root("pending");
    let cache = root.join("cache");
    let package = cache.join("pkg.apk");
    write_task(&cache, &package);
    let status = run_prepare(&cache);
    assert!(status.success());
    assert!(cache.join("pending-install.json").exists());
    assert!(package.exists());

    let status = run_install_ok(&cache, &[]);
    assert!(status.success());
    assert!(!package.exists());
    assert!(!cache.join("pending-install.json").exists());
    assert_eq!(
        fs::read_to_string(cache.join("tasks.json")).expect("tasks"),
        "[]"
    );

    // 包里的 postinst 与旧进程同时收尾时，第二次必须成功返回。
    let status = run_install_ok(&cache, &["apk-1"]);
    assert!(status.success());
}

#[test]
#[cfg(unix)]
fn install_ok_without_pending_is_a_successful_noop() {
    let root = temp_root("no-pending");
    let cache = root.join("cache");
    fs::create_dir_all(&cache).expect("cache");
    let status = run_install_ok(&cache, &[]);
    assert!(status.success());
}

#[test]
#[cfg(unix)]
fn auto_remove_false_keeps_package_and_clears_pending() {
    let root = temp_root("keep");
    let cache = root.join("cache");
    let package = cache.join("pkg.apk");
    write_task(&cache, &package);
    fs::write(cache.join("settings.toml"), "auto_remove = false\n").expect("settings");
    let fake = root.join("apk");
    let log = root.join("apk.log");
    fake_tool(&fake, &log, 0);

    let status = run_install(&cache, &fake);
    assert!(status.success());
    assert!(package.exists());
    assert!(!cache.join("pending-install.json").exists());
    assert!(
        fs::read_to_string(cache.join("tasks.json"))
            .expect("tasks")
            .contains("apk-1")
    );
}
