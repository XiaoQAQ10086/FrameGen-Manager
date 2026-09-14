// 关闭控制台窗口（仅 release）。调试时保留，方便 println! 排查。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod anticheat;
mod deploy;
mod gpu;
mod icon;
mod scan;
mod theme;
mod update;
mod util;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use anticheat::{AcReport, AcTier};
use scan::GameEntry;

fn main() -> eframe::Result<()> {
    // 一次性搬迁：老版本的备份在 %APPDATA%，现在放到程序同级。
    // 放在最前面，这样 GUI 和所有命令行模式看到的是同一份备份。
    // 结果会缓存，App::new() 里再调用拿到的就是同一句话。
    let _ = util::migrate_backups();

    // 无界面自检：cargo run -- --selftest
    if std::env::args().any(|a| a == "--selftest") {
        selftest();
        return Ok(());
    }

    // 软件自身版本检查自测：cargo run -- --selfupdate
    if std::env::args().any(|a| a == "--selfupdate") {
        selfupdatetest();
        return Ok(());
    }

    // 下载测速：走生产路径（镜像优先 + 官方指纹校验）拉一遍 version.dll
    if std::env::args().any(|a| a == "--speedtest") {
        speedtest();
        return Ok(());
    }

    // 显卡与驱动只读自检（不写任何东西）：cargo run -- --gpuinfo
    if std::env::args().any(|a| a == "--gpuinfo") {
        gpuinfo();
        return Ok(());
    }

    // 提权子进程入口。父进程用 ShellExecuteW("runas") 拉起下面两个模式，
    // 结果通过一个临时 JSON 文件回传。必须排在创建窗口之前。
    //   framegen-manager.exe --gpuspoof-apply "NVIDIA GeForce RTX 5060" <结果文件>
    //   framegen-manager.exe --gpuspoof-restore driver|backup <结果文件>
    let argv_gpu: Vec<String> = std::env::args().collect();
    if let Some(i) = argv_gpu.iter().position(|a| a == "--gpuspoof-apply") {
        gpuspoof_apply(
            argv_gpu.get(i + 1).map(String::as_str).unwrap_or(""),
            argv_gpu.get(i + 2).map(String::as_str).unwrap_or(""),
        );
        return Ok(());
    }
    if let Some(i) = argv_gpu.iter().position(|a| a == "--gpuspoof-restore") {
        gpuspoof_restore(
            argv_gpu.get(i + 1).map(String::as_str).unwrap_or(""),
            argv_gpu.get(i + 2).map(String::as_str).unwrap_or(""),
        );
        return Ok(());
    }

    // 下载 + 完整性校验自测（只下 581 字节的 ini）：cargo run -- --downloadtest
    if std::env::args().any(|a| a == "--downloadtest") {
        downloadtest();
        return Ok(());
    }

    // 取消下载 / 残留清理自测：cargo run -- --canceltest
    if std::env::args().any(|a| a == "--canceltest") {
        canceltest();
        return Ok(());
    }

    // 选源排序 + 看门狗自测（本地起慢服务器，不依赖外网）：cargo run -- --sourcetest
    if std::env::args().any(|a| a == "--sourcetest") {
        sourcetest();
        return Ok(());
    }

    // 打开系统设置里「硬件加速 GPU 计划」那一页（测试跳转用）：cargo run -- --openhags
    if std::env::args().any(|a| a == "--openhags") {
        println!("系统构建号 = {:?}", gpu::windows_build());
        println!("用的 URI  = {}", gpu::hags_settings_uri());
        println!("当前状态 = {}", gpu::hags_state().label());
        match gpu::open_hags_settings() {
            Ok(()) => println!("已请求打开系统设置（ShellExecute 成功）"),
            Err(e) => println!("[FAIL] 打开失败: {e}"),
        }
        return Ok(());
    }

    // 把所有候选源实测一遍并打印速度表：cargo run -- --speedall
    // 和界面上「测速」按钮走的是同一个函数，用来验证选源这条链路。
    if std::env::args().any(|a| a == "--speedall") {
        speedall();
        return Ok(());
    }

    // 调试图标提取：cargo run -- --icontest <exe>
    let argv_i: Vec<String> = std::env::args().collect();
    if let Some(pos) = argv_i.iter().position(|a| a == "--icontest") {
        let p = PathBuf::from(argv_i.get(pos + 1).cloned().unwrap_or_default());
        match icon::icon_of(&p) {
            Some(ic) => {
                let total = ic.width * ic.height;
                let mut transparent = 0usize;
                let mut opaque = 0usize;
                let mut luma = 0.0f64;
                for px in ic.rgba.chunks_exact(4) {
                    if px[3] == 0 {
                        transparent += 1;
                    }
                    if px[3] == 255 {
                        opaque += 1;
                    }
                    luma += 0.299 * px[0] as f64 + 0.587 * px[1] as f64 + 0.114 * px[2] as f64;
                }
                let corner_alpha = |x: usize, y: usize| ic.rgba[(y * ic.width + x) * 4 + 3];
                println!("{}", p.display());
                println!("  尺寸 = {}x{}", ic.width, ic.height);
                println!(
                    "  完全透明 {:.1}%   完全不透明 {:.1}%   平均亮度 {:.0}",
                    transparent as f64 / total as f64 * 100.0,
                    opaque as f64 / total as f64 * 100.0,
                    luma / total as f64
                );
                println!(
                    "  四角 alpha = {},{},{},{} （有透明角说明带 alpha 通道，正常）",
                    corner_alpha(0, 0),
                    corner_alpha(ic.width - 1, 0),
                    corner_alpha(0, ic.height - 1),
                    corner_alpha(ic.width - 1, ic.height - 1)
                );
            }
            None => println!("{} -> 提取失败", p.display()),
        }
        return Ok(());
    }

    // 调试文件身份判定：cargo run -- --idtest <文件>
    let argv_id: Vec<String> = std::env::args().collect();
    if let Some(pos) = argv_id.iter().position(|a| a == "--idtest") {
        let p = PathBuf::from(argv_id.get(pos + 1).cloned().unwrap_or_default());
        let id = scan::identify_dll(&p);
        println!("{}", p.display());
        println!("  身份 = {}  ({:?})", id.label(), id);
        println!("  可安全覆盖 = {}", id.is_ours());
        return Ok(());
    }

    // 调试 DLSS 运行库下载：cargo run -- --dlssrun
    if std::env::args().any(|a| a == "--dlssrun") {
        println!("===== DLSS 运行库下载自测 =====");
        let c = match update::client() {
            Ok(c) => c,
            Err(e) => {
                println!("[FAIL] {e}");
                return Ok(());
            }
        };
        for (prefix, tag, _zip, _dll, label) in update::DLSS_RUNTIME {
            match update::find_release_zip(&c, update::DLSS_REPO, prefix, tag) {
                Ok(a) => println!(
                    "  查到 {label}: tag={}  asset={}  ({})",
                    a.tag,
                    a.asset_name,
                    util::format_bytes(a.size)
                ),
                Err(e) => println!("  {label} 查找失败: {e}"),
            }
        }
        let cancel = AtomicBool::new(false);
        let plan = update::dlss_runtime_plan(&c);
        let total: u64 = plan.iter().map(|s| s.size).sum();
        let ctx = update::ProgressCtx {
            base_bytes: 0,
            total_bytes: total,
            base_step: 0,
            total_steps: plan.len(),
        };
        match update::ensure_dlss_runtime(&c, &cancel, &plan, ctx, 0, |msg, _f| {
            println!("  {msg}");
        }) {
            Ok(paths) => {
                for p in &paths {
                    let id = scan::identify_dll(p);
                    let sz = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                    println!(
                        "  [OK] {}  {}  {}",
                        p.display(),
                        util::format_bytes(sz),
                        id.label()
                    );
                }
            }
            Err(e) => println!("  [FAIL] {e}"),
        }
        if let Ok(d) = util::assets_dir() {
            println!("  资产目录: {}", d.display());
            let mut leftovers = 0;
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.ends_with(".zip") || n.ends_with(".part") {
                    println!("  [警告] 残留: {n}");
                    leftovers += 1;
                }
            }
            println!("  残留 zip/part 数量 = {leftovers}（应为 0）");
        }
        println!("===== 结束 =====");
        return Ok(());
    }

    // 调试 zip 单文件解压：cargo run -- --ziptest <zip> <输出文件>
    let argv_z: Vec<String> = std::env::args().collect();
    if let Some(pos) = argv_z.iter().position(|a| a == "--ziptest") {
        let zip = PathBuf::from(argv_z.get(pos + 1).cloned().unwrap_or_default());
        let out = PathBuf::from(argv_z.get(pos + 2).cloned().unwrap_or_default());
        match update::zip_extract_dll(&zip, &out) {
            Ok(name) => {
                let id = scan::identify_dll(&out);
                let sz = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
                println!("zip    : {}", zip.display());
                println!("条目   : {name}");
                println!("输出   : {}  {} 字节", out.display(), sz);
                println!("身份   : {} ({:?})", id.label(), id);
                println!("sha256 : {}", util::sha256_file(&out).unwrap_or_default());
            }
            Err(e) => println!("解压失败: {e}"),
        }
        return Ok(());
    }


    // 备用源实测：cargo run -- --backuptest
    if std::env::args().any(|a| a == "--backuptest") {
        println!("===== 备用源实测 =====");
        let c = match update::client() {
            Ok(c) => c,
            Err(e) => {
                println!("[FAIL] {e}");
                return Ok(());
            }
        };
        let path = update::INI_REPO_PATH;
        let remote = match update::probe_remote(&c, path) {
            Ok(r) => r,
            Err(e) => {
                println!("[FAIL] 取远端指纹失败: {e}");
                return Ok(());
            }
        };
        let dest = update::asset_path("backup-test.ini").unwrap_or_default();
        let cancel = AtomicBool::new(false);
        let url = format!(
            "{}{}",
            update::DEFAULT_BACKUP_PREFIX,
            update::official_url(path)
        );
        println!("  内置备用源 = {}", update::DEFAULT_BACKUP_PREFIX);
        println!("  实际请求   = {url}");
        match update::download(
            &c,
            path,
            &dest,
            &url,
            Some(&remote.etag),
            &cancel,
            update::DEFAULT_BACKUP_PREFIX,
            0,
            &mut |_, _, _| {},
        ) {
            Ok(dl) => {
                // 通过标准是「长度对得上」。有些镜像（比如现在的 gh-proxy.com）
                // 不转发 GitHub 的 ETag，拿不到就没法比指纹 —— 那是镜像的特性，
                // 不是下载失败。代理 DLL 由签名校验兜底，ini 走官方优先不受影响。
                let len_ok = dl.bytes == remote.size;
                println!(
                    "  [{}] 走镜像下载 {} 字节（期望 {}）",
                    if len_ok { "PASS" } else { "FAIL" },
                    dl.bytes,
                    remote.size
                );
                match dl.etag.as_deref() {
                    Some(e) if e.eq_ignore_ascii_case(&remote.etag) => {
                        println!("  ETag 比对：通过")
                    }
                    Some(e) => println!("  ETag 比对：不一致！镜像可能返回了错误内容 ({e})"),
                    None => println!("  ETag 比对：该镜像不转发 ETag，跳过（属正常）"),
                }
                let _ = std::fs::remove_file(&dest);
            }
            Err(e) => println!("  [FAIL] {e}"),
        }
        return Ok(());
    }

    // 部署/还原端到端自测：cargo run -- --deploytest
    if std::env::args().any(|a| a == "--deploytest") {
        deploytest();
        return Ok(());
    }

    // 调试 PE 解析：cargo run -- --pedump <exe>
    let argv: Vec<String> = std::env::args().collect();
    if let Some(pos) = argv.iter().position(|a| a == "--pedump") {
        let p = PathBuf::from(argv.get(pos + 1).cloned().unwrap_or_default());
        println!("== {} ==", p.display());
        println!("size = {:?}", std::fs::metadata(&p).map(|m| m.len()));
        let imports = scan::pe_imports(&p);
        println!("imports({}) = {:?}", imports.len(), imports);
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FrameGen Manager")
            // 卡片比原来的纯文本行高，默认窗口给大一点，四张卡片不用滚动就能看全
            .with_inner_size([1060.0, 820.0])
            .with_min_inner_size([880.0, 560.0]),
        ..Default::default()
    };

    // 关闭窗口 -> run_native 返回 -> main 返回 -> 进程退出。无后台常驻。
    eframe::run_native(
        "FrameGen Manager",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

// ------------------------------------------------------------------ 自检

// ---------------------------------------------------------------- 显卡：自检与提权子进程

/// 只读自检：把所有渠道看到的显卡名列出来，用来判断到底哪一层被改过。
/// cargo run -- --gpuinfo
fn gpuinfo() {
    let adapters = gpu::enumerate();
    println!("=== 显卡实例（{} 个）===", adapters.len());
    for a in &adapters {
        println!("  类键实例    : {}", a.class_sub);
        println!("  驱动记录名  : {}", a.driver_name);
        println!("  驱动版本    : {}", a.driver_version);
        println!("  Enum 键     : HKLM\\{}", a.enum_key);
        println!("  硬件 ID     : {}", a.hardware_id);
        match &a.device_desc {
            Some(d) => {
                println!("  DeviceDesc  : {d}");
                println!("  实际显示名  : {}", gpu::display_name(d));
            }
            None => println!("  DeviceDesc  : (该值不存在)"),
        }
        println!("  是否被改过  : {}", if a.spoofed() { "是" } else { "否" });
        println!();
    }

    println!("=== 驱动 ===");
    match gpu::detect_driver(&adapters) {
        Some(d) => {
            println!("  市场版本     : {}（来源 {}）", d.marketing, d.source);
            if let Some(w) = &d.windows {
                // 注意：这是驱动的 Windows 格式版本号（32.0.16.1692 这种），
                // 不是系统版本 —— 原来的标签会让人误以为是系统版本
                println!("  驱动的 Windows 格式版本 : {w}");
            }
            println!(
                "  低于 {}      : {}",
                gpu::MIN_FG_DRIVER_TEXT,
                if d.too_old() { "是（界面会警告）" } else { "否" }
            );
        }
        None => println!("  读取失败：既没有 nvidia-smi，注册表里也没有 DriverVersion"),
    }

    println!();
    match gpu::load_backup() {
        Some(b) => {
            println!("=== 备份（{}）===", b.saved_at);
            for e in &b.entries {
                println!("  {}", e.key);
                println!("    原值        : {:?}", e.original);
                println!("    本工具写入  : {}", e.applied);
            }
        }
        None => println!("=== 备份 === 没有找到备份记录"),
    }
    println!();
    match gpu::backup_path() {
        Ok(p) => println!("备份文件位置: {}", p.display()),
        Err(e) => println!("备份文件位置: 无法确定（{e}）"),
    }

    println!();
    println!("=== 硬件加速 GPU 计划（DLSS 帧生成的系统前提）===");
    println!("  当前状态   : {}", gpu::hags_state().label());
    println!("  系统构建号 : {:?}（Win11 = 22000 起）", gpu::windows_build());
    println!("  跳转 URI   : {}", gpu::hags_settings_uri());
    println!(
        "  注册表位置 : HKLM\\SYSTEM\\CurrentControlSet\\Control\\GraphicsDrivers\\HwSchMode"
    );
    println!("  （这个值可能根本不存在 —— Win11 默认开启，系统不一定写它；读不到只能报未知）");
    println!("  值 -> 状态的翻译：");
    let cases: [(Option<u32>, gpu::HagsState, &str); 5] = [
        (Some(2), gpu::HagsState::Enabled, "2 = 已开启"),
        (Some(1), gpu::HagsState::Disabled, "1 = 已关闭"),
        (None, gpu::HagsState::Unknown, "值不存在 = 未知"),
        (Some(0), gpu::HagsState::Unknown, "0 = 未知（没见过，不瞎猜）"),
        (Some(3), gpu::HagsState::Unknown, "3 = 未知（没见过，不瞎猜）"),
    ];
    for (v, want, label) in cases {
        let got = gpu::hags_from_value(v);
        println!(
            "    [{}] {label}（读到 {:?} -> {}）",
            if got == want { "PASS" } else { "FAIL" },
            v,
            got.label()
        );
    }
}

/// 提权子进程入口：改显卡名。写结果 JSON 后直接退出，不创建窗口。
fn gpuspoof_apply(name: &str, out: &str) {
    use anyhow::Context as _;
    let r = (|| -> anyhow::Result<String> {
        let adapters = gpu::enumerate();
        let a = gpu::primary(&adapters).context("没有找到可操作的 NVIDIA 显卡注册表实例")?;
        gpu::apply(a, name)
    })();
    write_result(out, r);
}

/// 提权子进程入口：还原显卡名。mode = "driver"（驱动记录的名称）| "backup"（改动前的值）
fn gpuspoof_restore(mode: &str, out: &str) {
    use anyhow::Context as _;
    let to = match mode {
        "backup" => gpu::RestoreTo::BackupOriginal,
        _ => gpu::RestoreTo::DriverName,
    };
    let r = (|| -> anyhow::Result<String> {
        let adapters = gpu::enumerate();
        let a = gpu::primary(&adapters).context("没有找到可操作的 NVIDIA 显卡注册表实例")?;
        gpu::restore(a, to)
    })();
    write_result(out, r);
}

fn write_result(out: &str, r: anyhow::Result<String>) {
    let payload = match r {
        Ok(m) => serde_json::json!({ "ok": true, "msg": m }),
        Err(e) => serde_json::json!({ "ok": false, "msg": format!("{e:#}") }),
    };
    let text = payload.to_string();
    if out.is_empty() {
        // 没给结果文件参数时打到控制台，方便手动跑
        println!("{text}");
        return;
    }
    if let Err(e) = std::fs::write(out, &text) {
        println!("写结果文件失败: {e}");
    }
}

/// 软件自身版本检查自测。这一套是「FrameGen Manager 自己有没有新版本」，
/// 和上游 Mod 的更新检查是两回事。
fn selfupdatetest() {
    println!("===== 软件自身更新检查 =====");
    println!("  本地版本 = {}", update::SELF_VERSION);
    println!("  仓库     = {}", update::SELF_REPO);
    println!("  发布页   = {}", update::RELEASES_URL);

    println!("  版本比较：");
    let cases: [(&str, &str, bool); 6] = [
        ("0.3.0", "0.2.0", true),
        ("v0.3.0", "0.2.0", true),
        ("0.2.0", "0.2.0", false),
        ("0.1.9", "0.2.0", false),
        ("0.2.10", "0.2.9", true),
        // 解析不出来的绝不能当成「有新版本」，否则会误报
        ("abc", "0.2.0", false),
    ];
    for (remote, local, want) in cases {
        let got = update::is_newer(remote, local);
        println!(
            "    [{}] is_newer({remote}, {local}) = {got}",
            if got == want { "PASS" } else { "FAIL" }
        );
    }

    println!("  多源取最大（绕开 CDN 缓存）：");
    let pick_cases: [(&[&str], Option<&str>, &str); 4] = [
        (&["0.3.0", "0.4.0", "0.2.0"], Some("0.4.0"), "取最大的那个"),
        (&["0.4.0", "0.3.0"], Some("0.4.0"), "顺序不影响"),
        (&["abc", "0.3.0"], Some("0.3.0"), "解析不出来的忽略掉"),
        (&[], None, "一个都没有 -> 没有新版本"),
    ];
    for (list, want, label) in pick_cases {
        let got = update::pick_latest(list.iter().map(|s| (*s).to_owned()));
        let ok = got.as_deref() == want;
        println!(
            "    [{}] {label}（{:?} -> {:?}）",
            if ok { "PASS" } else { "FAIL" },
            list,
            got
        );
    }

    println!("  实际查询：");
    match update::client().ok().and_then(|c| update::fetch_latest_self_version(&c)) {
        Some(v) => {
            println!("    远端版本 = {v}");
            println!(
                "    需要更新 = {}",
                update::is_newer(&v, update::SELF_VERSION)
            );
        }
        None => println!("    [FAIL] 取远端版本失败（网络问题）"),
    }
}

/// 下载测速。走的就是界面上「下载 / 更新资产」那条路径，
/// 所以测出来的数就是用户实际会遇到的数。
fn speedtest() {
    println!("===== 下载测速（生产路径：镜像优先 + 官方指纹校验）=====");
    let c = match update::client() {
        Ok(c) => c,
        Err(e) => {
            println!("[FAIL] 创建客户端失败: {e}");
            return;
        }
    };
    let repo_path = update::proxy_repo_path("version.dll");
    let remote = match update::probe_remote(&c, repo_path) {
        Ok(r) => r,
        Err(e) => {
            println!("[FAIL] 取远端指纹失败: {e}");
            return;
        }
    };
    println!(
        "  远端 {}  {} 字节  指纹 {}...",
        remote.name,
        remote.size,
        &remote.etag[..remote.etag.len().min(12)]
    );

    let dest = match update::asset_path(&update::local_name(repo_path)) {
        Ok(d) => d,
        Err(e) => {
            println!("[FAIL] 定位目标失败: {e}");
            return;
        }
    };
    let cancel = AtomicBool::new(false);
    let t0 = std::time::Instant::now();
    let mut last = 0u64;
    let r = update::download_auto(
        &c,
        repo_path,
        &dest,
        Some(&remote.etag),
        &cancel,
        "",
        true,
        update::DEFAULT_MIN_SPEED_KBPS,
        // 和界面里一样：代理 DLL 必须带本项目签名
        &|p: &Path| {
            let id = scan::identify_dll(p);
            if id.is_ours() {
                Ok(())
            } else {
                Err(anyhow::anyhow!("签名校验失败，判定为「{}」", id.label()))
            }
        },
        &mut |got, _, _src| {
            if got.saturating_sub(last) >= 4 * 1024 * 1024 {
                last = got;
                println!("      ... {} MB", got / (1024 * 1024));
            }
        },
    );
    let dt = t0.elapsed().as_secs_f64();
    match r {
        Ok(dl) => {
            let mb = dl.bytes as f64 / (1024.0 * 1024.0);
            println!(
                "  [PASS] {mb:.2} MB 用时 {dt:.2}s => {:.2} MB/s",
                if dt > 0.0 { mb / dt } else { 0.0 }
            );
            match &dl.etag {
                Some(e) => println!("  ETag 比对：通过（响应 ETag = {e}）"),
                None => println!(
                    "  ETag 比对：这次是从镜像拿的，镜像不转发 ETag，跳过 —— \
                     已由签名校验兜住（这正是 download_auto 里 prefer_mirror=true 时必须配 verify 的原因）"
                ),
            }
            println!("  保存于 {}", dest.display());
        }
        Err(e) => println!("  [FAIL] {e}"),
    }
}

/// 本地起一个 HTTP 服务，按 chunk/delay 的节奏往外吐 bytes 字节。
/// 用来把「看门狗」和「放宽速度要求后的兜底重试」真跑一遍 —— 不依赖外网，结果可重复。
fn spawn_http_server(bytes: u64, chunk: u64, delay_ms: u64) -> (u16, Arc<AtomicBool>) {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本地端口失败");
    let port = listener.local_addr().expect("拿本地端口失败").port();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if s2.load(Ordering::Relaxed) {
                break;
            }
            let Ok(mut sock) = stream else { continue };
            // 客户端中途放弃时不能让这里一直阻塞
            let _ = sock.set_write_timeout(Some(std::time::Duration::from_secs(2)));
            let mut req = [0u8; 2048];
            let _ = sock.read(&mut req);
            let head = format!(
                "HTTP/1.1 200 OK
Content-Length: {bytes}
Content-Type: application/octet-stream

"
            );
            if sock.write_all(head.as_bytes()).is_err() {
                continue;
            }
            let buf = vec![0u8; chunk as usize];
            for _ in 0..bytes.div_ceil(chunk.max(1)) {
                if s2.load(Ordering::Relaxed) || sock.write_all(&buf).is_err() {
                    break;
                }
                let _ = sock.flush();
                if delay_ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                }
            }
        }
    });
    (port, stop)
}

/// 实测每个候选源的下载速率，打印成表。
/// **和界面上「测速」按钮调的是同一个 speed_test_all**，所以这个命令的
/// 结果就代表了那个按钮会不会工作。
fn speedall() {
    println!("===== 下载源实测（每个源拉 512 KB）=====");
    let c = match update::client() {
        Ok(c) => c,
        Err(e) => {
            println!("[FAIL] 建客户端失败: {e}");
            return;
        }
    };
    let cancel = AtomicBool::new(false);
    let list = update::speed_test_all(&c, &cancel, |msg| println!("  {msg}"));
    println!();
    for s in &list {
        match &s.error {
            Some(e) => println!("  {:<26} 不可用（{e}）", s.label),
            None => println!("  {:<26} {} KB/s", s.label, s.kbps),
        }
    }
    println!("
  下载时的实际尝试顺序：");
    let builtins: Vec<String> = update::MIRRORS.iter().map(|s| (*s).to_owned()).collect();
    for (i, m) in update::rank_mirrors(&builtins).iter().enumerate() {
        println!("    {}. {}", i + 1, update::source_label(m));
    }
    println!("  （官方源永远排最后；低于 {} KB/s 的源下载中会被看门狗换掉）", update::DEFAULT_MIN_SPEED_KBPS);
}

fn ck(fails: &mut Vec<String>, ok: bool, what: &str) {
    println!("  [{}] {what}", if ok { "PASS" } else { "FAIL" });
    if !ok {
        fails.push(what.to_owned());
    }
}

/// 选源 / 看门狗自测。
fn sourcetest() {
    println!("===== 选源 + 看门狗 自测 =====");
    let mut fails: Vec<String> = Vec::new();

    println!("-- 源名字 --");
    ck(&mut fails, update::source_label("") == "官方源", "空前缀认成「官方源」");
    ck(
        &mut fails,
        update::source_label("https://gh-proxy.com/") == "gh-proxy.com",
        "镜像前缀转成人能看的名字",
    );

    println!("-- 排序（快的在前、没测过居中、太慢的垫底）--");
    let fast = "https://fast.example/".to_owned();
    let mid = "https://mid.example/".to_owned();
    let dead = "https://dead.example/".to_owned();
    let items = vec![
        (dead.clone(), Some(50u64)),
        (mid.clone(), None),
        (fast.clone(), Some(5000)),
    ];
    let ranked = update::rank_by_scores(&items);
    ck(&mut fails, ranked.first() == Some(&fast), "实测 5000 KB/s 的排最前");
    ck(&mut fails, ranked.get(1) == Some(&mid), "没测过的排中间");
    ck(&mut fails, ranked.get(2) == Some(&dead), "实测 50 KB/s 的垫底");

    let c = match update::client() {
        Ok(c) => c,
        Err(e) => {
            println!("[FAIL] 建客户端失败: {e}");
            return;
        }
    };
    let cancel = AtomicBool::new(false);
    let tmp = std::env::temp_dir();

    // 慢源：8 MB 的响应，每 200ms 只给 32 KB，约 160 KB/s
    let (slow_port, slow_stop) = spawn_http_server(8 * 1024 * 1024, 32 * 1024, 200);
    let d1 = tmp.join("fgm-watchdog-test.bin");
    let _ = std::fs::remove_file(&d1);
    let u1 = format!("http://127.0.0.1:{slow_port}/slow");
    println!("-- 看门狗：慢源（约 160 KB/s，阈值 300 KB/s）--");
    let t0 = std::time::Instant::now();
    // 顺手记下进度回调里报给界面的那几段文字 —— 用户能不能看懂就靠它
    let mut notes: Vec<String> = Vec::new();
    let r1 = update::download(
        &c, "watchdog-test", &d1, &u1, None, &cancel, "本地慢源", 300,
        &mut |_, _, note| {
            if notes.last().map(|n| n != note).unwrap_or(true) {
                notes.push(note.to_owned());
            }
        },
    );
    let el = t0.elapsed().as_secs_f64();
    let msg1 = match &r1 {
        Ok(_) => "居然成功了".to_owned(),
        Err(e) => e.to_string(),
    };
    ck(
        &mut fails,
        msg1.starts_with(update::TOO_SLOW_PREFIX),
        &format!("慢源被拦下：{msg1}"),
    );
    ck(&mut fails, el < 10.0, &format!("拦得够快（{el:.1} 秒，不是等整个文件）"));
    ck(&mut fails, !d1.exists(), "被拦下后没留下文件");
    println!("  进度里报出来的文字：{notes:?}");
    ck(
        &mut fails,
        notes.first().map(|n| n.starts_with("经 ")).unwrap_or(false),
        "正常进度里会说清楚用的是哪个源（经 xxx）",
    );
    ck(
        &mut fails,
        notes.iter().any(|n| n.contains("速度不达标")),
        "换源前会说明原因：速度不达标，换下一个",
    );

    // 小文件豁免：512 KB 的响应，看门狗不该管
    let (small_port, small_stop) = spawn_http_server(512 * 1024, 64 * 1024, 0);
    let d2 = tmp.join("fgm-small-test.bin");
    let _ = std::fs::remove_file(&d2);
    let u2 = format!("http://127.0.0.1:{small_port}/small");
    let r2 = update::download(&c, "small-test", &d2, &u2, None, &cancel, "本地小源", 300, &mut |_, _, _| {});
    println!("-- 小文件不受看门狗管 --");
    ck(&mut fails, r2.is_ok(), "512 KB 的文件正常下完（阈值对它是摆设）");
    ck(
        &mut fails,
        std::fs::metadata(&d2).map(|m| m.len() == 512 * 1024).unwrap_or(false),
        "小文件字节数正确",
    );

    // min_kbps=0：大文件也不该被拦 —— 兜底那一遍靠的就是这个
    let (big_port, big_stop) = spawn_http_server(4 * 1024 * 1024, 256 * 1024, 0);
    let d3 = tmp.join("fgm-big-test.bin");
    let _ = std::fs::remove_file(&d3);
    let u3 = format!("http://127.0.0.1:{big_port}/big");
    let r3 = update::download(&c, "big-test", &d3, &u3, None, &cancel, "本地快源", 0, &mut |_, _, _| {});
    println!("-- min_kbps=0（不按速度挑源）--");
    ck(&mut fails, r3.is_ok(), "4 MB 的文件不被拦");
    ck(
        &mut fails,
        std::fs::metadata(&d3).map(|m| m.len() == 4 * 1024 * 1024).unwrap_or(false),
        "大文件字节数正确",
    );

    // 兜底两遍：第一遍太慢被标，第二遍不按速度挑源就拿到了
    println!("-- 全部太慢 -> 放宽速度要求重试（download_auto 的兜底路径）--");
    let too_slow = AtomicBool::new(false);
    let (s2_port, s2_stop) = spawn_http_server(8 * 1024 * 1024, 32 * 1024, 200);
    let d4 = tmp.join("fgm-fallback-test.bin");
    let _ = std::fs::remove_file(&d4);
    let su = format!("http://127.0.0.1:{s2_port}/x");
    let p1 = update::download_pass(
        &c, "fallback-test", &d4, None, &cancel, &su, &[], true, 300, &|_| Ok(()), &too_slow,
        &mut |_, _, _| {},
    );
    ck(
        &mut fails,
        p1.is_err() && too_slow.load(Ordering::Relaxed),
        "第一遍：唯一的源太慢，被标记为「太慢」",
    );
    let (ok_port, ok_stop) = spawn_http_server(256 * 1024, 64 * 1024, 0);
    let ou = format!("http://127.0.0.1:{ok_port}/y");
    let p2 = update::download_pass(
        &c, "fallback-test", &d4, None, &cancel, &ou, &[], true, 0, &|_| Ok(()), &too_slow,
        &mut |_, _, _| {},
    );
    ck(&mut fails, p2.is_ok(), "第二遍：放宽速度要求后拿到了文件");

    for s in [&slow_stop, &small_stop, &big_stop, &s2_stop, &ok_stop] {
        s.store(true, Ordering::Relaxed);
    }
    for d in [&d1, &d2, &d3, &d4] {
        let _ = std::fs::remove_file(d);
    }

    println!("
===== 结果: {} 项失败 =====", fails.len());
    for f in &fails {
        println!("  - {f}");
    }
}

fn selftest() {
    println!("===== FrameGen Manager 自检 =====");

    match util::app_data_dir() {
        Ok(d) => println!("[OK]   数据目录: {}", d.display()),
        Err(e) => println!("[FAIL] 数据目录: {e}"),
    }

    println!("\n--- Steam ---");
    let roots = scan::steam_roots();
    println!("根目录: {:?}", roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>());
    for r in &roots {
        for l in scan::steam_libraries(r) {
            println!("  库: {}", l.display());
        }
    }

    println!("\n--- 游戏扫描 ---");
    let games = scan::scan_all();
    println!("共 {} 个", games.len());
    for g in &games {
        let exe = scan::find_render_exe(&g.install_dir);
        let icon_info = exe
            .as_deref()
            .and_then(icon::icon_of)
            .map(|ic| {
                let total = ic.width * ic.height;
                let transparent = ic.rgba.chunks_exact(4).filter(|p| p[3] == 0).count();
                format!(
                    "{}x{}（透明 {:.0}%）",
                    ic.width,
                    ic.height,
                    transparent as f64 / total as f64 * 100.0
                )
            })
            .unwrap_or_else(|| "未取到".to_owned());
        // 部署目标 = 渲染 EXE 所在目录（界面里「用作部署目录」用的也是它）
        let target = exe
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.to_path_buf())
            .unwrap_or_else(|| g.install_dir.clone());
        let st = deploy::state_of(&target);
        println!(
            "  [{}] {}\n       dir    = {}\n       exe    = {}\n       icon   = {}\n       target = {}\n       状态   = {}",
            g.source.label(),
            g.name,
            g.install_dir.display(),
            exe.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "未找到".to_owned()),
            icon_info,
            target.display(),
            st.label()
        );
    }

    println!("\n--- 系统反作弊（注册表服务/驱动） ---");
    let sys = anticheat::scan_system();
    for h in &sys.hits {
        println!("  [{:?}] {} <- {}", h.tier, h.name, h.evidence);
    }
    println!("系统结论: {}", sys.verdict().label());

    println!("\n--- 各游戏目录反作弊判定 ---");
    for g in &games {
        // 和界面里一致：根目录 + 渲染 EXE 所在目录一起看
        let mut r = anticheat::scan_deep(&g.install_dir);
        if let Some(dir) = scan::find_render_exe(&g.install_dir).and_then(|p| p.parent().map(|d| d.to_path_buf())) {
            r.merge(anticheat::scan_game_dir(&dir));
        }
        if r.verdict() != AcTier::None {
            let names: Vec<String> = r.hits.iter().map(|h| format!("{} ({:?})", h.name, h.tier)).collect();
            println!("  {} -> {} : {}", g.name, r.verdict().label(), names.join(", "));
        }
    }

    println!("\n--- 上游更新检查（走 raw 的 HEAD + ETag，完全不占 API 配额） ---");
    match update::client() {
        Ok(c) => {
            println!("  README 版本: {:?}", update::fetch_version(&c));
            let st = update::load_state();
            // 和界面里一样：每个文件发一次 HEAD 拿 ETag，一次 API 都不调
            let specs = [
                "version.dll",
                "altnative/winmm.dll",
                "altnative/dinput8.dll",
                "altnative/winhttp.dll",
                "altnative/dxgi.dll",
                update::INI_REPO_PATH,
            ];
            println!("  逐个 HEAD 取内容指纹（0 次 API 调用）：");
            // 这一段以前要 15 秒以上：release 直链是 github.com，第一次探测会先干等
            // 连接超时才轮到镜像。现在直链探测改镜像优先，连接超时也从 15s 收到 8s。
            let t_head = std::time::Instant::now();
            for path in specs {
                match update::probe_remote(&c, path) {
                    Ok(r) => {
                        let local = update::local_name(path);
                        println!(
                            "  {:<22} etag={} size={:>9} 需要下载={}",
                            local,
                            &r.etag[..r.etag.len().min(10)],
                            r.size,
                            st.needs_update(&local, &r.etag)
                        );
                    }
                    Err(e) => println!("  {:<22} 取指纹失败: {e}", path),
                }
            }

            println!("
  DLSS 运行库（界面里会单独分组显示）：");
            for (_prefix, tag, zip_name, dll_name, _label) in update::DLSS_RUNTIME {
                let url = update::release_url(tag, zip_name);
                let dest = update::asset_path(dll_name).unwrap_or_default();
                let ready =
                    dest.is_file() && scan::identify_dll(&dest) == scan::FileIdentity::Nvidia;
                let size = update::probe_url(&c, &url).map(|(n, _)| n).unwrap_or(0);
                println!(
                    "  {:<22} {}  直链大小 {}  本地={}",
                    dll_name,
                    tag,
                    util::format_bytes(size),
                    if ready { "已就绪(NVIDIA 签名)" } else { "未下载" }
                );
            }
            println!(
                "  取指纹总耗时 {:.1}s（以前光等 github.com 连接超时就要 15s）",
                t_head.elapsed().as_secs_f64()
            );
        }
        Err(e) => println!("  客户端创建失败: {e}"),
    }

    println!("\n--- 显卡与路由 ---");
    let gpu = scan::detect_gpu();
    let route = gpu
        .as_deref()
        .map(scan::classify_gpu)
        .unwrap_or(scan::GpuRoute::Unknown);
    println!("  显卡: {:?}", gpu);
    println!("  路由: {} ({:?})", route.label(), route);

    println!("\n--- 代理入口推荐（按真实部署目录，也就是渲染 EXE 所在目录）---");
    for g in &games {
        let target = scan::find_render_exe(&g.install_dir)
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| g.install_dir.clone());
        let a = scan::advise_proxy(&target);
        println!(
            "  [{}]\n       目标目录 = {}",
            g.name,
            target.display()
        );
        println!(
            "       推荐 {}  依据={}  已分析 {} 个模块  未判定={}",
            a.recommended, a.reason, a.scanned, a.undetermined
        );
        for e in &a.own_existing {
            println!(
                "       已装过本项目: {}（{}，{}）-> 可直接覆盖",
                e.name,
                util::format_bytes(e.bytes),
                e.identity.label()
            );
        }
        for e in &a.occupied {
            println!(
                "       被占用: {}（{}，{}）-> 已跳过",
                e.name,
                util::format_bytes(e.bytes),
                e.identity.label()
            );
        }
    }

    println!("\n--- INI 改写（强制走 SM75 分支做验证）---");
    match update::prepare_deploy_ini(scan::GpuRoute::Sm75, Some("NVIDIA GeForce RTX 2080")) {
        Ok(p) => {
            println!("  生成: {}", p.path.display());
            for c in &p.changes {
                println!("  改动说明: {c}");
            }
            if let Ok(t) = std::fs::read_to_string(&p.path) {
                for l in t.lines() {
                    if l.starts_with("Router") || l.starts_with("KernelImage") {
                        println!("  实际写入: {l}");
                    }
                }
            }
        }
        Err(e) => println!("  失败: {e}"),
    }

    println!("\n===== 自检结束 =====");
}

/// 验证「取消下载」和「残留清理」两条路径。
fn canceltest() {
    println!("===== 取消下载 / 残留清理 自测 =====");
    let c = match update::client() {
        Ok(c) => c,
        Err(e) => {
            println!("[FAIL] 创建客户端失败: {e}");
            return;
        }
    };
    let remote = match update::probe_remote(&c, update::INI_REPO_PATH) {
        Ok(r) => r,
        Err(e) => {
            println!("[FAIL] 取远端指纹失败: {e}");
            return;
        }
    };
    let dest = match update::asset_path("cancel-test.ini") {
        Ok(d) => d,
        Err(e) => {
            println!("[FAIL] 定位目标失败: {e}");
            return;
        }
    };
    let _ = std::fs::remove_file(&dest);
    let part = dest.with_file_name("cancel-test.ini.part");
    let _ = std::fs::remove_file(&part);

    // 一开始就把取消标志置上，下载循环应当在第一次读取前就退出。
    // 走 download_auto 是为了连「多源回退」一起验：取消绝不能被当成
    // 「官方源失败 -> 试备用源」，那样会一路试完所有镜像才报错。
    let cancel = AtomicBool::new(true);
    let r = update::download_auto(
        &c,
        update::INI_REPO_PATH,
        &dest,
        Some(&remote.etag),
        &cancel,
        "",
        false,
        update::DEFAULT_MIN_SPEED_KBPS,
        &|_p| Ok(()),
        &mut |_, _, _| {},
    );

    let msg = match &r {
        Ok(_) => "意外成功了".to_owned(),
        Err(e) => format!("{e}"),
    };
    println!("  下载结果: {msg}");
    println!("  [{}] 目标文件未被创建", if dest.exists() { "FAIL" } else { "PASS" });
    println!("  [{}] 没有 .part 残留", if part.exists() { "FAIL" } else { "PASS" });
    println!(
        "  [{}] 取消被识别为「已取消」而不是下载失败",
        if msg.contains(update::CANCELLED_MSG) { "PASS" } else { "FAIL" }
    );
    println!(
        "  [{}] 没有继续去试后面的镜像",
        if msg.contains("备用源") { "FAIL" } else { "PASS" }
    );

    // 手工造一个残留，验证清理函数能扫到
    let _ = std::fs::write(&part, b"leftover");
    let before = part.exists();
    let n = update::clean_stale_partials();
    println!(
        "  [{}] 手工造的 .part 被清理（清理了 {n} 个，之前存在={before}）",
        if !part.exists() { "PASS" } else { "FAIL" }
    );
    let _ = std::fs::remove_file(&dest);
    println!("===== 结束 =====");
}

/// 只下载 581 字节的 dlssg_sm86.ini，用来验证下载 + git blob sha 校验链路。
/// 故意不下 15.6 MB 的 DLL，避免自测里跑大流量。
fn downloadtest() {
    println!("===== 下载 + git blob sha 校验 自测 =====");
    let c = match update::client() {
        Ok(c) => c,
        Err(e) => {
            println!("[FAIL] 创建客户端失败: {e}");
            return;
        }
    };
    let repo_path = update::INI_REPO_PATH;
    let remote = match update::probe_remote(&c, repo_path) {
        Ok(r) => r,
        Err(e) => {
            println!("[FAIL] 取远端指纹失败: {e}");
            return;
        }
    };
    println!("  远端 {} etag={} size={}", remote.name, remote.etag, remote.size);

    let dest = match update::asset_path(&update::local_name(repo_path)) {
        Ok(d) => d,
        Err(e) => {
            println!("[FAIL] 定位目标失败: {e}");
            return;
        }
    };

    let cancel = AtomicBool::new(false);
    // 走生产路径（多源自动回退），而不是死磕官方那一个地址 ——
    // 官方 raw 现在会间歇性卡十几秒，测试跟着一起卡就没意义了。
    match update::download_auto(
        &c,
        repo_path,
        &dest,
        Some(&remote.etag),
        &cancel,
        "",
        false,
        update::DEFAULT_MIN_SPEED_KBPS,
        &|_p| Ok(()),
        &mut |got, total, _src| {
            if total > 0 && got >= total {
                println!("  已下载 {got} / {total} 字节");
            }
        },
    ) {
        Ok(dl) => {
            let data = std::fs::read(&dest).unwrap_or_default();
            let blob = util::git_blob_sha1(&data);
            // 字节数是两条路径都拿得到的一致性信号，先看这个
            println!(
                "  [{}] 响应字节数 = {}（HEAD 拿到 {}）",
                if dl.bytes == remote.size { "PASS" } else { "FAIL" },
                dl.bytes,
                remote.size
            );
            // ETag 只有官方源给。官方 raw 抽风时会自动回退到镜像，镜像不转发 ETag，
            // 这时候「没得比」和「比出来不一样」是两码事，不能都算 FAIL。
            match dl.etag.as_deref().map(|e| e.eq_ignore_ascii_case(&remote.etag)) {
                Some(true) => println!("  [PASS] 响应 ETag 和 HEAD 一致"),
                Some(false) => println!(
                    "  [FAIL] 响应 ETag = {} 和 HEAD 拿到的 {} 不一致，内容可能不是同一个版本",
                    dl.etag.as_deref().unwrap_or(""),
                    remote.etag
                ),
                None => println!(
                    "  [INFO] 这次是从加速镜像拿到的，镜像不转发 ETag，没得比 —— \
                     正式路径上 ini 靠字节数 + 本机记录，DLL 靠本项目签名兜底"
                ),
            }
            println!("  本地内容 sha256 = {}", dl.sha256);
            println!("  内容 blob sha = {blob}");

            // local_is_current 是「跳过重复下载」的依据，两条路径都验一遍：
            // 记录 + 文件都在 -> 已是最新；把文件删掉 -> 必须立刻变回「需要下载」。
            let mut st = update::load_state();
            st.files.insert(
                update::local_name(repo_path),
                update::LocalFile {
                    blob_sha: blob.clone(),
                    etag: remote.etag.clone(),
                    sha256: dl.sha256.clone(),
                    bytes: dl.bytes,
                    downloaded_at: util::now_utc(),
                },
            );
            let probe = dest.with_file_name("iscurrent-probe.tmp");
            let _ = std::fs::copy(&dest, &probe);
            let present =
                update::local_is_current(&st, &update::local_name(repo_path), &probe, &remote.etag);
            let _ = std::fs::remove_file(&probe);
            let gone =
                update::local_is_current(&st, &update::local_name(repo_path), &probe, &remote.etag);
            println!(
                "  [{}] 记录和文件都在时判定为「已是最新」",
                if present { "PASS" } else { "FAIL" }
            );
            println!(
                "  [{}] 文件被删掉后判定为「需要下载」",
                if !gone { "PASS" } else { "FAIL" }
            );
            println!("  保存于: {}", dest.display());
            println!(
                "  内容:\n{}",
                String::from_utf8_lossy(&data[..data.len().min(200)])
            );
        }
        Err(e) => println!("  [FAIL] 下载失败: {e}"),
    }
}

/// 在一个真实目录上跑完整的 部署 -> 校验 -> 还原 流程，最后删掉测试目录。
fn deploytest() {
    use std::fs;

    println!("===== 部署 / 备份 / 还原 端到端自测 =====");
    let root = PathBuf::from(r"D:\Test\deploytest");
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    let target = root.join("target");
    if fs::create_dir_all(&src).is_err() || fs::create_dir_all(&target).is_err() {
        println!("[FAIL] 无法创建测试目录");
        return;
    }

    let dll_src = src.join("version.dll");
    let ini_src = src.join("dlssg_sm86.ini");
    fs::write(&dll_src, b"FAKE_DLL_PAYLOAD_V1").unwrap();
    fs::write(&ini_src, b"FAKE_INI_PAYLOAD_V1").unwrap();

    // 目标目录里预先放一个用户自己的 INI，它必须被备份并在还原时恢复
    let orig_ini: &[u8] = b"ORIGINAL_INI_FROM_USER";
    fs::write(target.join("dlssg_sm86.ini"), orig_ini).unwrap();

    let mut fails = 0usize;
    macro_rules! check {
        ($cond:expr, $label:expr) => {
            if $cond {
                println!("  [PASS] {}", $label);
            } else {
                println!("  [FAIL] {}", $label);
                fails += 1;
            }
        };
    }

    // ---- 旧代理入口的处置规则（纯逻辑，不需要真 DLL）----
    println!("
-- 旧代理入口怎么处置 --");
    check!(
        deploy::plan_orphan(true, false, true, true, false) == deploy::OrphanAction::Remove,
        "原本空着、我们放进去的旧入口 -> 删掉"
    );
    check!(
        deploy::plan_orphan(true, false, true, true, true)
            == deploy::OrphanAction::RemoveButKeepRecord,
        "原本就有文件的旧入口 -> 删掉但保留备份记录（这次修的就是这条）"
    );
    check!(
        deploy::plan_orphan(true, true, true, true, true) == deploy::OrphanAction::Keep,
        "这次还要用的入口 -> 不碰"
    );
    check!(
        deploy::plan_orphan(true, false, true, false, false) == deploy::OrphanAction::Keep,
        "内容已被用户换过 -> 不碰"
    );
    check!(
        deploy::plan_orphan(false, false, true, true, false) == deploy::OrphanAction::Keep,
        "不是代理入口 -> 不碰"
    );
    check!(
        deploy::plan_orphan(true, false, false, false, true) == deploy::OrphanAction::Keep,
        "文件已经不在了 -> 不碰"
    );

    check!(
        deploy::state_of(&target) == deploy::DeployState::NotDeployed,
        "初始状态 = 未部署"
    );

    let dep_files = [
        deploy::DeployFile::new("version.dll", &dll_src),
        deploy::DeployFile::new(deploy::INI_NAME, &ini_src),
    ];
    match deploy::deploy(&target, "version.dll", &dep_files, &[]) {
        Ok(_) => {
            check!(
                fs::read(target.join("version.dll")).ok().as_deref() == Some(&b"FAKE_DLL_PAYLOAD_V1"[..]),
                "部署后代理 DLL 内容正确"
            );
            check!(
                fs::read(target.join("dlssg_sm86.ini")).ok().as_deref() == Some(&b"FAKE_INI_PAYLOAD_V1"[..]),
                "部署后 INI 内容正确"
            );
            check!(
                matches!(deploy::state_of(&target), deploy::DeployState::Deployed { .. }),
                "部署后状态 = 已部署"
            );
            check!(
                !target.join(".version.dll.tmp").exists(),
                "没有残留临时文件"
            );

            match deploy::restore(&target) {
                Ok(_) => {
                    check!(!target.join("version.dll").exists(), "还原后代理 DLL 已移除");
                    check!(
                        fs::read(target.join("dlssg_sm86.ini")).ok().as_deref() == Some(orig_ini),
                        "还原后 INI 恢复为原内容"
                    );
                    check!(
                        deploy::state_of(&target) == deploy::DeployState::NotDeployed,
                        "还原后状态 = 未部署"
                    );
                }
                Err(e) => {
                    println!("  [FAIL] 还原报错: {e}");
                    fails += 1;
                }
            }
        }
        Err(e) => {
            println!("  [FAIL] 部署报错: {e}");
            fails += 1;
        }
    }

    // 重部署：上游更新之后用户会再点一次「部署」。
    // 关键：这时不能把我们上次部署进去的文件当成「游戏原文件」重新备份，
    // 否则真正的原件会被覆盖掉，「还原」就还原不回游戏原样了。
    println!("
-- 重部署（模拟上游更新后再部署一次）--");
    // 这里只部署 INI、不带 DLL：deploy 会检查目标里同名文件的签名，测试用的假字节
    // 没有本项目签名，第二次部署会被正当地拒掉。真实的已签名 DLL（15 MB）没法在自测里
    // 造出来，所以这一段专门验证「原始备份不被自己的旧版本覆盖」。
    let t4 = root.join("redeploy");
    let _ = fs::create_dir_all(&t4);
    fs::write(t4.join(deploy::INI_NAME), orig_ini).unwrap();
    let ini_only = [deploy::DeployFile::new(deploy::INI_NAME, &ini_src)];
    fs::write(&ini_src, b"FAKE_INI_PAYLOAD_V1").unwrap();
    match deploy::deploy(&t4, "version.dll", &ini_only, &[]) {
        Ok(_) => {
            // 上游更新了：源文件换成 V2，注意中间**没有**先还原
            fs::write(&ini_src, b"FAKE_INI_PAYLOAD_V2").unwrap();
            match deploy::deploy(&t4, "version.dll", &ini_only, &[]) {
                Ok(_) => {
                    check!(
                        fs::read(t4.join(deploy::INI_NAME)).ok().as_deref()
                            == Some(&b"FAKE_INI_PAYLOAD_V2"[..]),
                        "重部署后 INI 已是新内容"
                    );
                    let orig_kept = deploy::load_manifest(&t4)
                        .and_then(|m| {
                            m.files.into_iter().find(|e| e.rel_path == deploy::INI_NAME)
                        })
                        .and_then(|e| e.original_sha256)
                        .map(|h| h == util::sha256_hex(orig_ini))
                        .unwrap_or(false);
                    check!(
                        orig_kept,
                        "重部署后备份里仍是最初的用户原始 INI（没被自己的旧版本覆盖）"
                    );
                    match deploy::restore(&t4) {
                        Ok(_) => check!(
                            fs::read(t4.join(deploy::INI_NAME)).ok().as_deref() == Some(orig_ini),
                            "重部署后仍能还原出最初的用户原始 INI"
                        ),
                        Err(e) => {
                            println!("  [FAIL] 重部署后还原报错: {e}");
                            fails += 1;
                        }
                    }
                }
                Err(e) => {
                    println!("  [FAIL] 重部署报错: {e}");
                    fails += 1;
                }
            }
        }
        Err(e) => {
            println!("  [FAIL] 首次部署报错: {e}");
            fails += 1;
        }
    }

    // 换代理入口：旧入口不能留在目录里变成孤儿
    println!("
-- 换代理入口 --");
    let t5 = root.join("switch");
    let _ = fs::create_dir_all(&t5);
    let winmm_src = src.join("winmm.dll");
    fs::write(&winmm_src, b"FAKE_WINMM_V1").unwrap();
    let only_version = [deploy::DeployFile::new("version.dll", &dll_src)];
    let only_winmm = [deploy::DeployFile::new("winmm.dll", &winmm_src)];
    match deploy::deploy(&t5, "version.dll", &only_version, &[]) {
        Ok(_) => {
            check!(t5.join("version.dll").is_file(), "先用 version.dll 部署成功");
            match deploy::deploy(&t5, "winmm.dll", &only_winmm, &[]) {
                Ok(_) => {
                    check!(t5.join("winmm.dll").is_file(), "换入口后 winmm.dll 已部署");
                    check!(
                        !t5.join("version.dll").exists(),
                        "换入口后旧的 version.dll 已被清掉（不再有两个代理并存）"
                    );
                    match deploy::restore(&t5) {
                        Ok(_) => check!(
                            !t5.join("winmm.dll").exists(),
                            "换入口后仍能正常还原"
                        ),
                        Err(e) => {
                            println!("  [FAIL] 换入口后还原报错: {e}");
                            fails += 1;
                        }
                    }
                }
                Err(e) => {
                    println!("  [FAIL] 换入口部署报错: {e}");
                    fails += 1;
                }
            }
        }
        Err(e) => {
            println!("  [FAIL] 首次部署报错: {e}");
            fails += 1;
        }
    }

    // 冲突：目录里已有第三方的 version.dll
    println!("
-- 入口冲突 --");
    let t2 = root.join("conflict");
    let _ = fs::create_dir_all(&t2);
    fs::write(t2.join("version.dll"), b"SOME_OTHER_MOD").unwrap();
    check!(
        deploy::deploy(&t2, "version.dll", &dep_files, &[]).is_err(),
        "已存在第三方 version.dll 时拒绝部署"
    );
    check!(
        fs::read(t2.join("version.dll")).ok().as_deref() == Some(&b"SOME_OTHER_MOD"[..]),
        "被拒绝后第三方文件未被改动"
    );
    let dep_files_alt = [
        deploy::DeployFile::new("winmm.dll", &dll_src),
        deploy::DeployFile::new(deploy::INI_NAME, &ini_src),
    ];
    check!(
        deploy::deploy(&t2, "winmm.dll", &dep_files_alt, &[]).is_ok(),
        "改用 winmm.dll 替代入口可以部署"
    );
    check!(t2.join("winmm.dll").is_file(), "替代入口文件已写入");
    let _ = deploy::restore(&t2);
    check!(!t2.join("winmm.dll").exists(), "替代入口已还原");

    // 反作弊闸门
    println!("
-- 反作弊闸门 --");
    let t3 = root.join("ac");
    let _ = fs::create_dir_all(t3.join("EasyAntiCheat"));
    check!(
        anticheat::scan_deep(&t3).is_blocked(),
        "含 EasyAntiCheat 目录被判为内核级并阻止"
    );
    let t4 = root.join("clean");
    let _ = fs::create_dir_all(&t4);
    let ok_to_deploy = !anticheat::scan_deep(&t4).is_blocked();
    // 注意：这台机器系统级存在 BEService，但那是系统状态，不应影响空目录的判定
    check!(ok_to_deploy, "普通空目录不阻止部署");

    // ---- 已装过本项目：允许覆盖（这是「判断用户是否手动装过」的核心行为）----
    println!("
-- 目标目录已有本项目文件 --");
    let ours_src: Option<PathBuf> = [
        r"D:\Epic Game\HogwartsLegacy\Phoenix\Binaries\Win64\version.dll",
        r"E:\SteamLibrary\steamapps\common\PUBG\TslGame\Binaries\Win64\version.dll",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file());

    match ours_src {
        None => {
            println!("  （本机没找到已安装的本项目文件，跳过这段）");
        }
        Some(real) => {
            check!(
                scan::identify_dll(&real) == scan::FileIdentity::ThisProject,
                "识别出真实的本项目文件（DLSSG Native Project 签名）"
            );
            let t5 = root.join("ours");
            let _ = fs::create_dir_all(&t5);
            fs::copy(&real, t5.join("version.dll")).unwrap();
            check!(
                scan::identify_dll(&t5.join("version.dll")).is_ours(),
                "复制过去后仍判定为本项目文件"
            );
            // 先确认它是「可覆盖」的，再实际部署一次
            let advice = scan::advise_proxy(&t5);
            check!(
                advice.occupied.is_empty() && !advice.own_existing.is_empty(),
                "已有本项目文件时不算被占用，而是归入 own_existing"
            );
            let r = deploy::deploy(&t5, "version.dll", &dep_files, &[]);
            check!(r.is_ok(), "目标已有本项目文件时允许覆盖（不再误拒）");
            let recorded_as_existing = deploy::load_manifest(&t5)
                .map(|m| {
                    m.files
                        .iter()
                        .any(|e| e.rel_path == "version.dll" && e.existed_before)
                })
                .unwrap_or(false);
            check!(recorded_as_existing, "这个位置被记成「原本就有文件」");

            // 换成别的入口再部署：以前这一分支被直接跳过，
            // 结果是两个代理并存，而且原件再也还原不回来。
            let winmm_switch = src.join("winmm_switch.dll");
            fs::write(&winmm_switch, b"FAKE_WINMM_SWITCH").unwrap();
            let switch = [deploy::DeployFile::new("winmm.dll", &winmm_switch)];
            match deploy::deploy(&t5, "winmm.dll", &switch, &[]) {
                Ok(_) => {
                    check!(
                        !t5.join("version.dll").exists(),
                        "换入口后旧的 version.dll 已从游戏目录挪走（不再两个代理并存）"
                    );
                    check!(t5.join("winmm.dll").is_file(), "新入口 winmm.dll 已部署");
                    let backup_kept = deploy::load_manifest(&t5)
                        .map(|m| {
                            m.files.iter().any(|e| {
                                e.rel_path == "version.dll"
                                    && e.existed_before
                                    && e.backup_name.is_some()
                            })
                        })
                        .unwrap_or(false);
                    check!(backup_kept, "旧入口的备份记录保留下来了（还原还管得着）");
                    match deploy::restore(&t5) {
                        Ok(_) => {
                            check!(
                                scan::identify_dll(&t5.join("version.dll")).is_ours(),
                                "还原后原本那份本项目文件原样回来了"
                            );
                            check!(
                                !t5.join("winmm.dll").exists(),
                                "还原后本次部署的 winmm.dll 也移除了"
                            );
                        }
                        Err(e) => {
                            println!("  [FAIL] 换入口后还原报错: {e}");
                            fails += 1;
                        }
                    }
                }
                Err(e) => {
                    println!("  [FAIL] 换入口部署报错: {e}");
                    fails += 1;
                }
            }
        }
    }

    // ---- 目录里有「本项目的另一个代理入口」，但不是本工具装的 ----
    println!("
-- 目录里有另一个本项目代理（手动装的）--");
    let real_ours: Option<PathBuf> = [
        r"D:\Epic Game\HogwartsLegacy\Phoenix\Binaries\Win64\version.dll",
        r"E:\SteamLibrary\steamapps\common\PUBG\TslGame\Binaries\Win64\version.dll",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file());
    match real_ours {
        None => println!("  （本机没有真实的本项目 DLL，跳过这段）"),
        Some(real) => {
            let t6 = root.join("extra-proxy");
            let _ = fs::create_dir_all(&t6);
            // 模拟「用户手动装了 dinput8.dll」，而我们这次要用 version.dll
            fs::copy(&real, t6.join("dinput8.dll")).unwrap();
            check!(
                deploy::find_extra_own_proxies(&t6, "version.dll") == vec!["dinput8.dll".to_owned()],
                "认出了目录里另一个本项目代理（该弹窗问用户）"
            );
            check!(
                deploy::find_extra_own_proxies(&t6, "dinput8.dll").is_empty(),
                "和本次要用的入口同名时不算多余"
            );

            let ver2 = src.join("ver_extra.dll");
            let ini2 = src.join("ini_extra.ini");
            fs::write(&ver2, b"FAKE_VER_EXTRA").unwrap();
            fs::write(&ini2, b"FAKE_INI_EXTRA").unwrap();
            let payload2 = [
                deploy::DeployFile::new("version.dll", &ver2),
                deploy::DeployFile::new(deploy::INI_NAME, &ini2),
            ];
            let r = deploy::deploy(&t6, "version.dll", &payload2, &["dinput8.dll".to_owned()]);
            check!(r.is_ok(), "用户选「移除并继续」后部署成功");
            check!(
                !t6.join("dinput8.dll").exists(),
                "多余的那个本项目代理已移出游戏目录（只剩一个代理）"
            );
            let recorded = deploy::load_manifest(&t6)
                .map(|m| {
                    m.files.iter().any(|e| {
                        e.rel_path == "dinput8.dll" && e.existed_before && e.backup_name.is_some()
                    })
                })
                .unwrap_or(false);
            check!(recorded, "移除的那份原件备份记录留下来了");

            // 再部署一次（用户往往还会再点一次），记录不能被丢掉
            let ini_only2 = [deploy::DeployFile::new(deploy::INI_NAME, &ini2)];
            let r2 = deploy::deploy(&t6, "version.dll", &ini_only2, &[]);
            check!(r2.is_ok(), "再部署一次也成功");
            let recorded2 = deploy::load_manifest(&t6)
                .map(|m| m.files.iter().any(|e| e.rel_path == "dinput8.dll"))
                .unwrap_or(false);
            check!(recorded2, "再部署之后那条记录还在（还原仍管得着）");

            match deploy::restore(&t6) {
                Ok(_) => {
                    check!(
                        scan::identify_dll(&t6.join("dinput8.dll")).is_ours(),
                        "还原后手动装的那份 dinput8.dll 回来了"
                    );
                    check!(
                        !t6.join("version.dll").exists(),
                        "还原后本次部署的 version.dll 已移除"
                    );
                }
                Err(e) => {
                    println!("  [FAIL] 还原报错: {e}");
                    fails += 1;
                }
            }
        }
    }

    let _ = fs::remove_dir_all(&root);
    println!("
===== 结果: {fails} 项失败 =====");
}

// ------------------------------------------------------------------ UI

#[derive(Debug, Clone)]
struct GameRow {
    entry: GameEntry,
    ac: AcTier,
    render_exe: Option<PathBuf>,
    deployed: deploy::DeployState,
    /// 从渲染 EXE 提取出来的图标（RGBA）
    icon: Option<icon::IconImage>,
}

/// 资产清单里一行的状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssetState {
    Ready,
    Missing,
    Outdated,
}

impl AssetState {
    fn label(self) -> &'static str {
        match self {
            AssetState::Ready => "已就绪",
            AssetState::Missing => "未下载",
            AssetState::Outdated => "有更新",
        }
    }
    fn color(self) -> egui::Color32 {
        match self {
            AssetState::Ready => theme::OK,
            AssetState::Missing => theme::NEUTRAL,
            AssetState::Outdated => theme::WARN,
        }
    }
}

/// 资产清单里的一行
#[derive(Debug, Clone)]
struct AssetRow {
    /// 分组名："核心 Mod" / "DLSS 运行库"
    group: &'static str,
    /// 本地文件名
    label: String,
    detail: String,
    bytes: u64,
    /// 核心 Mod：远端内容指纹（raw 的 ETag = 内容 SHA-256），用来判断有没有更新
    remote_etag: Option<String>,
    /// DLSS 运行库：本地文件名，靠签名判断在不在
    runtime_file: Option<String>,
}

#[derive(Debug, Clone)]
struct UpdateSummary {
    version: Option<String>,
    rows: Vec<AssetRow>,
}

enum Msg {
    Scanned(Vec<GameRow>),
    UpdateChecked(UpdateSummary),
    /// 文案 / 总进度 / 总字节数（0 表示还没算出来）
    Progress(String, f32, u64),
    Done(String),
    Failed(String),
    /// 下载失败。单独一个变体，是为了在界面上给出「改用备用源」的提示。
    DownloadFailed(String),
    /// 提权子进程改完 / 还原完显卡名了
    GpuOpDone(Result<String, String>),
    /// 用户点了取消下载。这不是失败，不该弹「改用备用源重试」。
    Cancelled,
    /// 软件自身的版本检查回来了。None 表示没查到（网络问题）。
    /// manual = 用户自己点的「检查更新」；自动检查时没查到就不要打扰他。
    SelfVersionChecked { latest: Option<String>, manual: bool },
    /// 所有候选源的测速结果回来了
    SpeedTested(Vec<update::SourceSpeed>),
}

struct App {
    ctx: egui::Context,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    game_dir: Option<PathBuf>,
    manual_path: String,
    proxy: String,

    games: Vec<GameRow>,
    scanned: bool,

    ac_target: Option<AcReport>,
    ac_system: AcReport,

    deploy_state: deploy::DeployState,
    update_state: update::UpdateState,
    update_summary: Option<UpdateSummary>,

    status: String,
    progress: Option<(String, f32)>,
    busy: bool,
    logs: Vec<String>,
    autoscan_done: bool,
    /// 启动时的那一次「检查本软件新版本」跑过没有
    autocheck_done: bool,
    /// 调试开关：启动就跑一次测速（DLSSG_AUTOSPEED=1），只为截图/排查用
    autospeed_done: bool,

    // ---- 代理入口推荐
    advice: Option<scan::ProxyAdvice>,

    // ---- 显卡
    gpu_name: Option<String>,
    gpu_route: scan::GpuRoute,
    /// 驱动版本。低于建议版本时界面会警告。
    driver: Option<gpu::DriverInfo>,
    /// 所有 NVIDIA PCI 显示适配器的注册表实例（名称伪装用）
    adapters: Vec<gpu::GpuAdapter>,

    // ---- 显卡名称伪装
    /// 查到的新版本号。Some 时右上角会出现下载入口。
    new_version: Option<String>,
    spoof_open: bool,
    spoof_target: String,
    spoof_ack: bool,
    /// 待用户确认的伪装操作。Some 时显示确认弹窗。
    spoof_pending: Option<gpu::Op>,
    /// 驱动过旧时点「部署」需要再确认一次
    confirm_old_driver: bool,
    /// 目标目录里有「本项目的另一个代理入口」时，先弹窗问一句。
    /// Some 里是要问用户是否移除的那些文件名。
    asked_extra_proxies: Option<Vec<String>>,

    // ---- 硬件加速 GPU 计划（DLSS 帧生成的系统前提，只读 + 跳转，绝不写注册表）
    hags: gpu::HagsState,
    /// 部署完成后要不要提示去开硬件加速
    hags_prompt: bool,
    /// 这次 Msg::Done 是不是部署来的（用它决定要不要弹提示）
    deploy_in_flight: bool,
    /// 上一帧窗口有没有焦点 —— 用户从系统设置切回来时重读状态
    was_focused: bool,
    /// 调试开关：DLSSG_FAKE_HAGS=on|off|unknown 强制一个状态，只为截图验证三种显示
    hags_fake: Option<gpu::HagsState>,

    // ---- 下载控制
    cancel: Option<Arc<AtomicBool>>,
    use_backup: bool,
    backup_prefix: String,
    download_failed: bool,
    /// 低于这个速率（KB/s）就换源
    min_speed_kbps: u64,
    /// 测速结果，界面按它列候选源
    speed_results: Vec<update::SourceSpeed>,
    speed_testing: bool,
    /// 本次下载的开始时刻 —— 用来算实时速度和剩余时间
    dl_started: Option<std::time::Instant>,
    dl_total: u64,
    dl_done: u64,

    // ---- 本次部署对 INI 的改动说明
    ini_changes: Vec<String>,

    // ---- 游戏图标纹理缓存，key = 安装目录
    icon_textures: HashMap<String, egui::TextureHandle>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx);
        let (tx, rx) = channel();

        // 清掉上次被强杀可能留下的半成品
        let cleaned = update::clean_stale_partials();
        // 老版本的备份在 %APPDATA%，新版本放到程序同级，这里做一次性搬迁
        let migrated = util::migrate_backups();
        let gpu_name = scan::detect_gpu();
        let gpu_route = gpu_name
            .as_deref()
            .map(scan::classify_gpu)
            .unwrap_or(scan::GpuRoute::Unknown);
        // 显卡实例和驱动版本都是读注册表，很快（nvidia-smi 兜底约 35ms）
        let adapters = gpu::enumerate();
        let driver = gpu::detect_driver(&adapters);
        let cfg = util::load_config();

        let mut boot_notes: Vec<String> = Vec::new();
        if cleaned > 0 {
            boot_notes.push(format!("已清理 {cleaned} 个未完成的下载残留"));
        }
        if let Some(m) = migrated {
            boot_notes.push(m);
        }
        let boot_status = if boot_notes.is_empty() {
            "就绪".to_owned()
        } else {
            format!("就绪（{}）", boot_notes.join("；"))
        };

        Self {
            ctx: cc.egui_ctx.clone(),
            tx,
            rx,
            game_dir: None,
            manual_path: String::new(),
            proxy: scan::PROXY_PRIORITY[0].to_owned(),
            games: Vec::new(),
            scanned: false,
            ac_target: None,
            // 枚举注册表很快，同步做完即可
            ac_system: anticheat::scan_system(),
            deploy_state: deploy::DeployState::NotDeployed,
            update_state: update::load_state(),
            update_summary: None,
            status: boot_status,
            progress: None,
            busy: false,
            logs: boot_notes,
            autoscan_done: false,
            autocheck_done: false,
            autospeed_done: false,
            advice: None,
            gpu_name,
            gpu_route,
            driver,
            adapters,
            new_version: None,
            spoof_open: std::env::var_os("DLSSG_SPOOF_OPEN").is_some(),
            // 默认指向 5060：既是最常见的目标，也和社区流传的做法一致
            spoof_target: gpu::PRESETS.last().copied().unwrap_or_default().to_owned(),
            spoof_ack: false,
            spoof_pending: None,
            confirm_old_driver: false,
            asked_extra_proxies: None,
            hags_fake: match std::env::var("DLSSG_FAKE_HAGS").ok().as_deref() {
                Some("on") | Some("2") => Some(gpu::HagsState::Enabled),
                Some("off") | Some("1") => Some(gpu::HagsState::Disabled),
                Some("unknown") | Some("0") => Some(gpu::HagsState::Unknown),
                _ => None,
            },
            hags: {
                let fake = std::env::var("DLSSG_FAKE_HAGS").ok();
                match fake.as_deref() {
                    Some("on") | Some("2") => gpu::HagsState::Enabled,
                    Some("off") | Some("1") => gpu::HagsState::Disabled,
                    Some("unknown") | Some("0") => gpu::HagsState::Unknown,
                    _ => gpu::hags_state(),
                }
            },
            hags_prompt: false,
            deploy_in_flight: false,
            was_focused: true,
            cancel: None,
            use_backup: cfg.allow_backup_source,
            // 配置里没填过就用内置备用源，省得用户自己去查网址
            backup_prefix: if cfg.backup_prefix.trim().is_empty() {
                update::DEFAULT_BACKUP_PREFIX.to_owned()
            } else {
                cfg.backup_prefix
            },
            download_failed: false,
            min_speed_kbps: cfg.min_speed_kbps as u64,
            speed_results: Vec::new(),
            speed_testing: false,
            dl_started: None,
            dl_total: 0,
            dl_done: 0,
            ini_changes: Vec::new(),
            icon_textures: HashMap::new(),
        }
    }

    fn spawn<F>(&self, f: F)
    where
        F: FnOnce(Sender<Msg>, egui::Context) + Send + 'static,
    {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || f(tx, ctx));
    }

    /// 重新推断该用哪个入口。选定目录、以及用户点「重新检测」时调用。
    fn redetect(&mut self) {
        let Some(dir) = self.game_dir.clone() else {
            self.advice = None;
            return;
        };
        let advice = scan::advise_proxy(&dir);
        // 判出来了就把下拉框切到推荐项（用户之后仍可手动改）
        if !advice.undetermined {
            self.proxy = advice.recommended.clone();
        }
        self.advice = Some(advice);
    }

    fn set_game_dir(&mut self, dir: PathBuf) {
        // 这是真正要写入的目录，用深度扫描
        self.ac_target = Some(anticheat::scan_deep(&dir));
        self.deploy_state = deploy::state_of(&dir);
        self.manual_path = dir.display().to_string();
        self.game_dir = Some(dir);
        self.redetect();
        self.status = format!(
            "已选择 {}",
            self.game_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        );
    }

    /// 资产当前状态。**只看本地文件 + 下载记录，不查网络。**
    ///
    /// assets 目录由调用方解析一次传进来，避免每一行都去读配置文件。
    ///
    /// 关键：必须先确认文件真的还躺在 assets 目录里。早先这里只比对
    /// update_state.json 里的下载记录，用户把资产文件删光之后，
    /// 界面照样显示「已就绪」—— 记录还在，文件早就没了。
    fn asset_state(&self, assets: Option<&Path>, row: &AssetRow) -> AssetState {
        let local = assets.map(|d| d.join(&row.label));

        if row.runtime_file.is_some() {
            let ok = local
                .as_deref()
                .map(|p| p.is_file() && scan::identify_dll(p) == scan::FileIdentity::Nvidia)
                .unwrap_or(false);
            return if ok {
                AssetState::Ready
            } else {
                AssetState::Missing
            };
        }

        let Some(p) = local else {
            return AssetState::Missing;
        };
        if !p.is_file() {
            return AssetState::Missing;
        }
        let Some(rec) = self.update_state.files.get(&row.label) else {
            // 文件在，但不是本工具下的（用户自己拷进来的）：版本说不清，按需要下载处理
            return AssetState::Outdated;
        };
        // 大小和下载记录对不上 = 被改过，或者当初没下完
        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        if rec.bytes > 0 && size != rec.bytes {
            return AssetState::Outdated;
        }
        match &row.remote_etag {
            Some(remote) if !rec.etag.is_empty() && rec.etag.eq_ignore_ascii_case(remote) => {
                AssetState::Ready
            }
            Some(_) => AssetState::Outdated,
            // 远端指纹没拿到（网络问题）时不要乱报「有更新」
            None => AssetState::Ready,
        }
    }

    fn refresh(&mut self) {
        self.update_state = update::load_state();
        if let Some(d) = self.game_dir.clone() {
            self.deploy_state = deploy::state_of(&d);
            self.ac_target = Some(anticheat::scan_deep(&d));
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Scanned(rows) => {
                self.status = format!("扫描完成，共 {} 个游戏", rows.len());
                self.games = rows;
                self.scanned = true;
                self.busy = false;
            }
            Msg::UpdateChecked(s) => {
                self.status = "更新检查完成".to_owned();
                self.update_summary = Some(s);
                self.update_state = update::load_state();
                self.busy = false;
            }
            Msg::Progress(text, f, total) => {
                self.progress = Some((text, f));
                if total > 0 {
                    // 计时从「真正开始传字节」那一刻起算，别把前面探测文件信息的
                    // 时间算进去 —— 那样算出来的速度会偏低。
                    if self.dl_started.is_none() {
                        self.dl_started = Some(std::time::Instant::now());
                    }
                    self.dl_total = total;
                    self.dl_done = (f * total as f32) as u64;
                }
            }
            Msg::SpeedTested(list) => {
                self.speed_testing = false;
                self.busy = false;
                self.progress = None;
                let mut fastest: Option<(u64, String)> = None;
                for s in &list {
                    match &s.error {
                        None => {
                            self.logs.push(format!("测速 {}：{} KB/s", s.label, s.kbps));
                            if !s.prefix.is_empty()
                                && fastest.as_ref().map(|(k, _)| s.kbps > *k).unwrap_or(true)
                            {
                                fastest = Some((s.kbps, s.prefix.clone()));
                            }
                        }
                        Some(e) => {
                            self.logs.push(format!("测速 {}：不可用（{e}）", s.label))
                        }
                    }
                }
                self.speed_results = list;
                self.status = match fastest {
                    Some((k, p)) => format!(
                        "测速完成，最快的是 {}（{} KB/s），在「下载源」里点一下就能选中",
                        update::source_label(&p),
                        k
                    ),
                    None => "测速完成，但一个能用的源都没测出来".to_owned(),
                };
            }
            Msg::Done(m) => {
                self.logs.push(m.clone());
                self.status = m;
                self.busy = false;
                self.progress = None;
                self.cancel = None;
                self.download_failed = false;
                self.dl_started = None;
                self.refresh();
                // 部署完，如果硬件加速明确是关着的，提示一次（「未知」不提示，
                // 否则 Win11 那些本来就开着的用户每次部署都会被骚扰）
                if self.deploy_in_flight {
                    self.deploy_in_flight = false;
                    if self.hags == gpu::HagsState::Disabled {
                        self.hags_prompt = true;
                    }
                }
            }
            Msg::Failed(e) => {
                self.logs.push(format!("错误: {e}"));
                self.status = format!("错误: {e}");
                self.busy = false;
                self.progress = None;
                self.cancel = None;
            }
            Msg::DownloadFailed(e) => {
                self.logs.push(format!("下载失败: {e}"));
                self.status = format!("下载失败: {e}");
                self.busy = false;
                self.progress = None;
                self.cancel = None;
                self.download_failed = true;
            }
            Msg::GpuOpDone(r) => {
                self.busy = false;
                // 不论成败都重新读一遍，界面上显示的必须是注册表的真实状态
                self.adapters = gpu::enumerate();
                self.driver = gpu::detect_driver(&self.adapters);
                match r {
                    Ok(m) => {
                        self.logs.push(m.clone());
                        self.status = m;
                    }
                    Err(e) => {
                        self.logs.push(format!("显卡名操作失败: {e}"));
                        self.status = format!("显卡名操作失败: {e}");
                    }
                }
            }
            Msg::SelfVersionChecked { latest, manual } => {
                // 只有手动点的那次才会把 busy 立起来，所以也只有它需要放下来
                if manual {
                    self.busy = false;
                }
                match latest {
                    Some(v) if update::is_newer(&v, update::SELF_VERSION) => {
                        let m = format!(
                            "发现新版本 {v}（当前 v{}），点标题栏的「有新版本」去下载",
                            update::SELF_VERSION
                        );
                        self.logs.push(m.clone());
                        self.status = m;
                        self.new_version = Some(v);
                    }
                    Some(v) => {
                        // 远端版本没变，之前那个入口该撤掉
                        self.new_version = None;
                        let m = format!("已是最新版本（当前 v{}，远端 {v}）", update::SELF_VERSION);
                        self.logs.push(m.clone());
                        if manual {
                            self.status = m;
                        }
                    }
                    None => {
                        self.new_version = None;
                        let m = format!(
                            "检查新版本失败（网络问题），不影响使用。当前 v{}",
                            update::SELF_VERSION
                        );
                        self.logs.push(m.clone());
                        if manual {
                            self.status = m;
                        }
                    }
                }
            }
            Msg::Cancelled => {
                // 取消不是失败：清一遍残留（正常都已在下载循环里删干净了），
                // 也别去碰 download_failed，否则会弹出「改用备用源重试」误导用户。
                let n = update::clean_stale_partials();
                let m = if n > 0 {
                    format!("已取消下载（清理了 {n} 个未完成的临时文件）")
                } else {
                    "已取消下载（未留下任何残留）".to_owned()
                };
                self.logs.push(m.clone());
                self.status = m;
                self.busy = false;
                self.progress = None;
                self.cancel = None;
                self.download_failed = false;
            }
        }
    }

    fn save_config(&mut self) {
        let cfg = util::AppConfig {
            asset_dir: util::load_config().asset_dir,
            allow_backup_source: self.use_backup,
            backup_prefix: self.backup_prefix.clone(),
            min_speed_kbps: self.min_speed_kbps as u32,
        };
        if let Err(e) = util::save_config(&cfg) {
            self.logs.push(format!("保存配置失败: {e}"));
        }
    }

    /// 更换资产存放目录。只提示、不自动搬动用户的文件。
    fn change_asset_dir(&mut self, dir: PathBuf) {
        if !util::is_writable(&dir) {
            self.status = format!("这个目录不可写，换一个：{}", dir.display());
            return;
        }
        let old = util::assets_dir().ok();
        let mut cfg = util::load_config();
        cfg.asset_dir = Some(dir.clone());
        cfg.allow_backup_source = self.use_backup;
        cfg.backup_prefix = self.backup_prefix.clone();
        cfg.min_speed_kbps = self.min_speed_kbps as u32;
        if let Err(e) = util::save_config(&cfg) {
            self.status = format!("保存配置失败: {e}");
            return;
        }

        if dir.join(&self.proxy).is_file() {
            self.status = format!("资产目录已改为 {}（该目录已有资产）", dir.display());
        } else {
            self.status =
                format!("资产目录已改为 {}。该目录还没有资产，需要重新下载一次。", dir.display());
        }
        if let Some(o) = old {
            if o != dir {
                self.logs.push(format!("旧资产目录仍保留在 {}，需要的话请手动处理。", o.display()));
            }
        }
    }

    // ---- 后台任务

    fn start_scan(&mut self) {
        self.busy = true;
        self.status = "正在扫描 Steam / Epic 游戏库...".to_owned();
        self.spawn(|tx, ctx| {
            let rows: Vec<GameRow> = scan::scan_all()
                .into_iter()
                .map(|entry| {
                    let render_exe = scan::find_render_exe(&entry.install_dir);
                    // 除了游戏根目录，还要看渲染 EXE 所在目录：
                    // BattlEye 经常埋在 ...\Binaries\Win64\BattlEye，只看根目录会漏
                    let mut rep = anticheat::scan_game_dir(&entry.install_dir);
                    if let Some(dir) = render_exe.as_ref().and_then(|p| p.parent()) {
                        rep.merge(anticheat::scan_game_dir(dir));
                    }
                    let ac = rep.verdict();
                    // 关键：mod 文件在渲染 EXE 目录，不是游戏根目录，
                    // 用根目录判断会一律显示「未部署」
                    let target = render_exe
                        .as_ref()
                        .and_then(|p| p.parent())
                        .map(|d| d.to_path_buf())
                        .unwrap_or_else(|| entry.install_dir.clone());
                    let deployed = deploy::state_of(&target);
                    let icon_img = render_exe.as_deref().and_then(icon::icon_of);
                    GameRow {
                        entry,
                        ac,
                        render_exe,
                        deployed,
                        icon: icon_img,
                    }
                })
                .collect();
            let _ = tx.send(Msg::Scanned(rows));
            ctx.request_repaint();
        });
    }

    fn start_update_check(&mut self) {
        self.busy = true;
        self.status = "正在检查上游更新...".to_owned();
        self.spawn(|tx, ctx| {
            let res = (|| -> anyhow::Result<UpdateSummary> {
                let c = update::client()?;
                let version = update::fetch_version(&c);
                let mut rows: Vec<AssetRow> = Vec::new();

                // 六个文件各发一次 HEAD 到 raw.githubusercontent.com 拿内容指纹。
                // 走的是 CDN，不占 api.github.com 那每小时 60 次的配额 ——
                // 配额被共享出口 IP 吃光正是之前「检查更新 / 下载」失败的原因。
                let specs = [
                    "version.dll".to_owned(),
                    "altnative/winmm.dll".to_owned(),
                    "altnative/dinput8.dll".to_owned(),
                    "altnative/winhttp.dll".to_owned(),
                    "altnative/dxgi.dll".to_owned(),
                    update::INI_REPO_PATH.to_owned(),
                ];
                let mut first_err: Option<String> = None;
                for path in specs {
                    match update::probe_remote(&c, &path) {
                        Ok(r) => {
                            let local = update::local_name(&path);
                            rows.push(AssetRow {
                                group: "核心 Mod",
                                label: local,
                                detail: format!(
                                    "内容指纹 {}",
                                    &r.etag[..r.etag.len().min(10)]
                                ),
                                bytes: r.size,
                                remote_etag: Some(r.etag),
                                runtime_file: None,
                            });
                        }
                        Err(e) => {
                            if first_err.is_none() {
                                first_err = Some(format!("{e}"));
                            }
                        }
                    }
                }
                // 一个都拿不到才算真失败；个别文件缺失不影响其它行显示
                if rows.is_empty() {
                    if let Some(e) = first_err {
                        return Err(anyhow::anyhow!("{e}"));
                    }
                }

                // DLSS 运行库：直接拼 release 直链，再 HEAD 一下问大小，全程不碰 API
                for (_prefix, tag, zip_name, dll_name, label) in update::DLSS_RUNTIME {
                    let url = update::release_url(tag, zip_name);
                    let size = update::probe_url(&c, &url).map(|(n, _)| n).unwrap_or(0);
                    rows.push(AssetRow {
                        group: "DLSS 运行库",
                        label: dll_name.to_owned(),
                        detail: format!("{tag} · {zip_name}（{label}）"),
                        bytes: size,
                        remote_etag: None,
                        runtime_file: Some(dll_name.to_owned()),
                    });
                }
                Ok(UpdateSummary { version, rows })
            })();
            let _ = tx.send(match res {
                Ok(s) => Msg::UpdateChecked(s),
                Err(e) => Msg::Failed(format!("检查更新失败: {e}")),
            });
            ctx.request_repaint();
        });
    }

    fn start_download(&mut self) {
        let proxy = self.proxy.clone();
        let use_backup = self.use_backup && !self.backup_prefix.trim().is_empty();
        let prefix = self.backup_prefix.trim().to_owned();
        let min_kbps = self.min_speed_kbps;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        self.busy = true;
        self.download_failed = false;
        // 速度/剩余时间从零开始算
        self.dl_started = None;
        self.dl_total = 0;
        self.dl_done = 0;
        self.status = if use_backup {
            "正在从备用源下载并校验资产...".to_owned()
        } else {
            "正在下载并校验资产...".to_owned()
        };

        self.spawn(move |tx, ctx| {
            let res = (|| -> anyhow::Result<String> {
                let c = update::client()?;
                let mut state = update::load_state();

                // 先把要下的东西全部探明、算出总字节数，进度条才能按「总进度」走。
                // 否则每换一个文件进度条就回零，看起来像卡住了。
                let _ = tx.send(Msg::Progress("正在获取文件信息...".to_owned(), 0.0, 0));
                ctx.request_repaint();

                struct Item {
                    path: String,
                    local: String,
                    dest: PathBuf,
                    etag: String,
                    size: u64,
                }

                let specs = [
                    update::proxy_repo_path(&proxy).to_owned(),
                    update::INI_REPO_PATH.to_owned(),
                ];
                let mut items: Vec<Item> = Vec::new();
                let mut skipped = 0usize;
                for path in specs {
                    // HEAD 拿期望的内容 SHA-256（不占 API 配额，官方源和镜像都会给）
                    let remote = update::probe_remote(&c, &path)?;
                    let local = update::local_name(&path);
                    let dest = update::asset_path(&local)?;

                    // 已经是最新版就不重下 —— 15 MB 的代理 DLL 没必要每次都拉一遍。
                    // 判定要求「记录在 + 指纹一致 + 文件真的在且大小对」，缺一不可。
                    if update::local_is_current(&state, &local, &dest, &remote.etag) {
                        skipped += 1;
                        continue;
                    }
                    let _ = tx.send(Msg::Progress(
                        format!(
                            "已获取 {local} 信息（{}）",
                            util::format_bytes(remote.size)
                        ),
                        0.0,
                        0,
                    ));
                    ctx.request_repaint();
                    items.push(Item {
                        path,
                        local,
                        dest,
                        etag: remote.etag,
                        size: remote.size,
                    });
                }

                // 运行库那边还要下多少也先问清楚，总字节数才算得准
                let plan = update::dlss_runtime_plan(&c);
                let total: u64 = items.iter().map(|i| i.size).sum::<u64>()
                    + plan.iter().map(|s| s.size).sum::<u64>();
                let steps = items.len() + plan.len();
                let mod_steps = items.len();
                let mut done: u64 = 0;

                for (n, it) in items.into_iter().enumerate() {
                    let tx2 = tx.clone();
                    let ctx2 = ctx.clone();
                    let label = it.local.clone();
                    let step_text = format!("第 {}/{} 步 ·", n + 1, steps);
                    let base = done;
                    let expect = it.size;

                    // 代理 DLL 走镜像优先（快几十倍），但 gh-proxy.com 不转发 ETag，
                    // ETag 比对会落空 —— 所以必须再加一道签名校验：这 5 个 DLL 都由
                    // DLSSG Native Project 自签，镜像伪造不出来。
                    // ini 只有 581 字节，走官方优先：官方会返回 ETag，比对能真正生效。
                    let is_dll = it.path.to_ascii_lowercase().ends_with(".dll");
                    let verifier = move |p: &Path| -> anyhow::Result<()> {
                        if !is_dll {
                            return Ok(());
                        }
                        let id = scan::identify_dll(p);
                        if id.is_ours() {
                            return Ok(());
                        }
                        Err(anyhow::anyhow!(
                            "下载到的文件不是本项目的签名版本（判定为「{}」），已丢弃并换源重试",
                            id.label()
                        ))
                    };

                    let dl = update::download_auto(
                        &c,
                        &it.path,
                        &it.dest,
                        Some(&it.etag),
                        &cancel,
                        &prefix,
                        is_dll,
                        min_kbps,
                        &verifier,
                        &mut move |got, len, src| {
                            let denom = if len > 0 { len } else { expect };
                            // 进度按「总字节」算，不是当前这个文件的百分比 ——
                            // 否则每换一个文件进度条就回零。
                            let f = if total > 0 {
                                ((base + got) as f64 / total as f64).min(1.0) as f32
                            } else {
                                0.0
                            };
                            let _ = tx2.send(Msg::Progress(
                                format!(
                                    "{step_text} 下载 {label} {} / {} · {}",
                                    util::format_bytes(got),
                                    util::format_bytes(denom),
                                    src
                                ),
                                f,
                                total,
                            ));
                            ctx2.request_repaint();
                        },
                    )?;

                    done += dl.bytes;
                    state.files.insert(
                        it.local,
                        update::LocalFile {
                            blob_sha: dl.blob_sha,
                            etag: it.etag.clone(),
                            sha256: dl.sha256,
                            bytes: dl.bytes,
                            downloaded_at: util::now_utc(),
                        },
                    );
                }
                state.version = update::fetch_version(&c);
                update::save_state(&state)?;

                // 再确保两个 DLSS 运行库（下载 -> 解压 -> 删包 -> 校验 NVIDIA 签名）
                {
                    let tx2 = tx.clone();
                    let ctx2 = ctx.clone();
                    let ctx = update::ProgressCtx {
                        base_bytes: done,
                        total_bytes: total,
                        base_step: mod_steps,
                        total_steps: steps,
                    };
                    update::ensure_dlss_runtime(&c, &cancel, &plan, ctx, min_kbps, move |msg, f| {
                        let _ = tx2.send(Msg::Progress(msg, f, total));
                        ctx2.request_repaint();
                    })?;
                }

                Ok(if skipped > 0 {
                    format!(
                        "资产已就绪（{skipped} 个文件本来就是最新版，未重复下载）-> {}",
                        util::assets_dir()?.display()
                    )
                } else {
                    format!("资产已下载并校验完成 -> {}", util::assets_dir()?.display())
                })
            })();
            let _ = tx.send(match res {
                Ok(m) => Msg::Done(m),
                Err(e) => {
                    // 用户点了取消时，错误信息会是 CANCELLED_MSG（也可能是取消标志已置位），
                    // 这不算失败，单独报「已取消」。
                    if cancel.load(Ordering::Relaxed)
                        || e.to_string().contains(update::CANCELLED_MSG)
                    {
                        Msg::Cancelled
                    } else {
                        Msg::DownloadFailed(e.to_string())
                    }
                }
            });
            ctx.request_repaint();
        });
    }

    /// 检查 FrameGen Manager 自己有没有新版本。
    /// 读我们仓库 raw 上的 Cargo.toml，不占任何 API 配额（见 update::fetch_latest_self_version）。
    /// manual = 用户自己点的标题栏「检查更新」。点了得立刻有反应，
    /// 不然就是「点了没动静」；启动时那次则安静地跑，别打断用户。
    fn start_self_update_check(&mut self, manual: bool) {
        if manual {
            self.busy = true;
            self.status = "正在检查新版本…".to_owned();
        }
        self.spawn(move |tx, ctx| {
            // 调试开关：DLSSG_FAKE_NEWVER=0.9.9 可以假装远端有新版本，
            // 用来验证「标题栏出现下载入口」这条路径（不然本地远端同版本看不到）。
            let latest = std::env::var("DLSSG_FAKE_NEWVER").ok().or_else(|| {
                update::client()
                    .ok()
                    .and_then(|c| update::fetch_latest_self_version(&c))
            });
            let _ = tx.send(Msg::SelfVersionChecked { latest, manual });
            ctx.request_repaint();
        });
    }

    /// 把所有候选源各测一遍，把结果列出来让用户自己挑最快的。
    ///
    /// 为什么让用户选而不是程序自动定：镜像快慢是按**用户自己的线路**变的，
    /// 我们这边测出来的名次对他们没有参考价值，只有他们本机测出来的才算数。
    fn start_speed_test(&mut self) {
        if self.speed_testing {
            return;
        }
        self.speed_testing = true;
        self.busy = true;
        self.status = "正在测速（每个源拉 512 KB，最多几秒）...".to_owned();
        self.spawn(|tx, ctx| {
            let cancel = AtomicBool::new(false);
            let list = update::client()
                .map(|c| {
                    update::speed_test_all(&c, &cancel, |msg| {
                        let _ = tx.send(Msg::Progress(msg, 0.0, 0));
                        ctx.request_repaint();
                    })
                })
                .unwrap_or_default();
            let _ = tx.send(Msg::SpeedTested(list));
            ctx.request_repaint();
        });
    }

    fn cancel_download(&mut self) {
        if let Some(c) = &self.cancel {
            c.store(true, Ordering::Relaxed);
            self.status = "正在取消下载...".to_owned();
        }
    }

    /// 部署入口。驱动低于建议版本时先弹一次确认，避免用户白忙一场。
    fn start_deploy(&mut self) {
        if self.driver.as_ref().map(|d| d.too_old()).unwrap_or(false) {
            self.confirm_old_driver = true;
            return;
        }
        // 上游要求「每次只保留本项目的一个代理」。如果目标目录里还躺着本项目的
        // 另一个入口（比如用户手动装过），本工具没有记录可查、也不会自动清 ——
        // 那就先问一句，别让用户以为部署成功了却有两个代理在打架。
        if let Some(dir) = self.game_dir.clone() {
            let extras = deploy::find_extra_own_proxies(&dir, &self.proxy);
            if !extras.is_empty() {
                self.asked_extra_proxies = Some(extras);
                return;
            }
        }
        self.do_deploy(Vec::new());
    }

    fn do_deploy(&mut self, remove_extra: Vec<String>) {
        let Some(dir) = self.game_dir.clone() else {
            self.status = "请先选择游戏目录".to_owned();
            return;
        };

        // 显卡闸门
        match self.gpu_route {
            scan::GpuRoute::Unsupported => {
                self.status =
                    "已阻止部署：检测到非 NVIDIA 显卡，本 Mod 完全不适用（需要 NVIDIA 驱动接口）"
                        .to_owned();
                return;
            }
            scan::GpuRoute::NotNeeded => {
                self.status =
                    "已阻止部署：RTX 40/50 系原生支持 DLSS 帧生成，不需要装本 Mod".to_owned();
                return;
            }
            _ => {}
        }

        // 反作弊闸门：内核级直接拒绝。这里用深度扫描，安全优先。
        let ac = anticheat::scan_deep(&dir);
        let blocked = ac.is_blocked();
        self.ac_target = Some(ac);
        if blocked {
            self.status = "已阻止部署：该游戏检测到内核级反作弊，使用可能导致封号".to_owned();
            return;
        }

        let proxy = self.proxy.clone();
        let dll = update::asset_path(&proxy).unwrap_or_default();
        if !dll.is_file() {
            self.status = format!(
                "缺少 {}，请先在上方「上游资产」点「下载 / 更新资产」",
                proxy
            );
            return;
        }

        // 两个 DLSS 运行库也是必需文件（很多游戏缺了就不生效）
        let mut files = vec![deploy::DeployFile::new(&proxy, dll.clone())];
        for (_prefix, _tag, _zip, dll_name, label) in update::DLSS_RUNTIME {
            let p = update::asset_path(dll_name).unwrap_or_default();
            if !p.is_file() {
                self.status = format!("缺少 {label}（{dll_name}），请先点「下载 / 更新资产」");
                return;
            }
            files.push(deploy::DeployFile::new(dll_name, p));
        }

        // 按显卡准备要部署的 INI（可能被改写过）
        let plan = match update::prepare_deploy_ini(self.gpu_route, self.gpu_name.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("准备 INI 失败: {e}");
                return;
            }
        };
        self.ini_changes = plan.changes.clone();
        files.push(deploy::DeployFile::new(deploy::INI_NAME, plan.path.clone()));

        self.busy = true;
        self.deploy_in_flight = true;
        self.status = "正在部署...".to_owned();
        self.spawn(move |tx, ctx| {
            let r = deploy::deploy(&dir, &proxy, &files, &remove_extra);
            let _ = tx.send(match r {
                Ok(m) => Msg::Done(m),
                Err(e) => Msg::Failed(e.to_string()),
            });
            ctx.request_repaint();
        });
    }

    fn start_restore(&mut self) {
        let Some(dir) = self.game_dir.clone() else {
            self.status = "请先选择游戏目录".to_owned();
            return;
        };
        self.busy = true;
        self.status = "正在还原...".to_owned();
        self.spawn(move |tx, ctx| {
            let r = deploy::restore(&dir);
            let _ = tx.send(match r {
                Ok(m) => Msg::Done(m),
                Err(e) => Msg::Failed(e.to_string()),
            });
            ctx.request_repaint();
        });
    }

    /// 启动提权子进程去改注册表。会弹一次 UAC，用户在弹窗上点「是」才继续。
    fn start_gpu_op(&mut self, op: gpu::Op) {
        let Ok(exe) = std::env::current_exe() else {
            self.status = "无法定位自身可执行文件，操作已取消".to_owned();
            return;
        };
        self.busy = true;
        self.status = "已请求管理员权限，请在弹窗上点「是」...".to_owned();
        self.logs.push(format!(
            "正在{}显卡名称（会弹一次 UAC）",
            if matches!(op, gpu::Op::Restore(_)) {
                "还原"
            } else {
                "修改"
            }
        ));
        self.spawn(move |tx, ctx| {
            let r = gpu::run_elevated(&exe, &op);
            let _ = tx.send(Msg::GpuOpDone(match r {
                Ok(m) => Ok(m),
                Err(e) => Err(format!("{e:#}")),
            }));
            ctx.request_repaint();
        });
    }
}

/// 改显卡名的副作用。界面上必须完整展示，动手前要用户勾选确认。
const SPOOF_WARNINGS: [&str; 8] = [
    "显卡名和硬件 ID（DEV_xxxx）不一致，内核级反作弊可能判定异常 —— 有封号风险，请自行判断。",
    "NVIDIA App / 驱动安装程序可能识别错型号，导致驱动更新或「优化」失败。",
    "重装驱动或大版本更新后会被重置，需要重新设一次。",
    "dxdiag、设备管理器、任务管理器里显示的显卡名都会跟着变。",
    "不保证一定生效：如果游戏改用硬件 ID 判断型号，改名没有任何作用。",
    "本工具只提供固定型号名单，避免填错；填了不存在的型号可能让游戏崩溃或拒绝运行。",
    "部分按型号生效的 NVIDIA 功能（DLSS 覆盖、控制面板选项）可能受影响。",
    "需要重启才生效；出问题可以随时点「还原」，原始值在改之前就已经备份好了。",
];

/// 用资源管理器打开目录。注意 explorer.exe 成功时也常返回非 0 退出码，所以不检查状态。
fn open_in_explorer(path: &Path) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

/// 部署状态 -> 颜色
fn deploy_color(s: &deploy::DeployState) -> egui::Color32 {
    match s {
        deploy::DeployState::Deployed { .. } => theme::OK,
        // 已安装但非本工具部署：也是装好了，用绿色，靠文字区分
        deploy::DeployState::ManuallyInstalled { .. } => theme::OK,
        deploy::DeployState::Occupied { .. } => theme::WARN,
        deploy::DeployState::NotDeployed => theme::NEUTRAL,
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(m) = self.rx.try_recv() {
            self.handle(m);
        }
        // 用户去系统设置里看完/改完再切回来 —— 这时重读一次硬件加速状态。
        // 只在「刚获得焦点」那一帧读，不是每帧都读注册表。
        let focused = self.ctx.input(|i| i.focused);
        if focused && !self.was_focused {
            self.hags = self.hags_fake.unwrap_or_else(gpu::hags_state);
        }
        self.was_focused = focused;
        if self.busy {
            self.ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }

        // 调试开关：只为截图/排查用，正常启动不受影响
        if !self.autoscan_done && std::env::var_os("DLSSG_AUTOSCAN").is_some() {
            self.autoscan_done = true;
            self.start_scan();
        }
        // 调试开关：启动就跑一次测速，省得脚本去点按钮
        if !self.autospeed_done && std::env::var_os("DLSSG_AUTOSPEED").is_some() {
            self.autospeed_done = true;
            self.start_speed_test();
        }

        // 启动时检查「本软件」有没有新版本，默认就开，界面上不设开关。
        // 注意这不是上游 Mod 的更新检查 —— 那个仍然只在你点资产卡片里的
        // 「检查更新」时才跑，两者是两回事，所以入口也不放在一起。
        if !self.autocheck_done {
            self.autocheck_done = true;
            self.start_self_update_check(false);
        }

        // ---------------- 顶栏
        egui::Panel::top("header")
            .frame(
                egui::Frame::NONE
                    .fill(theme::BG_CARD)
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("FrameGen Manager")
                            .size(20.0)
                            .color(theme::TEXT)
                            .strong(),
                    );
                    ui.add_space(4.0);
                    ui.label(theme::hint("为 RTX 20 / 30 系一键部署 DLSS 帧生成"));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // 本软件有新版本 —— 放最右边，最显眼
                        if let Some(v) = self.new_version.clone() {
                            if theme::primary_button(ui, &format!("有新版本 {v}"), true)
                                .on_hover_text("点一下打开 GitHub 发布页下载新版")
                                .clicked()
                            {
                                if let Err(e) = util::open_url(update::RELEASES_URL) {
                                    self.status = format!("打开发布页失败: {e}");
                                }
                            }
                            ui.add_space(6.0);
                        }
                        // 本软件的更新检查。放标题栏，和资产卡片里那个「检查 Mod 更新」
                        // 从位置上就分开，免得被当成上游 Mod 的更新。
                        if theme::ghost_button(ui, "检查更新", !self.busy)
                            .on_hover_text(format!(
                                "检查 FrameGen Manager 自己有没有新版本（当前 v{}）",
                                update::SELF_VERSION
                            ))
                            .clicked()
                        {
                            self.start_self_update_check(true);
                        }
                        ui.add_space(2.0);
                        ui.label(theme::hint(format!("v{}", update::SELF_VERSION)));
                        ui.add_space(6.0);
                        let version = self
                            .update_state
                            .version
                            .clone()
                            .unwrap_or_else(|| "未检查".to_owned());
                        theme::badge(ui, &format!("上游 {version}"), theme::NEUTRAL);
                        ui.add_space(6.0);
                        let t = self.ac_system.verdict();
                        theme::badge(
                            ui,
                            &format!("系统反作弊 {}", t.label()),
                            theme::tier_color(t),
                        );
                    });
                });
            });

        // ---------------- 底栏
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::NONE
                    .fill(theme::BG_CARD)
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.busy {
                        ui.add(egui::Spinner::new().size(13.0));
                    } else {
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), 3.5, theme::OK);
                    }
                    ui.label(
                        egui::RichText::new(&self.status)
                            .size(12.5)
                            .color(theme::TEXT_MUTED),
                    );
                });
            });

        // ---------------- 左侧操作区（卡片流）
        egui::Panel::left("actions")
            .frame(
                egui::Frame::NONE
                    .fill(theme::BG_APP)
                    .inner_margin(egui::Margin::symmetric(12, 0)),
            )
            .default_size(364.0)
            .show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(10.0);
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);

                // --- 目标目录
                theme::card(ui, |ui| {
                    theme::card_title(ui, "目标目录");
                    let path = self.game_dir.clone();
                    ui.label(match &path {
                        Some(p) => theme::path_text(p.display().to_string()),
                        None => egui::RichText::new("（未选择）")
                            .size(12.0)
                            .color(theme::TEXT_MUTED),
                    });
                    ui.horizontal(|ui| {
                        if theme::ghost_button(ui, "选择目录...", !self.busy).clicked() {
                            if let Some(dir) = rfd::FileDialog::new()
                                .set_title("选择游戏渲染 EXE 所在目录")
                                .pick_folder()
                            {
                                self.set_game_dir(dir);
                            }
                        }
                        if theme::ghost_button(ui, "打开文件夹", path.is_some()).clicked() {
                            if let Some(p) = &path {
                                open_in_explorer(p);
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.manual_path)
                                .desired_width(176.0)
                                .hint_text("或手动粘贴路径"),
                        );
                        if theme::ghost_button(ui, "应用", true).clicked() {
                            let p = PathBuf::from(self.manual_path.trim());
                            if p.is_dir() {
                                self.set_game_dir(p);
                            } else {
                                self.status = "路径不存在".to_owned();
                            }
                        }
                    });
                });

                // --- 上游资产（放在部署上方，因为必须先把资产下下来）
                theme::card(ui, |ui| {
                    theme::card_title(ui, "上游资产");

                    // 显卡与路由
                    // 先克隆出来，后面在闭包里用，避免和 self 的可变借用打架
                    let driver_badge: Option<(String, bool)> = self
                        .driver
                        .as_ref()
                        .map(|d| (d.marketing.clone(), d.too_old()));
                    match self.gpu_name.clone() {
                        Some(n) => {
                            let (txt, col) = match self.gpu_route {
                                scan::GpuRoute::Sm86 => ("SM86 路由", theme::OK),
                                scan::GpuRoute::Sm75 => ("需改 SM75", theme::WARN),
                                scan::GpuRoute::NotNeeded => ("不需要本 Mod", theme::WARN),
                                scan::GpuRoute::Unsupported => ("不适用", theme::DANGER),
                                scan::GpuRoute::Unknown => ("未识别", theme::NEUTRAL),
                            };
                            ui.horizontal(|ui| {
                                theme::badge(ui, txt, col);
                                ui.label(theme::hint(n));
                                match &driver_badge {
                                    Some((ver, true)) => {
                                        theme::badge(
                                            ui,
                                            &format!("驱动 {ver} 过旧"),
                                            theme::DANGER,
                                        );
                                    }
                                    Some((ver, false)) => {
                                        theme::badge(
                                            ui,
                                            &format!("驱动 {ver}"),
                                            theme::OK,
                                        );
                                    }
                                    None => {
                                        theme::badge(ui, "驱动版本未知", theme::NEUTRAL);
                                    }
                                }
                            });
                        }
                        None => {
                            ui.label(theme::hint(
                                "读不到显卡信息，将按上游默认 SM86 处理，请自行确认。",
                            ));
                        }
                    }
                    // 驱动过旧：这里就给红字，不用等用户点到部署
                    if let Some((ver, true)) = &driver_badge {
                        let mut go_driver_page = false;
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "驱动 {ver} 低于 {}，帧生成可能不生效，建议先更新显卡驱动。",
                                    gpu::MIN_FG_DRIVER_TEXT
                                ))
                                .size(11.5)
                                .color(theme::DANGER),
                            );
                            if theme::ghost_button(ui, "打开驱动下载页", true).clicked() {
                                go_driver_page = true;
                            }
                        });
                        if go_driver_page {
                            if let Err(e) = util::open_url(gpu::DRIVER_URL) {
                                self.status = format!("打开驱动下载页失败: {e}");
                            }
                        }
                    }

                    // 硬件加速 GPU 计划：和驱动版本同一个性质（帧生成的系统前提），
                    // 所以放在一起。只读显示 + 一个跳转按钮，程序不碰系统设置。
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("硬件加速 GPU 计划")
                                .size(12.0)
                                .color(theme::TEXT_MUTED),
                        );
                        let color = match self.hags {
                            gpu::HagsState::Enabled => theme::OK,
                            gpu::HagsState::Disabled => theme::DANGER,
                            gpu::HagsState::Unknown => theme::NEUTRAL,
                        };
                        theme::badge(ui, self.hags.label(), color);
                        if theme::ghost_button(ui, "去设置", true)
                            .on_hover_text("打开 Windows 设置里「硬件加速 GPU 计划」那一页")
                            .clicked()
                        {
                            if let Err(e) = gpu::open_hags_settings() {
                                self.status = format!("打开系统设置失败: {e}");
                            }
                        }
                    });
                    match self.hags {
                        gpu::HagsState::Unknown => {
                            ui.label(theme::hint("Win11 默认开启，想确认请点「去设置」"));
                        }
                        gpu::HagsState::Disabled => {
                            ui.label(
                                egui::RichText::new(
                                    "DLSS 帧生成要求这一项开启，关着的话可能不生效。",
                                )
                                .size(11.5)
                                .color(theme::DANGER),
                            );
                        }
                        gpu::HagsState::Enabled => {}
                    }

                    // 入口推荐
                    ui.add_space(2.0);
                    match self.advice.clone() {
                        None => {
                            ui.label(theme::hint("选择游戏目录后会自动判断该用哪个代理入口。"));
                        }
                        Some(a) => {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("推荐入口")
                                        .size(12.0)
                                        .color(theme::TEXT_MUTED),
                                );
                                ui.label(
                                    egui::RichText::new(&a.recommended)
                                        .size(13.0)
                                        .color(theme::TEXT)
                                        .strong(),
                                );
                                if a.undetermined {
                                    theme::badge(ui, "未能自动判定", theme::WARN);
                                }
                            });
                            ui.label(theme::hint(format!(
                                "依据：{}（已分析 {} 个模块）",
                                a.reason, a.scanned
                            )));
                            if a.undetermined {
                                ui.label(theme::hint(
                                    "用的是上游默认入口。如果游戏里没生效，换个入口重新部署试试。",
                                ));
                            }
                            // 「用户是不是已经手动装过本项目」的判定结果
                            for e in &a.own_existing {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "✓ 目录里已有本项目的 {}（{}）— 是本项目文件，可直接覆盖",
                                        e.name,
                                        util::format_bytes(e.bytes)
                                    ))
                                    .size(11.5)
                                    .color(theme::OK),
                                );
                            }
                            for e in &a.occupied {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "! 目录里已有 {}（{}，{}）— 不是本项目文件，已跳过",
                                        e.name,
                                        util::format_bytes(e.bytes),
                                        e.identity.label()
                                    ))
                                    .size(11.5)
                                    .color(theme::WARN),
                                );
                            }
                        }
                    }
                    if theme::ghost_button(ui, "重新检测入口", !self.busy).clicked() {
                        self.redetect();
                        // 明确反馈：否则用户以为点了没反应（尤其是结果没变化时）
                        let msg = match &self.advice {
                            None => "请先选择游戏目录，再点重新检测".to_owned(),
                            Some(a) if a.undetermined => format!(
                                "已重新检测：未能自动判定，暂用上游默认 {}（{}）",
                                a.recommended, a.reason
                            ),
                            Some(a) => format!(
                                "已重新检测：推荐 {}（{}）",
                                a.recommended, a.reason
                            ),
                        };
                        self.logs.push(msg.clone());
                        self.status = msg;
                    }

                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(4.0);

                    // 资产目录
                    ui.label(egui::RichText::new("资产目录").size(12.0).color(theme::TEXT_MUTED));
                    let adir = util::assets_dir()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| "未知".to_owned());
                    ui.label(theme::path_text(&adir));
                    ui.horizontal(|ui| {
                        if theme::ghost_button(ui, "打开文件夹", true).clicked() {
                            if let Ok(p) = util::assets_dir() {
                                open_in_explorer(&p);
                            }
                        }
                        if theme::ghost_button(ui, "更改位置...", !self.busy).clicked() {
                            if let Some(d) = rfd::FileDialog::new()
                                .set_title("选择资产存放目录")
                                .pick_folder()
                            {
                                self.change_asset_dir(d);
                            }
                        }
                    });

                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(4.0);

                    ui.label(theme::hint(
                        "上游没有 GitHub Releases，版本以 main 分支文件的 git blob sha 为准。",
                    ));

                    // 下载 / 取消
                    ui.horizontal(|ui| {
                        if self.cancel.is_some() {
                            if theme::danger_button(ui, "取消下载", true).clicked() {
                                self.cancel_download();
                            }
                        } else if theme::primary_button(ui, "下载 / 更新资产", !self.busy).clicked() {
                            self.start_download();
                        }
                        if theme::ghost_button(ui, "检查更新", !self.busy).clicked() {
                            self.start_update_check();
                        }
                    });

                    // ---- 下载源：先测速，再把结果摆出来让用户自己挑最快的
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("下载源").size(12.5).color(theme::TEXT));
                        if theme::ghost_button(ui, "测速", !self.busy)
                            .on_hover_text("对每个源各拉 512 KB，实测你这条线路上的真实速度")
                            .clicked()
                        {
                            self.start_speed_test();
                        }
                        if self.speed_testing {
                            ui.add(egui::Spinner::new().size(12.0));
                        }
                        if self.use_backup {
                            theme::badge(ui, "已指定", theme::WARN);
                        }
                    });

                    // 没测过速就把内置镜像先列出来，照样能选
                    let rows: Vec<(String, String, String)> = if self.speed_results.is_empty() {
                        update::MIRRORS
                            .iter()
                            .map(|m| ((*m).to_owned(), update::source_label(m), "未测速".to_owned()))
                            .collect()
                    } else {
                        self.speed_results
                            .iter()
                            .filter(|s| !s.prefix.is_empty())
                            .map(|s| {
                                (
                                    s.prefix.clone(),
                                    s.label.clone(),
                                    match &s.error {
                                        Some(_) => "不可用".to_owned(),
                                        None => format!("{} KB/s", s.kbps),
                                    },
                                )
                            })
                            .collect()
                    };
                    let current = if self.use_backup {
                        self.backup_prefix.trim().to_owned()
                    } else {
                        String::new()
                    };
                    let mut choice = current.clone();
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(&mut choice, String::new(), "自动");
                        for (prefix, label, speed) in &rows {
                            ui.selectable_value(
                                &mut choice,
                                prefix.clone(),
                                format!("{label}  {speed}"),
                            );
                        }
                    });
                    if choice != current {
                        self.use_backup = !choice.is_empty();
                        if !choice.is_empty() {
                            self.backup_prefix = choice.clone();
                        }
                        self.save_config();
                        self.status = if choice.is_empty() {
                            "已改为自动选源（按实测速率挑最快的）".to_owned()
                        } else {
                            format!("已选中下载源 {}", update::source_label(&choice))
                        };
                    }
                    // 官方源单独列出来：让用户看见为什么默认不用它
                    for s in &self.speed_results {
                        if !s.prefix.is_empty() {
                            continue;
                        }
                        let txt = match &s.error {
                            Some(e) => format!("{}：不可用（{e}）", s.label),
                            None => format!("{}：{} KB/s", s.label, s.kbps),
                        };
                        ui.label(theme::hint(txt));
                    }
                    ui.label(theme::hint(
                        "选中的源排最前面，其余镜像仍会兜底；下载中低于阈值会自动换源。",
                    ));
                    ui.collapsing("填自己的源地址（高级）", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.backup_prefix)
                                .desired_width(260.0)
                                .hint_text("https://xxx/"),
                        );
                        if theme::ghost_button(ui, "用这个源", true).clicked() {
                            self.use_backup = !self.backup_prefix.trim().is_empty();
                            self.save_config();
                            self.status = "下载源已保存".to_owned();
                        }
                        ui.label(theme::hint(
                            "前缀会拼在官方地址前面。填错也没关系，内容对不上会被自动拒绝。",
                        ));
                    });

                    // 进度条就放在按钮下面，速度/剩余时间单独一行
                    if let Some((text, f)) = self.progress.clone() {
                        ui.add(egui::ProgressBar::new(f).text(text));
                        if let (Some(t0), true) = (self.dl_started, self.dl_total > 0) {
                            let el = t0.elapsed().as_secs_f64();
                            if el >= 1.5 && self.dl_done > 0 {
                                let bps = self.dl_done as f64 / el;
                                let left = self.dl_total.saturating_sub(self.dl_done) as f64;
                                let eta = if bps > 1.0 { left / bps } else { 0.0 };
                                let eta_txt = if eta < 60.0 {
                                    format!("{eta:.0} 秒")
                                } else {
                                    format!("{:.0} 分 {:.0} 秒", (eta / 60.0).floor(), eta % 60.0)
                                };
                                ui.label(theme::hint(format!(
                                    "{} / {} · {:.2} MB/s · 剩余约 {}",
                                    util::format_bytes(self.dl_done),
                                    util::format_bytes(self.dl_total),
                                    bps / 1024.0 / 1024.0,
                                    eta_txt
                                )));
                            }
                        }
                    }

                    // 下载失败 -> 一键改用备用源重试（内置地址，不用用户填）
                    if self.download_failed {
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new("官方源下载失败（国内网络常见，通常是间歇性的）。")
                                .size(12.0)
                                .color(theme::DANGER),
                        );
                        let can_retry = !self.busy && self.cancel.is_none();
                        if theme::primary_button(ui, "改用备用源重试", can_retry)
                            .on_hover_text(
                                "用内置的加速镜像重新下载。内容仍会用 git blob sha 校验，镜像返回错误内容会被自动拒绝。",
                            )
                            .clicked()
                        {
                            self.use_backup = true;
                            self.save_config();
                            self.start_download();
                        }
                        ui.label(theme::hint(format!("当前备用源：{}", self.backup_prefix)));
                        ui.collapsing("想换个备用源地址", |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.backup_prefix)
                                    .desired_width(260.0)
                                    .hint_text("https://xxx/"),
                            );
                            if theme::ghost_button(ui, "保存", true).clicked() {
                                self.save_config();
                                self.status = "备用源设置已保存".to_owned();
                            }
                            ui.label(theme::hint(
                                "前缀会拼在官方地址前面。填错也没关系，内容对不上会被自动拒绝。",
                            ));
                        });
                    }

                    // 资产清单：按「核心 Mod / DLSS 运行库」分组显示
                    if let Some(s) = self.update_summary.clone() {
                        // 资产目录只解析一次，别在每一行里重复读配置文件
                        let assets = util::assets_dir().ok();
                        ui.add_space(2.0);
                        if let Some(v) = &s.version {
                            ui.label(theme::hint(format!("上游版本 {v}")));
                        }
                        for group in ["核心 Mod", "DLSS 运行库"] {
                            let group_rows: Vec<AssetRow> = s
                                .rows
                                .iter()
                                .filter(|r| r.group == group)
                                .cloned()
                                .collect();
                            if group_rows.is_empty() {
                                continue;
                            }
                            ui.add_space(3.0);
                            ui.label(
                                egui::RichText::new(group)
                                    .size(11.5)
                                    .color(theme::TEXT)
                                    .strong(),
                            );
                            for row in group_rows {
                                let st = self.asset_state(assets.as_deref(), &row);
                                ui.horizontal(|ui| {
                                    theme::badge(ui, st.label(), st.color());
                                    ui.label(theme::hint(format!(
                                        "{}  ·  {}",
                                        row.label,
                                        util::format_bytes(row.bytes)
                                    )));
                                });
                                ui.label(
                                    egui::RichText::new(format!("     {}", row.detail))
                                        .size(10.5)
                                        .color(theme::TEXT_MUTED),
                                );
                            }
                        }
                    }
                });

                // --- 部署
                theme::card(ui, |ui| {
                    theme::card_title(ui, "部署");
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("代理入口").size(12.0).color(theme::TEXT_MUTED));
                        // 必须给唯一 salt：from_label("") 会用空字符串当 id，
                        // 和界面上别的下拉撞 id 就会点不动
                        egui::ComboBox::from_id_salt("proxy-entry")
                            .selected_text(self.proxy.clone())
                            .show_ui(ui, |ui| {
                                for p in scan::PROXY_PRIORITY {
                                    ui.selectable_value(&mut self.proxy, p.to_owned(), p);
                                }
                            });
                    });
                    theme::badge(ui, &self.deploy_state.label(), deploy_color(&self.deploy_state));

                    // 显卡闸门
                    match self.gpu_route {
                        scan::GpuRoute::Unsupported => {
                            ui.label(
                                egui::RichText::new(
                                    "本机是非 NVIDIA 显卡，本 Mod 不适用，已禁止部署。",
                                )
                                .size(12.0)
                                .color(theme::DANGER),
                            );
                        }
                        scan::GpuRoute::NotNeeded => {
                            ui.label(
                                egui::RichText::new("RTX 40/50 系原生支持帧生成，不需要装本 Mod。")
                                    .size(12.0)
                                    .color(theme::WARN),
                            );
                        }
                        _ => {}
                    }

                    // 部署前预告会改 INI
                    if self.gpu_route == scan::GpuRoute::Sm75 {
                        ui.label(
                            egui::RichText::new(
                                "⚠ 部署时会自动把 INI 的 Router 改为 SM75（你的显卡属于 Turing/SM75）。",
                            )
                            .size(11.5)
                            .color(theme::WARN),
                        );
                    }
                    // 部署后展示实际改动
                    for c in &self.ini_changes {
                        ui.label(
                            egui::RichText::new(format!("⚠ {c}"))
                                .size(11.5)
                                .color(theme::WARN),
                        );
                    }

                    // 缺资产提示
                    let has_asset = util::assets_dir()
                        .map(|d| d.join(&self.proxy).is_file())
                        .unwrap_or(false);
                    if !has_asset && self.game_dir.is_some() {
                        ui.label(
                            egui::RichText::new(format!(
                                "本地还没有 {}，请先点上方「下载 / 更新资产」。",
                                self.proxy
                            ))
                            .size(12.0)
                            .color(theme::WARN),
                        );
                    }

                    // 驱动过旧警告：放在按钮正上方，免得点完才发现白忙一场
                    if let Some(d) = self.driver.as_ref().filter(|d| d.too_old()) {
                        ui.label(
                            egui::RichText::new(format!(
                                "⚠ 当前驱动 {} 低于 {}，帧生成可能不生效（点「部署」时会再确认一次）。",
                                d.marketing,
                                gpu::MIN_FG_DRIVER_TEXT
                            ))
                            .size(11.5)
                            .color(theme::DANGER),
                        );
                    }

                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        let can = !self.busy && self.game_dir.is_some();
                        if theme::primary_button(ui, "部署", can)
                            .on_hover_text("把 4 个文件部署到上面这个目录，并先备份被覆盖的原文件")
                            .clicked()
                        {
                            self.start_deploy();
                        }
                        // 只有本工具部署过（有 manifest 备份记录）才能一键还原
                        let can_restore =
                            can && matches!(self.deploy_state, deploy::DeployState::Deployed { .. });
                        if theme::danger_button(ui, "还原", can_restore)
                            .on_hover_text(if can_restore {
                                "撤回本工具部署的全部文件，并恢复部署前的原文件"
                            } else {
                                "没有本工具的部署记录，无法还原"
                            })
                            .clicked()
                        {
                            self.start_restore();
                        }
                    });

                    // 把「还原」到底做什么讲清楚
                    match &self.deploy_state {
                        deploy::DeployState::Deployed { .. } => {
                            ui.label(theme::hint(
                                "「还原」= 撤回本工具部署的全部 4 个文件（代理 DLL + INI + 两个 DLSS 运行库），并恢复部署前备份的原文件。不是只删 version.dll。",
                            ));
                        }
                        deploy::DeployState::ManuallyInstalled { .. } => {
                            ui.label(
                                egui::RichText::new(
                                    "这些文件是手动安装的，没有本工具的备份记录，所以「还原」不可用；要继续用就点「部署」（会先备份再覆盖），想卸载请自己删除。",
                                )
                                .size(11.5)
                                .color(theme::WARN),
                            );
                        }
                        _ => {}
                    }
                });

                // --- 反作弊
                theme::card(ui, |ui| {
                    theme::card_title(ui, "反作弊检查");
                    match &self.ac_target {
                        None => {
                            ui.label(theme::hint("选择目录后自动检测。"));
                        }
                        Some(r) => {
                            let t = r.verdict();
                            ui.horizontal(|ui| {
                                theme::badge(ui, t.label(), theme::tier_color(t));
                                ui.label(theme::hint(format!("命中 {} 项", r.hits.len())));
                            });
                            if t == AcTier::Kernel {
                                ui.label(
                                    egui::RichText::new("已禁止部署：内核级反作弊可能因修改游戏文件而封号。")
                                        .size(12.0)
                                        .color(theme::DANGER),
                                );
                            }
                            for h in &r.hits {
                                ui.label(theme::hint(format!("· {} — {}", h.name, h.evidence)));
                            }
                        }
                    }
                });

                // --- 显卡名称伪装（高级 · 谨慎）
                theme::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(3.0, 15.0), egui::Sense::hover());
                        ui.painter()
                            .rect_filled(rect, egui::CornerRadius::same(1), theme::WARN);
                        ui.label(
                            egui::RichText::new("显卡名称伪装（高级 · 谨慎）")
                                .size(14.0)
                                .color(theme::TEXT)
                                .strong(),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if theme::ghost_button(
                                    ui,
                                    if self.spoof_open { "收起" } else { "展开" },
                                    true,
                                )
                                .clicked()
                                {
                                    self.spoof_open = !self.spoof_open;
                                }
                            },
                        );
                    });

                    // 找到要操作的那块 NVIDIA 显卡；找不到就整个功能禁用
                    let Some(a) = gpu::primary(&self.adapters).cloned() else {
                        ui.label(theme::hint(
                            "没有找到 NVIDIA 显卡的注册表实例，这个功能在本机不可用。",
                        ));
                        return;
                    };

                    // 状态行收起时也可见：一眼看出名字有没有被改过
                    ui.horizontal(|ui| {
                        if a.spoofed() {
                            theme::badge(ui, "已伪装", theme::WARN);
                        } else {
                            theme::badge(ui, "未伪装", theme::NEUTRAL);
                        }
                        ui.label(theme::hint(format!(
                            "当前显示「{}」，驱动记录为「{}」",
                            a.current_name().unwrap_or_else(|| "(空)".to_owned()),
                            a.driver_name
                        )));
                    });

                    if !self.spoof_open {
                        return;
                    }

                    ui.add_space(6.0);
                    ui.label(theme::hint(
                        "只有在某个游戏的「帧生成」选项不出现、并且已经按上面的步骤正常部署过时，才建议尝试。这个功能不是必须的。",
                    ));

                    ui.add_space(6.0);
                    theme::warn_box(ui, |ui| {
                        ui.label(
                            egui::RichText::new("动手前请先读完（点「应用伪装」即代表你已了解）：")
                                .size(12.0)
                                .color(theme::WARN)
                                .strong(),
                        );
                        ui.add_space(2.0);
                        for w in SPOOF_WARNINGS {
                            ui.label(
                                egui::RichText::new(format!("· {w}"))
                                    .size(11.0)
                                    .color(theme::TEXT),
                            );
                        }
                    });

                    ui.add_space(6.0);
                    ui.label(theme::hint("本工具只会改这一个注册表值，其它一律不碰："));
                    ui.label(theme::path_text(format!(
                        "HKLM\\{}\\DeviceDesc",
                        a.enum_key
                    )));
                    if let Some(d) = &a.device_desc {
                        ui.label(theme::path_text(format!("    现在的值：{d}")));
                    }

                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("伪装成（点一下选中）")
                            .size(12.0)
                            .color(theme::TEXT_MUTED),
                    );
                    // 这里故意不用下拉框：下拉是弹层，在滚动区域里容易点不动，
                    // 而且 from_label("") 会和页面上别的下拉共用同一个 id。
                    // 单选按钮就在当前布局里，最稳。
                    // 固定 3 + 3 两行：交给 horizontal_wrapped 自动换行会排成 5 + 1，
                    // 最后一项孤零零占一行，看着像出了错。
                    // 用 selectable_value 而不是 radio_value：radio 被选中时只多画一个
                    // 小圆点（实测整行只有 32 个像素变化），看着像没点动；
                    // selectable_value 选中后整个选项底色变绿，一眼可见，点击区域也更大。
                    ui.horizontal(|ui| {
                        for p in &gpu::PRESETS[..3] {
                            let short = p.trim_start_matches("NVIDIA GeForce ");
                            ui.selectable_value(&mut self.spoof_target, (*p).to_owned(), short);
                        }
                    });
                    ui.horizontal(|ui| {
                        for p in &gpu::PRESETS[3..] {
                            let short = p.trim_start_matches("NVIDIA GeForce ");
                            ui.selectable_value(&mut self.spoof_target, (*p).to_owned(), short);
                        }
                    });
                    ui.label(theme::hint(format!("已选中：{}", self.spoof_target)));

                    ui.add_space(4.0);
                    // 不用 ui.checkbox：egui 0.36 勾选后只是在 8px 的小方框里画一条
                    // 1 像素宽的细对勾，方框底色完全不变。实测勾上前后整行只差 32 个
                    // 像素，肉眼几乎看不出勾没勾上 —— 用户会以为「点了没反应」。
                    // toggle_value 选中时整行变绿，状态一眼可见，而且点击区域大得多。
                    let ack_text = if self.spoof_ack {
                        "已勾选：我已阅读并理解上面的副作用（再点一次取消）"
                    } else {
                        "点这里勾选：我已阅读并理解上面的副作用"
                    };
                    ui.toggle_value(&mut self.spoof_ack, ack_text);

                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let can = !self.busy && self.spoof_ack;
                        if theme::primary_button(ui, "应用伪装", can)
                            .on_hover_text(if self.spoof_ack {
                                "先备份原值并读回校验，再改注册表；需要管理员权限（弹一次 UAC）"
                            } else {
                                "请先勾选上面的「我已阅读并理解」"
                            })
                            .clicked()
                        {
                            self.spoof_pending =
                                Some(gpu::Op::Apply(self.spoof_target.clone()));
                        }
                    });

                    // 还原有两种目标，差别很大，所以拆成两个按钮，别让用户猜
                    let bk_orig = gpu::backup_original_of(&a);
                    ui.horizontal(|ui| {
                        let can_driver = !self.busy && a.spoofed();
                        if theme::danger_button(ui, "还原为驱动记录的名称", can_driver)
                            .on_hover_text(if can_driver {
                                "写回驱动自己记录的名称，等于彻底去掉伪装"
                            } else {
                                "当前没有被改过，不需要还原"
                            })
                            .clicked()
                        {
                            self.spoof_pending =
                                Some(gpu::Op::Restore(gpu::RestoreTo::DriverName));
                        }
                        // 备份值要和「驱动记录名」和「当前显示名」都不同，还原才有意义
                        let cur_name = a.current_name().unwrap_or_default();
                        let can_backup = !self.busy
                            && bk_orig
                                .as_deref()
                                .map(|o| {
                                    !o.eq_ignore_ascii_case(&a.driver_name)
                                        && !o.eq_ignore_ascii_case(&cur_name)
                                })
                                .unwrap_or(false);
                        if theme::ghost_button(ui, "还原为改动前的值", can_backup)
                            .on_hover_text(match &bk_orig {
                                Some(o) if can_backup => format!(
                                    "写回本工具第一次改动之前的值：{}",
                                    gpu::display_name(o)
                                ),
                                _ => "没有可用的备份记录，或备份值就是驱动记录的名称".to_owned(),
                            })
                            .clicked()
                        {
                            self.spoof_pending =
                                Some(gpu::Op::Restore(gpu::RestoreTo::BackupOriginal));
                        }
                    });

                    ui.label(theme::hint(match gpu::backup_path() {
                        Ok(p) => format!("备份位置：{}", p.display()),
                        Err(_) => "备份位置：无法确定".to_owned(),
                    }));
                    ui.label(theme::hint(
                        "生效需要重启。重启后 dxdiag 的「Card name」应该变成你选的型号。",
                    ));
                });

                // --- 操作日志
                theme::card(ui, |ui| {
                    theme::card_title(ui, "操作日志");
                    if self.logs.is_empty() {
                        ui.label(theme::hint("暂无操作记录。"));
                    }
                    for l in self.logs.iter().rev().take(6) {
                        ui.label(theme::hint(l.clone()));
                    }
                });

                ui.add_space(10.0);
            });
        });

        // 把还没上传的游戏图标上传成纹理（只做一次）
        let mut pending_icons: Vec<(String, usize, usize, Vec<u8>)> = Vec::new();
        for row in &self.games {
            let key = row.entry.install_dir.display().to_string();
            if self.icon_textures.contains_key(&key) {
                continue;
            }
            if let Some(ic) = &row.icon {
                pending_icons.push((key, ic.width, ic.height, ic.rgba.clone()));
            }
        }
        for (key, w, h, rgba) in pending_icons {
            let img = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
            let tex = self
                .ctx
                .load_texture(key.clone(), img, egui::TextureOptions::LINEAR);
            self.icon_textures.insert(key, tex);
        }

        // ---------------- 中央：游戏库卡片列表
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(theme::BG_APP)
                    .inner_margin(egui::Margin::symmetric(14, 12)),
            )
            .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("游戏库")
                        .size(17.0)
                        .color(theme::TEXT)
                        .strong(),
                );
                ui.add_space(6.0);
                if theme::ghost_button(ui, "扫描 Steam / Epic", !self.busy).clicked() {
                    self.start_scan();
                }
                if self.scanned {
                    theme::badge(ui, &format!("{} 个", self.games.len()), theme::NEUTRAL);
                }
            });
            ui.add_space(10.0);

            if !self.scanned {
                ui.label(theme::hint(
                    "点「扫描 Steam / Epic」列出已安装游戏。启动时不扫描，也不会后台轮询。",
                ));
                return;
            }

            let mut pick: Option<PathBuf> = None;
            let mut open: Option<PathBuf> = None;

            egui::ScrollArea::vertical().show(ui, |ui| {
                for row in &self.games {
                    let color = theme::tier_color(row.ac);
                    let icon_key = row.entry.install_dir.display().to_string();
                    let (_, rect) = theme::card_rect(ui, |ui| {
                        ui.horizontal(|ui| {
                            // 游戏图标（从渲染 EXE 提取）
                            match self.icon_textures.get(&icon_key) {
                                Some(tex) => {
                                    ui.add(
                                        egui::Image::new(tex)
                                            .fit_to_exact_size(egui::vec2(28.0, 28.0)),
                                    );
                                }
                                None => {
                                    ui.add_space(28.0);
                                }
                            }
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(&row.entry.name)
                                    .size(15.0)
                                    .color(theme::TEXT)
                                    .strong(),
                            );
                            theme::badge(ui, row.entry.source.label(), theme::NEUTRAL);
                            theme::badge(
                                ui,
                                &row.deployed.label(),
                                deploy_color(&row.deployed),
                            );
                            theme::badge(ui, row.ac.label(), color);
                                });
                                ui.label(theme::path_text(
                                    row.entry.install_dir.display().to_string(),
                                ));
                            });
                        });
                        ui.label(match &row.render_exe {
                            Some(p) => theme::path_text(format!("渲染 EXE   {}", p.display())),
                            None => theme::hint("渲染 EXE   未找到（可手动选择其所在目录）"),
                        });
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            // 关键：部署目标必须是「渲染 EXE 所在目录」，不是游戏根目录。
                            // mod 文件放错地方游戏根本不会加载；早先这里传的是根目录，
                            // 导致选中后部署卡片去根目录找文件，一律显示「未部署」。
                            let target_dir = row
                                .render_exe
                                .as_ref()
                                .and_then(|p| p.parent())
                                .map(|d| d.to_path_buf())
                                .unwrap_or_else(|| row.entry.install_dir.clone());
                            let has_exe = row.render_exe.is_some();
                            if theme::primary_button(ui, "用作部署目录", !self.busy && has_exe)
                                .on_hover_text(if has_exe {
                                    "把 mod 部署到渲染 EXE 所在目录"
                                } else {
                                    "没找到渲染 EXE，请手动选择它所在目录"
                                })
                                .clicked()
                            {
                                pick = Some(target_dir.clone());
                            }
                            if theme::ghost_button(ui, "打开文件夹", true).clicked() {
                                open = Some(target_dir.clone());
                            }
                        });
                    });

                    // 卡片左侧的等级色条，用卡片实际矩形画，高度自动跟随内容
                    ui.painter().rect_filled(
                        egui::Rect::from_min_size(
                            rect.min + egui::vec2(1.0, 1.0),
                            egui::vec2(3.0, rect.height() - 2.0),
                        ),
                        egui::CornerRadius {
                            nw: theme::R_CARD,
                            ne: 0,
                            sw: theme::R_CARD,
                            se: 0,
                        },
                        color,
                    );

                    ui.add_space(10.0);
                }
            });

            if let Some(p) = pick {
                self.set_game_dir(p);
            }
            if let Some(p) = open {
                open_in_explorer(&p);
            }
        });

        // ---------------- 显卡名操作的确认弹窗
        // 改注册表属于不可逆操作（虽然能还原），所以这里再让用户看一眼「原值 -> 新值」。
        if let Some(op) = self.spoof_pending.clone() {
            let ctx = self.ctx.clone();
            let (mut go, mut close) = (false, false);
            let title = match &op {
                gpu::Op::Apply(_) => "确认修改显卡名称",
                gpu::Op::Restore(_) => "确认还原显卡名称",
            };
            let adapters = self.adapters.clone();
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(&ctx, |ui| {
                    ui.set_max_width(460.0);
                    let a = gpu::primary(&adapters).cloned();
                    match &op {
                        gpu::Op::Apply(name) => {
                            if let Some(a) = &a {
                                ui.label(theme::hint("要改的注册表值（只改这一个）："));
                                ui.label(theme::path_text(format!(
                                    "HKLM\\{}\\DeviceDesc",
                                    a.enum_key
                                )));
                                ui.add_space(6.0);
                                ui.label(format!(
                                    "原值：{}",
                                    a.current_name().unwrap_or_else(|| "(空)".to_owned())
                                ));
                                ui.label(
                                    egui::RichText::new(format!("新值：{name}")).strong(),
                                );
                            }
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new(
                                    "重启后生效。有内核级反作弊的游戏请格外谨慎。",
                                )
                                .size(11.5)
                                .color(theme::WARN),
                            );
                        }
                        gpu::Op::Restore(to) => {
                            let target = match to {
                                gpu::RestoreTo::DriverName => a
                                    .as_ref()
                                    .map(|x| x.driver_name.clone())
                                    .unwrap_or_default(),
                                gpu::RestoreTo::BackupOriginal => a
                                    .as_ref()
                                    .and_then(gpu::backup_original_of)
                                    .unwrap_or_default(),
                            };
                            ui.label("会把注册表里的显卡名写回下面这个值：");
                            ui.add_space(6.0);
                            ui.label(egui::RichText::new(target).strong());
                        }
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if theme::primary_button(ui, "确认并继续", true).clicked() {
                            go = true;
                        }
                        if theme::ghost_button(ui, "取消", true).clicked() {
                            close = true;
                        }
                    });
                    ui.label(theme::hint(
                        "继续后会弹出 Windows 的管理员确认框（UAC），点「是」才真正写入。",
                    ));
                });
            if go {
                self.spoof_pending = None;
                self.start_gpu_op(op);
            } else if close {
                self.spoof_pending = None;
                self.status = "已取消，没有改动任何注册表值".to_owned();
            }
        }

        // ---------------- 驱动过旧时「部署」的二次确认
        if self.confirm_old_driver {
            let ctx = self.ctx.clone();
            let ver = self
                .driver
                .as_ref()
                .map(|d| d.marketing.clone())
                .unwrap_or_default();
            let (mut go, mut close) = (false, false);
            egui::Window::new("驱动版本偏低")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(&ctx, |ui| {
                    ui.set_max_width(440.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "当前驱动 {ver} 低于建议的 {}。",
                            gpu::MIN_FG_DRIVER_TEXT
                        ))
                        .size(13.0)
                        .color(theme::DANGER)
                        .strong(),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        "驱动过旧时帧生成很可能不生效，部署了也是白部署。建议先更新显卡驱动再试。",
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if theme::ghost_button(ui, "仍要继续部署", true).clicked() {
                            go = true;
                        }
                        if theme::primary_button(ui, "先去更新驱动", true).clicked() {
                            close = true;
                        }
                    });
                });
            if go {
                self.confirm_old_driver = false;
                self.do_deploy(Vec::new());
            } else if close {
                self.confirm_old_driver = false;
                if let Err(e) = util::open_url(gpu::DRIVER_URL) {
                    self.status = format!("打开驱动下载页失败: {e}");
                }
            }
        }

        // ---------------- 目录里还有另一个本项目代理时的确认
        if let Some(extras) = self.asked_extra_proxies.clone() {
            let ctx = self.ctx.clone();
            let list = extras.join("、");
            let mut go = false;
            let mut keep = false;
            egui::Window::new("目录里还有另一个代理入口")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(&ctx, |ui| {
                    ui.set_max_width(470.0);
                    ui.label(
                        egui::RichText::new(format!("目标目录里还有本项目的 {list}。"))
                            .size(13.0)
                            .color(theme::WARN)
                            .strong(),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        "上游要求「每次只保留本项目的一个代理」。同时存在两个时，游戏加载哪一个是没准的 —— 可能用的还是旧的那个，看起来就像部署没生效。",
                    );
                    ui.add_space(4.0);
                    ui.label(theme::hint(
                        "移除前会先把原文件备份下来，之后点「还原」可以恢复。",
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if theme::primary_button(ui, "移除并继续部署", true).clicked() {
                            go = true;
                        }
                        if theme::ghost_button(ui, "保留并继续", true).clicked() {
                            keep = true;
                        }
                    });
                });
            if go {
                self.asked_extra_proxies = None;
                self.do_deploy(extras);
            } else if keep {
                self.asked_extra_proxies = None;
                self.do_deploy(Vec::new());
            }
        }

        // ---------------- 部署完发现「硬件加速 GPU 计划」是关着的
        if self.hags_prompt {
            let ctx = self.ctx.clone();
            let (mut go, mut ok) = (false, false);
            egui::Window::new("建议开启硬件加速 GPU 计划")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(&ctx, |ui| {
                    ui.set_max_width(470.0);
                    ui.label(
                        egui::RichText::new("检测到「硬件加速 GPU 计划」是关闭的。")
                            .size(13.0)
                            .color(theme::DANGER)
                            .strong(),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        "DLSS 帧生成要求这一项开启（游戏官方的支持说明里也是这么写的）。关着的话，即使部署全对，帧生成也可能不生效。",
                    );
                    ui.add_space(4.0);
                    ui.label(theme::hint(
                        "点「去设置」会打开 Windows 设置里那一页，你自己把开关打开即可。改完要重启一次电脑才生效 —— 程序不会替你重启。",
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if theme::primary_button(ui, "去设置", true).clicked() {
                            go = true;
                        }
                        if theme::ghost_button(ui, "知道了", true).clicked() {
                            ok = true;
                        }
                    });
                });
            if go {
                self.hags_prompt = false;
                if let Err(e) = gpu::open_hags_settings() {
                    self.status = format!("打开系统设置失败: {e}");
                }
            } else if ok {
                self.hags_prompt = false;
            }
        }
    }
}
/// egui 自带字体不含汉字，不装字体整个中文界面会是方块。
/// 直接读系统字体，避免往 exe 里塞 10 MB 字体。
fn install_cjk_font(ctx: &egui::Context) {
    // 诊断开关：设了就用默认字体（中文会变方块），用来量化字体占多少内存
    if std::env::var_os("DLSSG_NO_CJK_FONT").is_some() {
        return;
    }
    const CANDIDATES: [&str; 3] = ["simhei.ttf", "msyh.ttc", "simsun.ttc"];

    let Some(font_dir) = windows_fonts_dir() else {
        return;
    };

    for name in CANDIDATES {
        let path = font_dir.join(name);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };

        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "cjk".to_owned(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "cjk".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("cjk".to_owned());

        ctx.set_fonts(fonts);
        return;
    }
}

fn windows_fonts_dir() -> Option<PathBuf> {
    if let Some(windir) = std::env::var_os("WINDIR") {
        let p = Path::new(&windir).join("Fonts");
        if p.is_dir() {
            return Some(p);
        }
    }
    let fallback = PathBuf::from(r"C:\Windows\Fonts");
    fallback.is_dir().then_some(fallback)
}
