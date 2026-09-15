// 关闭控制台窗口（仅 release）。调试时保留，方便 println! 排查。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod anticheat;
mod deploy;
mod gpu;
mod icon;
mod importer;
mod log;
mod scan;
mod theme;
mod update;
mod util;
mod verify;

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

    // 日志：放在程序同级 logs\ 下。用户反馈问题时把这里面的文件发过来就行。
    if let Some(p) = log::init() {
        log::line(&format!("FrameGen Manager v{}", update::SELF_VERSION));
        log::line(&format!("日志文件: {}", p.display()));
        log::line(&format!("Windows 构建号: {:?}", gpu::windows_build()));
        log::line(&format!(
            "exe: {}",
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|e| format!("未知（{e}）"))
        ));
        log::line(&format!("参数: {:?}", std::env::args().collect::<Vec<_>>()));
        log::line(&format!(
            "assets: {:?}",
            util::assets_dir().map(|p| p.display().to_string())
        ));
        log::line(&format!(
            "backups: {:?}",
            util::backups_dir().map(|p| p.display().to_string())
        ));

        // ---- 显卡识别全过程：用户报「型号识别错」时，这一段就是答案
        log::section("显卡识别");
        log::line(&format!("nvidia-smi 报的型号: {:?}", gpu::nvidia_smi_gpu_name()));
        let adapters = gpu::enumerate();
        log::line(&format!("在位且在跑的 NVIDIA 显示适配器: {} 个", adapters.len()));
        for a in &adapters {
            log::line(&format!(
                "  {}  类键实例 {}  驱动 {}  硬件 ID {}  Enum {}",
                a.driver_name, a.class_sub, a.driver_version, a.hardware_id, a.enum_key
            ));
        }
        log::line("（注意：这里只列「设备真的还在」的显卡。类键里可能还有旧显卡留下的幽灵条目，");
        log::line("  它们不参与判定 —— 以前正是它们导致型号忽而 1030 忽而 40 系。）");
        let name = scan::detect_gpu();
        log::line(&format!("最终使用的型号: {:?}", name));
        if let Some(n) = &name {
            log::line(&format!("路由判定: {}", scan::classify_gpu(n).label()));
        }
        log::line(&format!("硬件加速 GPU 计划: {}", gpu::hags_state().label()));
    }

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

    // 选源排序 + 慢源下载自测（本地起慢服务器，不依赖外网）：cargo run -- --sourcetest
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

    // 手动导入流水线（只解包 + 校验，不写资产目录）：cargo run -- --importtest <zip/文件夹>...
    let argv_i2: Vec<String> = std::env::args().collect();
    if let Some(pos) = argv_i2.iter().position(|a| a == "--importtest") {
        println!("===== 手动导入 自测（只校验，不写资产目录）=====");
        let paths: Vec<PathBuf> = argv_i2.iter().skip(pos + 1).map(PathBuf::from).collect();
        let work = std::env::temp_dir().join("fgm-import-selftest");
        let _ = std::fs::remove_dir_all(&work);
        let cancel = AtomicBool::new(false);
        let legacy = util::load_config().legacy_3101;
        println!(
            "  当前在用的版本 = {}",
            if legacy { "310.1 版（RTX 20 系）" } else { "最新版（310.9）" }
        );
        match importer::stage(&paths, legacy, &work, &cancel, |m| println!("  {m}")) {
            Ok((items, notes)) => {
                println!("  认出来 {} 个文件：", items.len());
                for it in &items {
                    println!(
                        "  [{}] {}（{}，{}）来自 {}",
                        if it.trusted { "通过" } else { "待确认" },
                        it.name,
                        it.kind.label(),
                        util::format_bytes(it.bytes),
                        it.from
                    );
                    println!("        {}", it.note);
                    println!("        sha256 {}", it.sha256);
                }
                for n in &notes {
                    println!("  说明: {n}");
                }
                // 真写一遍到临时目录：验证「点了继续导入之后」那条路不会再失败
                let dest = std::env::temp_dir().join("fgm-import-install-test");
                let _ = std::fs::remove_dir_all(&dest);
                match importer::install_to(&dest, &items) {
                    Ok(done) => {
                        println!("  写盘测试：{} 个文件已落位", done.len());
                        for d in &done {
                            let p = dest.join(d);
                            let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                            println!("    {}  {} 字节", d, sz);
                        }
                    }
                    Err(e) => println!("  [FAIL] 写盘测试失败：{e}"),
                }
                let _ = std::fs::remove_dir_all(&dest);
                importer::cleanup(&items);
            }
            Err(e) => println!("  [FAIL] {e}"),
        }
        let _ = std::fs::remove_dir_all(&work);
        return Ok(());
    }

    // 严格签名校验（手动导入用的那套）：cargo run -- --verifytest <文件>...
    let argv_v: Vec<String> = std::env::args().collect();
    if let Some(pos) = argv_v.iter().position(|a| a == "--verifytest") {
        println!("===== 严格签名校验 =====");
        for p in argv_v.iter().skip(pos + 1) {
            let path = PathBuf::from(p);
            if !path.is_file() {
                println!("{} -> 文件不存在", path.display());
                continue;
            }
            let rep = verify::verify_file(&path);
            println!("{}", path.display());
            println!("  {}", rep.summary());
            println!(
                "  静默通过（内容可信）= {}",
                if rep.content_trusted() { "是" } else { "否" }
            );
            if let Some(n) = &rep.note {
                println!("  说明: {n}");
            }
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
        match update::ensure_dlss_runtime(&c, &cancel, &plan, ctx, |msg, _f| {
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
    println!("=== 显示适配器类键下的所有实例（含非 NVIDIA、含幽灵条目）===");
    println!("  本程序判定型号时**不看这里**，只列出来对照。用户报「型号识别错」时，");
    println!("  下面几行能直接看出有没有旧显卡留下的幽灵条目、或者被人改过的名字。");
    if let Ok(base) =
        winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE).open_subkey(gpu::CLASS_KEY)
    {
        for sub in base.enum_keys().flatten() {
            if sub.len() != 4 || !sub.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let Ok(k) = base.open_subkey(&sub) else { continue };
            let desc: String = k.get_value("DriverDesc").unwrap_or_default();
            let mid: String = k.get_value("MatchingDeviceId").unwrap_or_default();
            let present = gpu::enumerate().iter().any(|a| a.class_sub == sub);
            println!("  {sub}  在位={present}  名称={desc}  匹配设备={mid}");
        }
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
    // 跟随配置里选的版本，命令行才能把两种版本都端到端跑一遍
    let legacy = util::load_config().legacy_3101;
    let repo_path = update::proxy_repo_path("version.dll", legacy);
    println!("  资产版本 = {}", if legacy { "310.1（RTX 20 系）" } else { "最新版 310.9" });
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
/// 本地起个 HTTP 服务，用来验证下载链路 —— 不依赖外网，结果可重复。
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
    println!(
        "  （官方源永远排最后；这个顺序只决定先试谁，下载中不会因为慢而换源 —— 慢源也让它下完）"
    );
}

fn ck(fails: &mut Vec<String>, ok: bool, what: &str) {
    println!("  [{}] {what}", if ok { "PASS" } else { "FAIL" });
    if !ok {
        fails.push(what.to_owned());
    }
}

/// 选源 / 下载自测。
fn sourcetest() {
    println!("===== 选源 + 下载 自测 =====");
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

    // 回归：有用户报「下载到四分之一就断了」。原因是当时按平均速度判 ——
    // 低于 300 KB/s 就立刻掐掉这个源去换下一个，他线路慢，永远换不到一个「够快」的源。
    // 现在这条规则没了：慢源必须能慢慢下完，而且字节数要对。
    // 4 MB 的响应，每 110ms 只给 32 KB，约 220 KB/s（比当年那个阈值还慢）。
    let (slow_port, slow_stop) = spawn_http_server(4 * 1024 * 1024, 32 * 1024, 110);
    let d1 = tmp.join("fgm-slow-source-test.bin");
    let _ = std::fs::remove_file(&d1);
    let u1 = format!("http://127.0.0.1:{slow_port}/slow");
    println!("-- 慢源（约 220 KB/s，比当年的 300 KB/s 阈值还慢）--");
    let t0 = std::time::Instant::now();
    // 顺手记下进度回调里报给界面的那几段文字 —— 用户能不能看懂就靠它
    let mut notes: Vec<String> = Vec::new();
    let r1 = update::download(
        &c, "slow-source-test", &d1, &u1, None, &cancel, "本地慢源",
        &mut |_, _, note| {
            if notes.last().map(|n| n != note).unwrap_or(true) {
                notes.push(note.to_owned());
            }
        },
    );
    let el = t0.elapsed().as_secs_f64();
    let msg1 = match &r1 {
        Ok(_) => "成功".to_owned(),
        Err(e) => e.to_string(),
    };
    ck(&mut fails, r1.is_ok(), &format!("慢源照样下完（{el:.1} 秒）：{msg1}"));
    ck(
        &mut fails,
        std::fs::metadata(&d1).map(|m| m.len() == 4 * 1024 * 1024).unwrap_or(false),
        "慢源下到的字节数完整（4 MB）",
    );
    ck(
        &mut fails,
        notes.first().map(|n| n.starts_with("经 ")).unwrap_or(false),
        "进度里会说清楚用的是哪个源（经 xxx）",
    );
    ck(
        &mut fails,
        !notes.iter().any(|n| n.contains("速度不达标")),
        "不再出现「速度不达标，换下一个」",
    );

    // 小文件照旧
    let (small_port, small_stop) = spawn_http_server(512 * 1024, 64 * 1024, 0);
    let d2 = tmp.join("fgm-small-test.bin");
    let _ = std::fs::remove_file(&d2);
    let u2 = format!("http://127.0.0.1:{small_port}/small");
    let r2 = update::download(&c, "small-test", &d2, &u2, None, &cancel, "本地小源", &mut |_, _, _| {});
    println!("-- 小文件 --");
    ck(&mut fails, r2.is_ok(), "512 KB 的文件正常下完");
    ck(
        &mut fails,
        std::fs::metadata(&d2).map(|m| m.len() == 512 * 1024).unwrap_or(false),
        "小文件字节数正确",
    );

    for s in [&slow_stop, &small_stop] {
        s.store(true, Ordering::Relaxed);
    }
    for d in [&d1, &d2] {
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

    // WeGame：判定保守，只认「里面真能找到游戏程序」的目录。
    // 开发机上没装 WeGame，所以这里验的是**不变式**（扫出来的都必须是真的），不是数量。
    let wg = scan::scan_wegame();
    let wg_ok = wg
        .iter()
        .all(|g| g.install_dir.is_dir() && scan::find_render_exe(&g.install_dir).is_some());
    println!(
        "  [{}] WeGame 扫出 {} 条，每条都指向真实存在的游戏目录{}",
        if wg_ok { "PASS" } else { "FAIL" },
        wg.len(),
        if wg.is_empty() { "（本机没装 WeGame，属正常）" } else { "" }
    );
    for g in &wg {
        println!("      {} -> {}", g.name, g.install_dir.display());
    }
    println!(
        "  [{}] WeGame 没装就不列任何东西（本机装了 QQ，它的键也在 Tencent 下面）",
        if scan::wegame_installed() || wg.is_empty() { "PASS" } else { "FAIL" }
    );
    println!(
        "  [{}] 没把 QQNT 之类的客户端当成 WeGame 游戏",
        if wg.iter().all(|g| !g.name.eq_ignore_ascii_case("QQNT")) { "PASS" } else { "FAIL" }
    );
    println!(
        "  [{}] wegame_value_is_path_like：InstallPath=true / Name=false",
        if scan::wegame_value_is_path_like("InstallPath")
            && !scan::wegame_value_is_path_like("Name")
        {
            "PASS"
        } else {
            "FAIL"
        }
    );
    println!(
        "  [{}] wegame_name_from_key：带编号的后缀会被去掉",
        if scan::wegame_name_from_key("铁甲雄兵(2000806)") == "铁甲雄兵"
            && scan::wegame_name_from_key("DNF") == "DNF"
        {
            "PASS"
        } else {
            "FAIL"
        }
    );

    // Q1 回归：显卡型号怎么挑。本机只有一块卡，多卡和幽灵条目只能靠这个纯函数验。
    println!("  显卡型号挑选（nvidia-smi 优先 / 幽灵条目不参与）:");
    let mk = |sub: &str, name: &str, ver: &str| gpu::GpuAdapter {
        class_sub: sub.to_owned(),
        driver_name: name.to_owned(),
        driver_version: ver.to_owned(),
        enum_key: format!("ENUM/{sub}"),
        hardware_id: "PCI-VEN-10DE".to_owned(),
        device_desc: None,
    };
    let cases: [(&str, Vec<gpu::GpuAdapter>, Option<&str>, Option<&str>); 4] = [
        (
            "能识别的 RTX 排在认不出来的 GT 1030 前面",
            vec![
                mk("0000", "NVIDIA GeForce GT 1030", "1"),
                mk("0001", "NVIDIA GeForce RTX 3050", "2"),
            ],
            None,
            Some("NVIDIA GeForce RTX 3050"),
        ),
        (
            "nvidia-smi 报的优先于注册表（注册表可能被别的工具改过）",
            vec![mk("0000", "NVIDIA GeForce GT 1030", "1")],
            Some("NVIDIA GeForce RTX 3050"),
            Some("NVIDIA GeForce RTX 3050"),
        ),
        (
            "两块都认得出来时按驱动版本从新到旧",
            vec![
                mk("0000", "NVIDIA GeForce RTX 3070", "32.0.16.1692"),
                mk("0001", "NVIDIA GeForce RTX 3080", "31.0.15.0000"),
            ],
            None,
            Some("NVIDIA GeForce RTX 3070"),
        ),
        ("一块卡都没有就返回空，不瞎猜", vec![], None, None),
    ];
    for (label, adapters, smi, want) in cases {
        let got = scan::pick_gpu_name(&adapters, smi.map(str::to_owned));
        println!(
            "    [{}] {label}（得到 {:?}）",
            if got.as_deref() == want { "PASS" } else { "FAIL" },
            got
        );
    }

    // 日志：用户反馈问题就靠它
    let log_file = log::path();
    println!(
        "  [{}] 日志文件已建立：{}",
        if log_file.map(|p| p.is_file()).unwrap_or(false) { "PASS" } else { "FAIL" },
        log_file
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "（没有）".to_owned())
    );

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
            // 和界面里一样：每个文件发一次 HEAD 拿 ETag，一次 API 都不调。
            // 名单跟着「在用的那一版」走 —— 上游 0.3.0 把 altnative/ 改成了 alternatives/。
            let legacy = util::load_config().legacy_3101;
            println!(
                "  资产版本: {}",
                if legacy {
                    "310.1 版（给 RTX 20 / GTX 16 系）"
                } else {
                    "最新版（上游 310.9 代理包）"
                }
            );
            let mut specs: Vec<String> = scan::PROXY_PRIORITY
                .iter()
                .map(|p| update::proxy_repo_path(p, legacy).to_owned())
                .collect();
            specs.push(update::ini_repo_path(legacy).to_owned());
            println!("  逐个 HEAD 取内容指纹（0 次 API 调用）：");
            // 这一段以前要 15 秒以上：release 直链是 github.com，第一次探测会先干等
            // 连接超时才轮到镜像。现在直链探测改镜像优先，连接超时也从 15s 收到 8s。
            let t_head = std::time::Instant::now();
            for path in &specs {
                match update::probe_remote(&c, path) {
                    Ok(r) => {
                        let local = update::local_name(path);
                        println!(
                            "  {:<22} etag={} size={:>9} 需要下载={}",
                            local,
                            &r.etag[..r.etag.len().min(10)],
                            r.size,
                            st.needs_update(&local, &r)
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

    // 上游 0.3.0 换过文件名、路径和 README 写法，下面这两组断言就是防它再改一次
    let mut fails: Vec<String> = Vec::new();

    println!("\n--- 上游版本号解析（新旧两种写法都要认）---");
    let cases: [(&str, Option<&str>); 4] = [
        ("# DLSSG Native 0.2.4\n", Some("0.2.4")),
        ("# DLSSG for SM86（Proxy）- 0.3.0 版本\n", Some("0.3.0")),
        ("# DLSSG for SM86 (proxy) - 0.3.0 Version\n", Some("0.3.0")),
        ("这一行没有任何版本号\n", None),
    ];
    for (text, want) in cases {
        let got = update::extract_version(text);
        ck(
            &mut fails,
            got.as_deref() == want,
            &format!("{:?} -> {:?}", text.trim(), got),
        );
    }

    println!("\n--- 仓库路径（最新版 / 给 RTX 20 系的 310.1 版）---");
    ck(
        &mut fails,
        update::proxy_repo_path("version.dll", false) == "version.dll",
        "新版 version.dll 在仓库根目录",
    );
    ck(
        &mut fails,
        update::proxy_repo_path("winmm.dll", false) == "alternatives/winmm.dll",
        "新版备用入口在 alternatives/（以前叫 altnative/）",
    );
    ck(
        &mut fails,
        update::proxy_repo_path("dbghelp.dll", false) == "alternatives/dbghelp.dll",
        "新版新增的 dbghelp 也能找到",
    );
    ck(
        &mut fails,
        update::proxy_repo_path("version.dll", true) == "310.1/version.dll",
        "RTX 20 系用的 310.1 版在 310.1/ 下",
    );
    ck(
        &mut fails,
        update::proxy_repo_path("dbghelp.dll", true) == "310.1/alternatives/dbghelp.dll",
        "310.1 版的备用入口在 310.1/alternatives/",
    );
    ck(
        &mut fails,
        update::ini_repo_path(true) == "dlssg_sm86.ini",
        "310.1 版也配根目录那份出厂 INI",
    );
    ck(
        &mut fails,
        scan::PROXY_PRIORITY.len() == 6,
        "现在有 6 个代理入口",
    );
    ck(
        &mut fails,
        scan::is_known_proxy("winhttp.dll") && scan::is_known_proxy("d3d12.dll"),
        "历史上出现过的入口名都算代理入口",
    );
    ck(
        &mut fails,
        !scan::is_known_proxy("nvngx_dlssg.dll"),
        "DLSS 运行库不算代理入口",
    );

    println!("\n--- INI 改写（SM75 路由）---");
    // 用临时文件造两种 INI：带 Router 的老版、没有 Router 的新版。
    // 不依赖 assets 目录里有没有下载过东西，结果可重复。
    {
        let dir = std::env::temp_dir().join("fgm-ini-selftest");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let old_ini = dir.join("old.ini");
        std::fs::write(
            &old_ini,
            "; Native 0.2.4\n[Compatibility]\nRouter=SM86\nKernelImage=PTX\n",
        )
        .unwrap();
        let old_out = dir.join("old-out.ini");
        let ok = update::prepare_deploy_ini_files(
            &old_ini,
            &old_out,
            scan::GpuRoute::Sm75,
            Some("NVIDIA GeForce RTX 2080"),
        );
        let patched = ok
            .as_ref()
            .ok()
            .and_then(|_| std::fs::read_to_string(&old_out).ok())
            .unwrap_or_default();
        ck(
            &mut fails,
            patched.contains("Router=SM75"),
            "带 Router 的老 INI：会被改成 SM75",
        );
        if let Ok(p) = &ok {
            for c in &p.changes {
                println!("  改动说明: {c}");
            }
        }

        // 上游 0.3.0 的 INI 里已经没有 Router 项了（路由改由 DLL 自己判断）。
        // 以前这里直接报错并中止部署，等于把 RTX 20 用户挡在门外。
        let new_ini = dir.join("new.ini");
        std::fs::write(&new_ini, "; slim 0.3.0\n[Compatibility]\nPreset=Auto\n").unwrap();
        let new_out = dir.join("new-out.ini");
        let r = update::prepare_deploy_ini_files(
            &new_ini,
            &new_out,
            scan::GpuRoute::Sm75,
            Some("NVIDIA GeForce RTX 2080"),
        );
        ck(
            &mut fails,
            r.is_ok(),
            "没有 Router 的 INI：SM75 部署不再失败（原样部署）",
        );
        ck(
            &mut fails,
            std::fs::read_to_string(&new_out)
                .map(|t| t.contains("Preset=Auto"))
                .unwrap_or(false),
            "原样部署的文件内容和上游一致",
        );
        if let Ok(p) = &r {
            for c in &p.changes {
                println!("  改动说明: {c}");
            }
        }

        // 顺带看看真实资产目录里的那份（只显示，不做断言）
        match update::prepare_deploy_ini(scan::GpuRoute::Sm75, Some("NVIDIA GeForce RTX 2080")) {
            Ok(p) => println!("  真实 assets 里的 INI：生成 {}", p.path.display()),
            Err(e) => println!("  真实 assets 里没有可用的 INI：{e}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    println!("\n--- 日志滚动（目录里只留两个文件）---");
    {
        let dir = std::env::temp_dir().join("fgm-log-selftest");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let names = |d: &Path| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(d)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
            v
        };
        let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();

        // 情况一：老版本留下的多个带时间戳文件（用户升级上来就是这个样）
        for n in [
            "framegen-20260101010101UTC.log",
            "framegen-20260202020202UTC.log",
        ] {
            let _ = std::fs::write(dir.join(n), b"legacy");
        }
        let cur = log::rotate(&dir);
        ck(
            &mut fails,
            cur == dir.join(log::CUR_NAME),
            "本次写的是固定名字 framegen.log",
        );
        ck(
            &mut fails,
            names(&dir) == vec![log::PREV_NAME.to_owned()],
            &format!(
                "升级上来第一次滚动：旧的带时间戳文件全清掉，只留 prev（实际 {:?}）",
                names(&dir)
            ),
        );
        ck(
            &mut fails,
            read(&dir.join(log::PREV_NAME)) == "legacy",
            "老版本里最新那份被留成「上一次」，没白丢",
        );

        // 情况二：正常一轮滚动 —— 本次的变成「上一次」，更早的删掉
        std::fs::write(dir.join(log::CUR_NAME), b"run2").unwrap();
        log::rotate(&dir);
        ck(
            &mut fails,
            read(&dir.join(log::PREV_NAME)) == "run2",
            "上一次运行的内容被滚到 prev",
        );
        ck(
            &mut fails,
            !dir.join(log::CUR_NAME).is_file(),
            "本次的文件先不存在（由 init 新建，保证从干净文件开始写）",
        );
        ck(
            &mut fails,
            names(&dir) == vec![log::PREV_NAME.to_owned()],
            "第二轮之后目录里只剩 prev 一个（init 马上会建本次那个）",
        );

        // 情况三：再滚一轮，prev 被删、本次上位
        std::fs::write(dir.join(log::CUR_NAME), b"run3").unwrap();
        log::rotate(&dir);
        ck(
            &mut fails,
            read(&dir.join(log::PREV_NAME)) == "run3" && names(&dir).len() == 1,
            "继续滚动也不会堆积（永远最多 2 个文件）",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    println!("\n--- 手动导入：校验与自定义源 ---");
    {
        // 没签名的文件（就是本程序自己）必须判成「不可信」
        if let Ok(exe) = std::env::current_exe() {
            let rep = verify::verify_file(&exe);
            ck(
                &mut fails,
                rep.sig == verify::SigState::NoSignature && !rep.content_trusted(),
                "没签名的文件判成「不可信」",
            );
        }
        // 资产目录里有真文件时，顺手验一遍（开发机上一定有）
        if let Ok(dir) = util::assets_dir() {
            let proxy = dir.join("version.dll");
            if proxy.is_file() {
                let rep = verify::verify_file(&proxy);
                ck(
                    &mut fails,
                    rep.content_trusted() && rep.kind == verify::SignerKind::Author,
                    "作者的代理 DLL 判成「本项目作者签的、内容可信」",
                );
            }
            let rt = dir.join("nvngx_dlssg.dll");
            if rt.is_file() {
                let rep = verify::verify_file(&rt);
                ck(
                    &mut fails,
                    rep.content_trusted() && rep.kind == verify::SignerKind::Nvidia,
                    "NVIDIA 运行库判成「NVIDIA 签的、内容可信」",
                );
            }
        }
        // ini 的结构检查
        let good = "; c\n[General]\nEnabled=1\n[FrameGeneration]\nOptimized=1\n";
        ck(
            &mut fails,
            importer::ini_sanity(good).is_ok(),
            "正常的 ini 结构检查通过",
        );
        ck(
            &mut fails,
            importer::ini_sanity("garbage\nno equals sign\n").is_err(),
            "不像 ini 的内容判成可疑",
        );
        ck(
            &mut fails,
            importer::ini_sanity("").is_err(),
            "空文件判成可疑",
        );
        ck(
            &mut fails,
            importer::ini_sanity("[Unknown]\nKey=1\n").is_err(),
            "没有任何已知段落也判成可疑",
        );
        // 自定义源：一行一个，顺便归一化结尾斜杠、去重
        let multi = update::split_custom_sources("https://a.example\nhttps://b.example/\n\nhttps://a.example/");
        ck(
            &mut fails,
            multi.len() == 2
                && multi[0] == "https://a.example/"
                && multi[1] == "https://b.example/",
            "自定义源支持一行一个（归一化 + 去重）",
        );
        ck(
            &mut fails,
            update::split_custom_sources("随便写点什么").is_empty(),
            "不像网址的行会被忽略",
        );
        // 上游源码 zip 里同时有根目录、310.1/、archive/0.2.4/ 三套同名文件：
        // 同名时只留当前在用的那一版（0 最好），否则用户会被三份一样的东西搞晕，
        // 而且老版文件会顶掉新版（这正是用户遇到的「官方文件被提示非法」）。
        ck(
            &mut fails,
            importer::build_rank("dlssg_sm86.ini", false) == 0
                && importer::build_rank("310.1/version.dll", false) == 1
                && importer::build_rank("archive/0.2.4/version.dll", false) == 2,
            "最新版模式：根目录优先 > 310.1/ > 归档老版",
        );
        ck(
            &mut fails,
            importer::build_rank("310.1/version.dll", true) == 0
                && importer::build_rank("version.dll", true) == 1,
            "310.1 模式：310.1/ 里的优先",
        );
        ck(
            &mut fails,
            verify::AUTHOR_CERT_NATIVE.len() == 40 && verify::AUTHOR_CERT_PROXY.len() == 40,
            "作者两张证书的指纹都在名单里（换过证书，两张都得认）",
        );
    }

    println!("\n--- Steam 运行库过滤（别误杀真游戏）---");
    {
        let cases: [(&str, bool); 7] = [
            ("Proton Bus Simulator", false),
            ("Proton 9.0", true),
            ("Proton Experimental", true),
            ("Steam Linux Runtime 3.0 (sniper)", true),
            ("SteamVR", true),
            ("Steamworks Common Redistributables", true),
            ("Counter-Strike 2", false),
        ];
        for (name, want_junk) in cases {
            let got = scan::is_steam_junk(name);
            ck(
                &mut fails,
                got == want_junk,
                &format!("{name} -> 当运行库排除={got}"),
            );
        }
        ck(
            &mut fails,
            scan::uninstall_is_steam("Steam")
                && !scan::uninstall_is_steam("SteamVR")
                && !scan::uninstall_is_steam("Steamworks"),
            "卸载项里只有名为 Steam 的那条算 Steam 本体",
        );
    }

    println!("\n--- 部署状态文案（别让人读成「部署失败」）---");
    {
        let manual = deploy::DeployState::ManuallyInstalled {
            files: vec!["version.dll".to_owned()],
        };
        ck(
            &mut fails,
            manual.label().contains("本工具无记录"),
            "文件在但没记录时，说的是「本工具无记录」而不是「非本工具部署」",
        );
        ck(
            &mut fails,
            deploy::DeployState::NotDeployed.label().contains("未部署"),
            "没装过的文案还是「未部署」",
        );
    }

    println!("\n--- 游戏库缓存（下次打开不用再扫一遍）---");
    {
        let dir = std::env::temp_dir().join("fgm-lib-selftest");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let dead = dir.join("这个游戏已经卸载了");
        let cache = scan::LibraryCache {
            scanned_at: "2026-01-01 00:00 UTC".to_owned(),
            scanned: vec![
                scan::CachedRow {
                    entry: GameEntry {
                        source: scan::Launcher::Steam,
                        app_id: "1".to_owned(),
                        name: "还在的游戏".to_owned(),
                        install_dir: dir.clone(),
                    },
                    render_exe: None,
                    ac: AcTier::UserMode,
                },
                scan::CachedRow {
                    entry: GameEntry {
                        source: scan::Launcher::Epic,
                        app_id: "2".to_owned(),
                        name: "卸载了的游戏".to_owned(),
                        install_dir: dead.clone(),
                    },
                    render_exe: None,
                    ac: AcTier::None,
                },
            ],
            manual: vec![scan::CachedRow {
                entry: scan::manual_entry(&dead),
                render_exe: None,
                ac: AcTier::None,
            }],
        };
        let p = dir.join("game_library.json");
        let wrote = scan::save_library_from(&p, &cache).is_ok();
        let back = scan::load_library_from(&p);
        ck(&mut fails, wrote && p.is_file(), "游戏库缓存能写进文件");
        ck(
            &mut fails,
            back.scanned.len() == 2 && back.manual.len() == 1,
            "缓存读回来条目数量一致",
        );
        ck(
            &mut fails,
            back.scanned[0].ac == AcTier::UserMode
                && back.scanned[1].entry.name == "卸载了的游戏",
            "反作弊等级和名字都留下来了",
        );
        ck(
            &mut fails,
            back.scanned_at == "2026-01-01 00:00 UTC",
            "上次扫描的时间留下来了",
        );
        ck(
            &mut fails,
            App::build_cached_row(&back.scanned[1], false).is_none(),
            "扫出来的条目：目录没了就丢掉（游戏卸载了）",
        );
        ck(
            &mut fails,
            App::build_cached_row(&back.manual[0], true).is_some(),
            "手动条目：目录没了也留着，让用户自己决定要不要移除",
        );
        if let Some(row) = App::build_cached_row(&back.manual[0], true) {
            ck(
                &mut fails,
                row.target == dead && row.manual,
                "手动条目的部署目标 = 用户存的目录本身",
            );
        }
        ck(
            &mut fails,
            scan::manual_entry(Path::new("D:\\Games\\My Game")).name == "My Game",
            "手动条目的名字取目录名",
        );

        // 飞行动画的插值：端点必须落在起止矩形上（自测不开窗口也能验）
        let f = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 50.0));
        let t = egui::Rect::from_min_max(egui::pos2(200.0, 100.0), egui::pos2(300.0, 200.0));
        let near = |a: egui::Rect, b: egui::Rect| {
            (a.min.x - b.min.x).abs() < 0.01
                && (a.min.y - b.min.y).abs() < 0.01
                && (a.max.x - b.max.x).abs() < 0.01
                && (a.max.y - b.max.y).abs() < 0.01
        };
        ck(&mut fails, near(fly_lerp(f, t, 0.0), f), "飞行动画：起点对得上");
        ck(&mut fails, near(fly_lerp(f, t, 1.0), t), "飞行动画：终点对得上");
        let mid = fly_lerp(f, t, 0.5).center();
        ck(
            &mut fails,
            (mid.x - 150.0).abs() < 0.01 && (mid.y - 87.5).abs() < 0.01,
            "飞行动画：中点确实在中途",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    if !fails.is_empty() {
        println!("\n  ★ 有 {} 项断言失败", fails.len());
        for f in &fails {
            println!("    - {f}");
        }
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
                    imported: false,
                },
            );
            let probe = dest.with_file_name("iscurrent-probe.tmp");
            let _ = std::fs::copy(&dest, &probe);
            let present =
                update::local_is_current(&st, &update::local_name(repo_path), &probe, &remote);
            let _ = std::fs::remove_file(&probe);
            let gone =
                update::local_is_current(&st, &update::local_name(repo_path), &probe, &remote);
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
    /// 点「用作部署目录」时真正要用的目录。
    /// 扫出来的游戏 = 渲染 EXE 所在目录；手动加的 = 用户当时选的那个目录本身。
    target: PathBuf,
    /// 用户自己「存到游戏库」的条目（可以移除）
    manual: bool,
}

/// 选中游戏后「飞向目标目录」的那张小卡片。
///
/// 只在动画的这 0.45 秒里请求重绘，动画结束就停止 —— 不做常驻动画。
struct FlyAnim {
    from: egui::Rect,
    to: egui::Rect,
    label: String,
    color: egui::Color32,
    t0: std::time::Instant,
}

/// 飞行动画时长（秒）
const FLY_SECS: f32 = 0.45;
/// 落地后目标卡片高亮多久（秒）
const FLASH_SECS: f32 = 0.8;

/// 飞行动画的插值：e=0 在起点，e=1 到终点。
///
/// 中途把尺寸缩到 38% 再放大 —— 看起来像「这张卡片被拎起来飞过去」，
/// 而不是一块和两边一样大的色块横穿界面。位置仍然是中心的线性插值，
/// 所以 e=0 正好是起点矩形、e=1 正好是终点矩形（自测就验这两个端点）。
fn fly_lerp(from: egui::Rect, to: egui::Rect, e: f32) -> egui::Rect {
    let lerp = |a: f32, b: f32| a + (b - a) * e;
    let cx = lerp(from.center().x, to.center().x);
    let cy = lerp(from.center().y, to.center().y);
    let shrink = 1.0 - 0.62 * (4.0 * e * (1.0 - e));
    let w = lerp(from.width(), to.width()) * shrink;
    let h = lerp(from.height(), to.height()) * shrink;
    egui::Rect::from_center_size(egui::pos2(cx, cy), egui::vec2(w, h))
}

/// 界面行 -> 缓存里的一行（存盘用）
fn row_to_cached(r: &GameRow) -> scan::CachedRow {
    scan::CachedRow {
        entry: r.entry.clone(),
        render_exe: r.render_exe.clone(),
        ac: r.ac,
    }
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
    /// 上面这个指纹是不是**官方源**给的。镜像给的指纹和官方对不上，
    /// 拿它判「有更新」会误报 —— 见 update::RemoteFile::etag_trusted
    remote_etag_trusted: bool,
    /// DLSS 运行库：本地文件名，靠签名判断在不在
    runtime_file: Option<String>,
}

#[derive(Debug, Clone)]
struct UpdateSummary {
    version: Option<String>,
    rows: Vec<AssetRow>,
}

enum Msg {
    /// 扫描完成（附带扫描过程的说明：谁被跳过了、为什么）
    Scanned(Vec<GameRow>, Vec<String>),
    /// 启动时从缓存里恢复出来的游戏库（行 / 扫描时间 / 丢掉了几个失效条目）
    LibraryLoaded(Vec<GameRow>, String, usize),
    /// 后台跑完的深度反作弊扫描（带着目录，用来丢弃过期的结果）
    AcScanned(PathBuf, AcReport),
    /// 手动导入：zip 读完并逐个校验完了（第二项是「跳过/说明」清单）
    ImportStaged(Vec<importer::Staged>, Vec<String>),
    /// 手动导入：写盘完成，带回给用户看的结果清单
    ImportDone(String),
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
    /// 上次扫描完成的时间（显示用）
    scanned_at: String,
    /// 用户手动存进游戏库的条目。单独存一份，重新扫描不会冲掉它们。
    manual: Vec<scan::CachedRow>,

    ac_target: Option<AcReport>,
    /// 反作弊深度扫描正在后台跑（徽章先显示「分析中」）
    ac_scanning: bool,
    ac_system: AcReport,

    // ---- 手动导入（网盘 / U 盘 拿到的 zip）
    import_busy: bool,
    /// 校验没过、等用户确认的项（Some 时显示确认弹窗）
    import_pending: Option<Vec<importer::Staged>>,
    /// 导入结果清单（Some 时显示结果窗口）
    import_report: Option<String>,
    /// 本次导入的「跳过 / 说明」清单（写进结果窗口）
    import_notes: Vec<String>,

    /// 正在飞的选中动画
    fly: Option<FlyAnim>,
    /// 「目标目录」卡片这一帧的矩形（动画要飞过去）
    target_card_rect: Option<egui::Rect>,
    /// 目标卡片高亮到什么时候（飞行动画落地后闪一下）
    flash_until: Option<std::time::Instant>,

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

    // ---- 反作弊：不再直接拦死，改成弹窗问一句（用户明确要求"可以直接继续"）
    /// Some 时显示确认弹窗，里面是检出明细
    kernel_ac_pending: Option<AcReport>,
    /// 用户这次选择了「仍要部署」。只对本次生效，部署完就清掉。
    allow_kernel_ac: bool,
    /// 用户在「另一个代理」弹窗里定下来的选择，等真正部署时用
    deploy_extras: Vec<String>,

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
    /// 是否使用 310.1 版程序本体（310.1/）—— RTX 20 / GTX 16 系（SM75）用得上
    legacy_3101: bool,
    download_failed: bool,
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

        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
            tx,
            rx,
            game_dir: None,
            manual_path: String::new(),
            proxy: scan::PROXY_PRIORITY[0].to_owned(),
            games: Vec::new(),
            scanned: false,
            scanned_at: String::new(),
            manual: Vec::new(),
            ac_target: None,
            ac_scanning: false,
            // 枚举注册表很快，同步做完即可
            ac_system: anticheat::scan_system(),
            import_busy: false,
            import_pending: None,
            import_report: None,
            import_notes: Vec::new(),
            fly: None,
            target_card_rect: None,
            flash_until: None,
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
            kernel_ac_pending: None,
            allow_kernel_ac: false,
            deploy_extras: Vec::new(),
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
            legacy_3101: cfg.legacy_3101,
            download_failed: false,
            speed_results: Vec::new(),
            speed_testing: false,
            dl_started: None,
            dl_total: 0,
            dl_done: 0,
            ini_changes: Vec::new(),
            icon_textures: HashMap::new(),
        };
        // 上次扫过的游戏库直接摆出来，不用用户再点一次「扫描」
        app.load_cached_library();
        app
    }

    /// 一个游戏条目 -> 界面行：找渲染 EXE、判反作弊、看部署状态、取图标。
    /// known_exe 是缓存里记着的渲染 EXE —— 还在就直接用，省掉遍历游戏目录。
    fn build_row_with(entry: GameEntry, manual: bool, known_exe: Option<PathBuf>) -> GameRow {
        let render_exe = match known_exe {
            Some(p) if p.is_file() => Some(p),
            _ => scan::find_render_exe(&entry.install_dir),
        };
        // 除了游戏根目录，还要看渲染 EXE 所在目录：
        // BattlEye 经常埋在 ...\Binaries\Win64\BattlEye，只看根目录会漏
        let mut rep = anticheat::scan_game_dir(&entry.install_dir);
        if let Some(dir) = render_exe.as_ref().and_then(|p| p.parent()) {
            rep.merge(anticheat::scan_game_dir(dir));
        }
        let ac = rep.verdict();
        // 关键：mod 文件在渲染 EXE 目录，不是游戏根目录。
        // 手动加的条目例外 —— 用户选的那个目录就是部署目标，不去猜是哪一级。
        let target = if manual {
            entry.install_dir.clone()
        } else {
            render_exe
                .as_ref()
                .and_then(|p| p.parent())
                .map(|d| d.to_path_buf())
                .unwrap_or_else(|| entry.install_dir.clone())
        };
        let deployed = deploy::state_of(&target);
        let icon_img = render_exe.as_deref().and_then(icon::icon_of);
        GameRow {
            entry,
            ac,
            render_exe,
            deployed,
            icon: icon_img,
            target,
            manual,
        }
    }

    fn build_row(entry: GameEntry, manual: bool) -> GameRow {
        Self::build_row_with(entry, manual, None)
    }

    /// 缓存条目 -> 界面行。安装目录已经不在的（游戏卸载了）扫出来的条目直接丢掉；
    /// 手动加的条目留着，让用户自己决定要不要移除（可能是移动硬盘没插上）。
    fn build_cached_row(c: &scan::CachedRow, manual: bool) -> Option<GameRow> {
        if !c.entry.install_dir.is_dir() && !manual {
            return None;
        }
        Some(Self::build_row_with(
            c.entry.clone(),
            manual,
            c.render_exe.clone(),
        ))
    }

    /// 启动时把上次的扫描结果读出来显示。只读本地缓存：不联网、不在后台反复轮询，
    /// 想刷新还是得点「扫描」。
    fn load_cached_library(&mut self) {
        let cache = scan::load_library();
        if cache.scanned.is_empty() && cache.manual.is_empty() {
            return;
        }
        self.manual = cache.manual.clone();
        self.scanned_at = cache.scanned_at.clone();
        self.scanned = true;
        self.status = "已载入上次的扫描结果（要刷新请点「扫描」）".to_owned();
        let (scanned, manual, at) = (cache.scanned, cache.manual, cache.scanned_at);
        self.spawn(move |tx, ctx| {
            let mut dropped = 0usize;
            let mut rows: Vec<GameRow> = Vec::new();
            for c in &scanned {
                match App::build_cached_row(c, false) {
                    Some(r) => rows.push(r),
                    None => dropped += 1,
                }
            }
            for c in &manual {
                if let Some(r) = App::build_cached_row(c, true) {
                    rows.push(r);
                }
            }
            let _ = tx.send(Msg::LibraryLoaded(rows, at, dropped));
            ctx.request_repaint();
        });
    }

    /// 把当前游戏库写进缓存：扫出来的条目现写，手动条目原样保留。
    fn persist_library(&mut self) {
        let scanned: Vec<scan::CachedRow> = self
            .games
            .iter()
            .filter(|r| !r.manual)
            .map(row_to_cached)
            .collect();
        let cache = scan::LibraryCache {
            scanned_at: self.scanned_at.clone(),
            scanned,
            manual: self.manual.clone(),
        };
        if let Err(e) = scan::save_library(&cache) {
            self.note(format!("保存游戏库缓存失败（下次打开不会自动显示）: {e}"));
        }
    }

    /// 把当前目录存进游戏库（手动条目）。
    fn add_to_library(&mut self) {
        let Some(dir) = self.game_dir.clone() else {
            self.status = "先选一个目录，再存进游戏库".to_owned();
            return;
        };
        if self
            .games
            .iter()
            .any(|r| r.entry.install_dir == dir || r.target == dir)
        {
            self.status = "这个目录已经在游戏库里了".to_owned();
            return;
        }
        let entry = scan::manual_entry(&dir);
        let name = entry.name.clone();
        let cached = scan::CachedRow {
            entry,
            render_exe: scan::find_render_exe(&dir),
            ac: self
                .ac_target
                .as_ref()
                .map(|r| r.verdict())
                .unwrap_or(AcTier::None),
        };
        if let Some(row) = Self::build_cached_row(&cached, true) {
            self.games.push(row);
        }
        self.manual.push(cached);
        self.scanned = true;
        self.status = format!("已把「{name}」存进游戏库");
        self.note(format!("存进游戏库：{}", dir.display()));
        self.persist_library();
    }

    fn remove_from_library(&mut self, dir: &Path) {
        self.manual.retain(|c| c.entry.install_dir != dir);
        self.games.retain(|r| !(r.manual && r.entry.install_dir == dir));
        self.status = "已从游戏库移除（游戏目录里的文件没动）".to_owned();
        self.note(format!("从游戏库移除：{}", dir.display()));
        self.persist_library();
    }

    /// 让被选中的那张卡片飞向「目标目录」卡片。
    /// 目标卡片这一帧已经画过了（右侧面板先于中央列表绘制），所以矩形是新鲜的。
    fn start_fly(&mut self, from: egui::Rect, label: String, color: egui::Color32) {
        let Some(to) = self.target_card_rect else {
            return;
        };
        let now = std::time::Instant::now();
        // 目标卡片被滚出可视区时不做飞行（画到屏幕外反而更让人迷惑），只闪一下
        if !self.ctx.input(|i| i.viewport_rect()).intersects(to) {
            self.flash_until = Some(now + std::time::Duration::from_secs_f32(FLASH_SECS));
            return;
        }
        self.fly = Some(FlyAnim {
            from,
            to,
            label,
            color,
            t0: now,
        });
        self.flash_until =
            Some(now + std::time::Duration::from_secs_f32(FLY_SECS + FLASH_SECS));
        self.ctx.request_repaint();
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
        // 先做便宜的事：部署状态、入口推断 —— 点下去立刻就有反应。
        // 反作弊深度扫描要遍历整个目录，放后台（徽章先显示「分析中」）。
        self.deploy_state = deploy::state_of(&dir);
        self.manual_path = dir.display().to_string();
        self.game_dir = Some(dir.clone());
        self.redetect();
        self.status = format!("已选择 {}", dir.display());
        self.ac_target = None;
        self.start_ac_scan(dir);
    }

    /// 后台跑一次深度反作弊扫描，结果回来再更新徽章。
    fn start_ac_scan(&mut self, dir: PathBuf) {
        self.ac_scanning = true;
        self.spawn(move |tx, ctx| {
            let rep = anticheat::scan_deep(&dir);
            let _ = tx.send(Msg::AcScanned(dir, rep));
            ctx.request_repaint();
        });
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
        // 指纹只有官方源给的才可信。探测落到镜像时指纹和官方对不上，
        // 拿它判「有更新」会导致同一个文件每次都被标成需要下载 —— 也就是
        // 用户报的「不断重复下载」。不可信时按「文件在 + 大小对」当作就绪。
        if !row.remote_etag_trusted {
            return AssetState::Ready;
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

    /// 拼一段给开发者看的诊断信息（用户点「复制诊断信息」时用）。
    /// 只放排查需要的东西，不放任何密钥类内容。
    fn diagnostic_text(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("FrameGen Manager v{}\n", update::SELF_VERSION));
        s.push_str(&format!("Windows 构建号: {:?}\n", gpu::windows_build()));
        s.push_str(&format!("硬件加速 GPU 计划: {}\n", self.hags.label()));
        s.push_str(&format!(
            "日志目录: {}\n",
            log::dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "未知".to_owned())
        ));
        s.push_str("\n--- 显卡识别 ---\n");
        s.push_str(&format!("nvidia-smi 报的: {:?}\n", gpu::nvidia_smi_gpu_name()));
        for a in gpu::enumerate() {
            s.push_str(&format!(
                "在位适配器: {}  类键 {}  驱动 {}\n",
                a.driver_name, a.class_sub, a.driver_version
            ));
        }
        s.push_str(&format!("最终使用: {:?}\n", self.gpu_name));
        s.push_str(&format!("路由判定: {}\n", self.gpu_route.label()));
        if let Some(d) = &self.driver {
            s.push_str(&format!("驱动版本: {}（来源 {}）\n", d.marketing, d.source));
        }
        s.push_str(&format!(
            "资产目录: {}\n",
            util::assets_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "未知".to_owned())
        ));
        if let Some(g) = &self.game_dir {
            s.push_str(&format!("部署目录: {}\n", g.display()));
            s.push_str(&format!("部署状态: {}\n", self.deploy_state.label()));
        }
        if let Some(ac) = &self.ac_target {
            s.push_str(&format!("反作弊判定: {}\n", ac.verdict().label()));
            for h in &ac.hits {
                s.push_str(&format!(
                    "  · {}（{}）{}\n",
                    h.name,
                    h.tier.label(),
                    h.evidence
                ));
            }
        }
        s.push_str(&format!(
            "下载源: 指定={:?}（下载中不按速度换源，慢源也让它下完）\n",
            self.backup_prefix
        ));
        s.push_str(&format!(
            "资产版本: {}\n",
            if self.legacy_3101 {
                "310.1 版（给 RTX 20 / GTX 16 系，带 SM75 内核）"
            } else {
                "最新版（上游 310.9 代理包）"
            }
        ));
        s.push_str("\n--- 最近的操作日志 ---\n");
        let tail: Vec<&String> = self.logs.iter().rev().take(30).collect();
        for l in tail.into_iter().rev() {
            s.push_str(l);
            s.push('\n');
        }
        s
    }

    /// 记一条日志：既进界面的操作日志，也写磁盘日志。
    /// 用户反馈问题时把 logs 目录发过来，就有完整上下文了。
    fn note(&mut self, m: impl Into<String>) {
        let m = m.into();
        log::line(&m);
        self.logs.push(m);
    }

    fn refresh(&mut self) {
        self.update_state = update::load_state();
        if let Some(d) = self.game_dir.clone() {
            self.deploy_state = deploy::state_of(&d);
            self.ac_target = None;
            self.start_ac_scan(d);
        }
        // 游戏库里每一行的「部署状态」也要跟着刷新。
        //
        // 不刷的话行上的徽章会停在部署前的状态：用户在库里点开一个「已安装，非本工具部署」
        // 的游戏、部署成功之后，左侧卡片已经变成「已部署」，行上却还写着「非本工具部署」
        // —— 看上去就像部署没生效。这个状态本来就该在每次部署 / 还原之后重算。
        for row in &mut self.games {
            row.deployed = deploy::state_of(&row.target);
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Scanned(rows, notes) => {
                // 手动条目接在扫描结果后面 —— 重新扫描绝不能把它们冲掉
                let manual_rows: Vec<GameRow> = self
                    .manual
                    .iter()
                    .filter_map(|c| App::build_cached_row(c, true))
                    .collect();
                let n = rows.len();
                self.games = rows;
                self.games.extend(manual_rows);
                self.scanned = true;
                self.busy = false;
                self.scanned_at = util::now_utc();
                // 扫描过程说明也放进界面上的操作日志（太多就只放前面一部分），
                // 这样用户在界面上就能看到「某某被跳过、为什么」。
                let shown = notes.len().min(25);
                for l in notes.iter().take(shown) {
                    self.note(l.clone());
                }
                if notes.len() > shown {
                    self.note(format!(
                        "（还有 {} 条扫描说明，见 logs 目录）",
                        notes.len() - shown
                    ));
                }
                self.status = if notes.is_empty() {
                    format!("扫描完成，共 {n} 个游戏")
                } else {
                    format!(
                        "扫描完成，共 {n} 个游戏（{} 条扫描说明，见下方操作日志）",
                        notes.len()
                    )
                };
                // 顺手把这次的结果留给下一次启动用
                self.persist_library();
            }
            Msg::LibraryLoaded(rows, at, dropped) => {
                let n = rows.len();
                self.games = rows;
                self.scanned = true;
                self.scanned_at = at;
                self.status = if dropped > 0 {
                    format!("已载入上次的扫描结果：{n} 个游戏（{dropped} 个目录已不存在，已跳过）")
                } else {
                    format!("已载入上次的扫描结果：{n} 个游戏（要刷新请点「扫描」）")
                };
            }
            Msg::AcScanned(dir, rep) => {
                self.ac_scanning = false;
                // 用户可能已经换了目录，过期的结果丢掉
                if self.game_dir.as_deref() == Some(dir.as_path()) {
                    self.ac_target = Some(rep);
                }
            }
            Msg::ImportStaged(items, notes) => {
                self.import_notes = notes;
                let untrusted = items.iter().filter(|i| !i.trusted).count();
                if untrusted == 0 {
                    self.status = format!("{} 个文件校验通过，正在写入 ...", items.len());
                    self.finish_import(items, true);
                } else {
                    self.busy = false;
                    self.import_busy = false;
                    self.cancel = None;
                    self.status = format!("{untrusted} 个文件校验没过，等你确认");
                    self.note(format!(
                        "手动导入：{} 个文件校验没过（等用户确认）",
                        untrusted
                    ));
                    self.import_pending = Some(items);
                }
            }
            Msg::ImportDone(report) => {
                self.busy = false;
                self.import_busy = false;
                self.cancel = None;
                self.update_state = update::load_state();
                for line in report.lines() {
                    if !line.trim().is_empty() {
                        self.note(line.to_owned());
                    }
                }
                self.status = "导入完成".to_owned();
                self.import_report = Some(report);
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
                            self.note(format!("测速 {}：{} KB/s", s.label, s.kbps));
                            if !s.prefix.is_empty()
                                && fastest.as_ref().map(|(k, _)| s.kbps > *k).unwrap_or(true)
                            {
                                fastest = Some((s.kbps, s.prefix.clone()));
                            }
                        }
                        Some(e) => {
                            self.note(format!("测速 {}：不可用（{e}）", s.label))
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
                self.note(m.clone());
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
                    // 只对本次生效的开关，部署完就复位
                    self.allow_kernel_ac = false;
                    self.deploy_extras.clear();
                    if self.hags == gpu::HagsState::Disabled {
                        self.hags_prompt = true;
                    }
                }
            }
            Msg::Failed(e) => {
                self.note(format!("错误: {e}"));
                self.status = format!("错误: {e}");
                self.busy = false;
                self.progress = None;
                self.cancel = None;
            }
            Msg::DownloadFailed(e) => {
                self.note(format!("下载失败: {e}"));
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
                        self.note(m.clone());
                        self.status = m;
                    }
                    Err(e) => {
                        self.note(format!("显卡名操作失败: {e}"));
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
                        self.note(m.clone());
                        self.status = m;
                        self.new_version = Some(v);
                    }
                    Some(v) => {
                        // 远端版本没变，之前那个入口该撤掉
                        self.new_version = None;
                        let m = format!("已是最新版本（当前 v{}，远端 {v}）", update::SELF_VERSION);
                        self.note(m.clone());
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
                        self.note(m.clone());
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
                self.note(m.clone());
                self.status = m;
                self.busy = false;
                self.progress = None;
                self.cancel = None;
                self.download_failed = false;
            }
        }
    }

    /// 切换程序本体：上游最新版（根目录，310.9 后端）<-> 310.1 版（带 SM75 内核的代理版）。
    ///
    /// 两版同名文件的内容不同，所以切换时必须把下载记录清掉。不能只靠指纹：
    /// 探测落到镜像时指纹不可信，判定「已是最新」会退化成「比字节数」，
    /// 那时就会拿旧记录把 310.9 的文件当成 310.1 的，**静默跳过下载**，用户以为切了其实没切。
    /// 清掉记录不影响 DLSS 运行库（那两个是按本地文件签名判断的，不会重下）。
    fn set_legacy_3101(&mut self, on: bool) {
        if self.legacy_3101 == on {
            return;
        }
        self.legacy_3101 = on;
        self.save_config();
        let mut st = update::load_state();
        if !st.files.is_empty() {
            st.files.clear();
            if let Err(e) = update::save_state(&st) {
                self.note(format!("清空下载记录失败（下一次下载可能不会重新拉）: {e}"));
            }
        }
        if !scan::is_known_proxy(&self.proxy) {
            self.proxy = scan::PROXY_PRIORITY[0].to_owned();
        }
        self.redetect();
        self.status = if on {
            format!(
                "已切到 {}（给 RTX 20 / GTX 16 系）。点「下载 / 更新资产」重新下载（约 18 MB）。",
                update::LEGACY_PREFIX
            )
        } else {
            "已切回最新版。点「下载 / 更新资产」重新下载（约 17 MB）。".to_owned()
        };
        let s = self.status.clone();
        self.note(s);
    }

    fn save_config(&mut self) {
        let cfg = util::AppConfig {
            asset_dir: util::load_config().asset_dir,
            allow_backup_source: self.use_backup,
            backup_prefix: self.backup_prefix.clone(),
            legacy_3101: self.legacy_3101,
        };
        if let Err(e) = util::save_config(&cfg) {
            self.note(format!("保存配置失败: {e}"));
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
        cfg.legacy_3101 = self.legacy_3101;
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
                self.note(format!("旧资产目录仍保留在 {}，需要的话请手动处理。", o.display()));
            }
        }
    }

    // ---- 后台任务

    /// 手动导入：选中 zip / 文件夹 → 后台解包 + 逐个校验。
    fn start_import(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let work = match util::assets_dir() {
            Ok(d) => d.join(".import"),
            Err(e) => {
                self.status = format!("定位资产目录失败: {e}");
                return;
            }
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        self.busy = true;
        self.import_busy = true;
        self.status = "正在读取压缩包并校验（大包要几秒）...".to_owned();
        self.note(format!("开始手动导入：{} 个来源", paths.len()));
        let legacy = self.legacy_3101;
        self.spawn(move |tx, ctx| {
            let ptx = tx.clone();
            let r = importer::stage(&paths, legacy, &work, &cancel, move |m| {
                let _ = ptx.send(Msg::Progress(m, 0.0, 0));
            });
            let _ = tx.send(match r {
                Ok((items, notes)) => Msg::ImportStaged(items, notes),
                Err(e) => Msg::Failed(e.to_string()),
            });
            ctx.request_repaint();
        });
    }

    /// 校验能过的直接装；有可疑项时界面先弹窗问一句（跳过 / 强行导入）。
    fn finish_import(&mut self, items: Vec<importer::Staged>, include_untrusted: bool) {
        let keep: Vec<importer::Staged> = items
            .iter()
            .filter(|i| i.trusted || include_untrusted)
            .cloned()
            .collect();
        let skipped = items.len() - keep.len();
        if keep.is_empty() {
            importer::cleanup(&items);
            self.busy = false;
            self.import_busy = false;
            self.cancel = None;
            self.status = "没有可导入的文件".to_owned();
            return;
        }
        self.status = format!("正在写入 {} 个文件 ...", keep.len());
        let notes = self.import_notes.clone();
        self.spawn(move |tx, ctx| {
            let mut report = String::new();
            match importer::install(&keep) {
                Ok(_) => {
                    report.push_str(&format!("已导入 {} 个文件：\n", keep.len()));
                    for it in &keep {
                        report.push_str(&format!(
                            "  ✓ {}（{}，{}）来自 {}{}\n",
                            it.name,
                            it.kind.label(),
                            util::format_bytes(it.bytes),
                            it.from,
                            if it.trusted {
                                ""
                            } else {
                                "  ⚠ 你选择了强行导入"
                            }
                        ));
                    }
                }
                Err(e) => report.push_str(&format!("导入失败：{e}\n")),
            }
            if skipped > 0 {
                report.push_str(&format!(
                    "\n跳过 {skipped} 个（校验没过，按你的选择没有安装）：\n"
                ));
                for it in items.iter().filter(|i| !(i.trusted || include_untrusted)) {
                    report.push_str(&format!(
                        "  · {}（{}，来自 {}）{}\n",
                        it.name,
                        it.kind.label(),
                        it.from,
                        it.note
                    ));
                }
            }
            App::append_import_notes(&mut report, &notes);
            importer::cleanup(&items);
            let _ = tx.send(Msg::ImportDone(report));
            ctx.request_repaint();
        });
    }

    /// 结果清单里补上「跳过 / 说明」那一段（哪些文件没装、为什么）
    fn append_import_notes(report: &mut String, notes: &[String]) {
        if notes.is_empty() {
            return;
        }
        report.push_str("\n说明：\n");
        for n in notes {
            report.push_str(&format!("  · {n}\n"));
        }
    }

    fn start_scan(&mut self) {
        self.busy = true;
        self.status = "正在扫描 Steam / Epic / WeGame 游戏库...".to_owned();
        self.spawn(|tx, ctx| {
            // scan_all_notes 会把「谁被跳过、为什么」一并带回来（同时已经写进日志），
            // 用户报「扫不出来」时这就是唯一的线索。
            let (entries, notes) = scan::scan_all_notes();
            let rows: Vec<GameRow> = entries
                .into_iter()
                .map(|entry| App::build_row(entry, false))
                .collect();
            let _ = tx.send(Msg::Scanned(rows, notes));
            ctx.request_repaint();
        });
    }

    fn start_update_check(&mut self) {
        self.busy = true;
        self.status = "正在检查上游更新...".to_owned();
        let legacy = self.legacy_3101;
        self.spawn(move |tx, ctx| {
            let res = (|| -> anyhow::Result<UpdateSummary> {
                let c = update::client()?;
                let version = update::fetch_version(&c);
                let mut rows: Vec<AssetRow> = Vec::new();

                // 六个文件各发一次 HEAD 到 raw.githubusercontent.com 拿内容指纹。
                // 走的是 CDN，不占 api.github.com 那每小时 60 次的配额 ——
                // 配额被共享出口 IP 吃光正是之前「检查更新 / 下载」失败的原因。
                // 在用的那一版有哪些文件：6 个代理入口 + INI。
                // 名单写死过一次，上游把 altnative/ 改名成 alternatives/ 之后就全 404 了。
                let mut specs: Vec<String> = scan::PROXY_PRIORITY
                    .iter()
                    .map(|p| update::proxy_repo_path(p, legacy).to_owned())
                    .collect();
                specs.push(update::ini_repo_path(legacy).to_owned());
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
                                remote_etag_trusted: r.etag_trusted,
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
                        remote_etag_trusted: false,
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
        let legacy = self.legacy_3101;
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
                    update::proxy_repo_path(&proxy, legacy).to_owned(),
                    update::ini_repo_path(legacy).to_owned(),
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
                    let cur = update::local_is_current(&state, &local, &dest, &remote);
                    let rec = state.files.get(&local);
                    log::line(&format!(
                        "{local}: 探测 size={} etag={} 指纹可信={} | 记录 etag={:?} 记录字节={:?} | 文件在={} | 判定={}",
                        remote.size,
                        &remote.etag[..remote.etag.len().min(12)],
                        remote.etag_trusted,
                        rec.map(|r| r.etag.clone()),
                        rec.map(|r| r.bytes),
                        dest.is_file(),
                        if cur { "已是最新，跳过" } else { "需要下载" }
                    ));
                    if cur {
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
                            imported: false,
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
                    update::ensure_dlss_runtime(&c, &cancel, &plan, ctx, move |msg, f| {
                        let _ = tx2.send(Msg::Progress(msg, f, total));
                        ctx2.request_repaint();
                    })?;
                }

                log::line(&format!(
                    "本次下载结束：跳过 {skipped} 个已是最新的文件，下载目标合计 {} 字节",
                    total
                ));
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

        // 反作弊闸门：检出内核级时**先弹窗问一句**，而不是直接拦死。
        // 用户明确要求可以继续 —— 但默认动作仍然是「取消部署」，而且不管选哪个
        // 都会写进日志，将来真出问题能追溯。
        let ac = anticheat::scan_deep(&dir);
        let blocked = ac.is_blocked();
        if blocked && !self.allow_kernel_ac {
            self.ac_target = Some(ac.clone());
            self.deploy_extras = remove_extra;
            self.kernel_ac_pending = Some(ac);
            return;
        }
        self.ac_target = Some(ac);

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
            // 「部署成功却显示非本工具部署」这类投诉，看这一行就能定论：
            // 备份记录（manifest）到底写没写进去、能不能读回来。
            match &r {
                Ok(_) => log::line(&format!(
                    "部署完成：{} 代理={} 备份记录可读={}",
                    dir.display(),
                    proxy,
                    deploy::load_manifest(&dir).is_some()
                )),
                Err(e) => log::line(&format!("部署失败：{} {} ", dir.display(), e)),
            }
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
            match &r {
                Ok(_) => log::line(&format!(
                    "还原完成：{} 备份记录还在={}",
                    dir.display(),
                    deploy::load_manifest(&dir).is_some()
                )),
                Err(e) => log::line(&format!("还原失败：{} {e}", dir.display())),
            }
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
        self.note(format!(
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
                let (_, target_rect) = theme::card_rect(ui, |ui| {
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
                        // 存进游戏库：下次打开直接能在列表里点它，重新扫描也不会丢
                        if theme::ghost_button(
                            ui,
                            "存到游戏库",
                            !self.busy && self.game_dir.is_some(),
                        )
                        .on_hover_text("把这个目录记进游戏库，以后直接从列表里选；重新扫描也不会丢")
                        .clicked()
                        {
                            self.add_to_library();
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
                // 飞行动画的落点就是这个卡片（下一帧的选中动画要用）
                self.target_card_rect = Some(target_rect);

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
                        self.note(msg.clone());
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
                    // 网络实在下不动时的后路：网盘/U 盘拿到的 zip，在这里选它就行。
                    // 程序自己解压、递归找需要的文件、逐个校验，用户不用管目录结构。
                    ui.horizontal(|ui| {
                        if theme::ghost_button(ui, "导入压缩包…", !self.busy)
                            .on_hover_text(
                                "选中从网盘 / U 盘拿到的 zip（可多选，上游源码包 + 运行库包一起选）",
                            )
                            .clicked()
                        {
                            if let Some(files) = rfd::FileDialog::new()
                                .set_title("选择上游压缩包（可多选）")
                                .add_filter("压缩包", &["zip"])
                                .pick_files()
                            {
                                self.start_import(files);
                            }
                        }
                        if theme::ghost_button(ui, "导入文件夹…", !self.busy)
                            .on_hover_text("已经自己解压过的话，直接选那个文件夹")
                            .clicked()
                        {
                            if let Some(dir) = rfd::FileDialog::new()
                                .set_title("选择解压出来的文件夹")
                                .pick_folder()
                            {
                                self.start_import(vec![dir]);
                            }
                        }
                        if self.import_busy {
                            ui.add(egui::Spinner::new().size(12.0));
                        }
                    });

                    // ---- 下载源：先测速，再把结果摆出来让用户自己挑最快的
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("下载源").size(12.5).color(theme::TEXT));
                        if theme::ghost_button(ui, "测速", !self.busy)
                            .on_hover_text("对每个源各拉 512 KB（每个最多 4 秒），实测你这条线路上的真实速度")
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
                    // 一行下拉：源多的时候不再把界面铺成好几行。
                    // 「自动」时把当前实测最快的那个写进标题，用户一眼知道会用谁。
                    let selected_text = if current.is_empty() {
                        let fastest = self
                            .speed_results
                            .iter()
                            .filter(|s| !s.prefix.is_empty() && s.error.is_none())
                            .max_by_key(|s| s.kbps);
                        match fastest {
                            Some(s) => format!("自动（最快：{}  {} KB/s）", s.label, s.kbps),
                            None => "自动（按实测速度挑最快）".to_owned(),
                        }
                    } else if current.contains('\n') {
                        format!(
                            "指定：自定义 {} 个源",
                            current.lines().filter(|l| !l.trim().is_empty()).count()
                        )
                    } else {
                        format!("指定：{}", update::source_label(&current))
                    };
                    egui::ComboBox::from_id_salt("source-pick")
                        .width(320.0)
                        .selected_text(selected_text)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut choice,
                                String::new(),
                                "自动（按实测速度挑最快）",
                            );
                            for (prefix, label, speed) in &rows {
                                ui.selectable_value(
                                    &mut choice,
                                    prefix.clone(),
                                    format!("{label}   {speed}"),
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
                        "选中的源排最前面，其余镜像仍会兜底；下载中不会因为慢而换源，慢也让它慢慢下完。",
                    ));
                    ui.collapsing("自定义下载源（高级，一行一个）", |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.backup_prefix)
                                .desired_width(f32::INFINITY)
                                .desired_rows(3)
                                .hint_text("https://自己的镜像/\nhttps://再来一个/"),
                        );
                        ui.horizontal(|ui| {
                            if theme::ghost_button(ui, "用这些源", true).clicked() {
                                self.use_backup = !self.backup_prefix.trim().is_empty();
                                self.save_config();
                                self.status = "下载源已保存".to_owned();
                            }
                            if theme::ghost_button(ui, "改回自动", true).clicked() {
                                self.backup_prefix.clear();
                                self.use_backup = false;
                                self.save_config();
                                self.status =
                                    "已改回自动选源（按实测速度挑最快的）".to_owned();
                            }
                        });
                        ui.label(theme::hint(
                            "一行一个前缀，会拼在官方地址前面，按你填的顺序先试。填错也没关系：内容对不上会被自动拒绝。",
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

                    // RTX 20 / GTX 16 系（SM75）：上游 0.3.0 改回代理模式后只面向 RTX 30 系，
                    // 给这类用户一个切到 310.1 版的开关 —— 上游新版（310.9）没打包 SM75 内核。
                    if self.gpu_route == scan::GpuRoute::Sm75 {
                        ui.add_space(2.0);
                        if self.legacy_3101 {
                            theme::badge(ui, "正在用 310.1 版", theme::WARN);
                            ui.label(theme::hint(
                                "310.1 版是代理模式里仍然带 SM75 内核的那一份，给 RTX 20 / GTX 16 系用；倍率上限是 4X（新版 310.9 是 6X）。",
                            ));
                            if theme::ghost_button(ui, "改回最新版（上游代理包）", !self.busy).clicked()
                            {
                                self.set_legacy_3101(false);
                            }
                        } else {
                            ui.label(
                                egui::RichText::new(
                                    "你的显卡是 RTX 20 / GTX 16 系（Turing / SM75）。上游 0.3.0 改回代理模式后只面向 RTX 30 系，装了很可能不生效。",
                                )
                                .size(11.5)
                                .color(theme::WARN),
                            );
                            if theme::ghost_button(
                                ui,
                                "改用 310.1 版（支持 RTX 20 系）",
                                !self.busy,
                            )
                            .on_hover_text("下载上游仓库里的 310.1 版程序本体，约 18 MB；随时可以切回最新版")
                            .clicked()
                            {
                                self.set_legacy_3101(true);
                            }
                        }
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
                            ui.label(theme::hint(if self.ac_scanning {
                                "正在后台分析反作弊（不影响你继续操作）..."
                            } else {
                                "选择目录后自动检测。"
                            }));
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
                    let mut open_logs = false;
                    let mut copy_diag = false;
                    ui.horizontal(|ui| {
                        if theme::ghost_button(ui, "打开日志文件夹", true)
                            .on_hover_text("把日志文件发给开发者，问题基本一眼能看出来")
                            .clicked()
                        {
                            open_logs = true;
                        }
                        if theme::ghost_button(ui, "复制诊断信息", true)
                            .on_hover_text("把版本、系统、显卡识别、资产状态等信息复制到剪贴板")
                            .clicked()
                        {
                            copy_diag = true;
                        }
                    });
                    ui.label(theme::hint(format!(
                        "日志：{}　每次启动一个文件，最多保留 10 个。里面有本地路径和用户名，发给别人前先看一眼。",
                        log::path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "程序同级的 logs 目录".to_owned())
                    )));
                    if open_logs {
                        match log::dir() {
                            Some(d) => open_in_explorer(&d),
                            None => self.status = "打不开日志目录".to_owned(),
                        }
                    }
                    if copy_diag {
                        let text = self.diagnostic_text();
                        self.ctx.copy_text(text);
                        self.status = "诊断信息已复制到剪贴板".to_owned();
                    }
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
                if theme::ghost_button(ui, "扫描 Steam / Epic / WeGame", !self.busy).clicked() {
                    self.start_scan();
                }
                if self.scanned {
                    theme::badge(ui, &format!("{} 个", self.games.len()), theme::NEUTRAL);
                }
                if self.scanned && !self.scanned_at.is_empty() {
                    ui.label(theme::hint(format!("上次扫描 {}", self.scanned_at)));
                }
            });
            ui.add_space(10.0);

            if !self.scanned {
                ui.label(theme::hint(
                    "点「扫描 Steam / Epic / WeGame」列出已安装游戏。启动时不扫描，也不会后台轮询。WeGame 的判定比较保守（它的记录方式没有官方文档），扫不出来的话用「选择目录」手动指到游戏渲染 EXE 所在的文件夹即可。",
                ));
                return;
            }

            // 选中的那个游戏要飞向「目标目录」卡片，所以把名字和颜色一起记下来。
            // 起飞矩形要等卡片画完才有（card_rect 的返回值），所以单独存一个。
            let mut pick: Option<(PathBuf, String, egui::Color32)> = None;
            let mut pick_rect: Option<egui::Rect> = None;
            let mut open: Option<PathBuf> = None;
            let mut remove: Option<PathBuf> = None;

            egui::ScrollArea::vertical().show(ui, |ui| {
                for row in &self.games {
                    let color = theme::tier_color(row.ac);
                    let icon_key = row.entry.install_dir.display().to_string();
                    // 当前部署目标就是这个游戏 —— 列表里要一直看得出来
                    let selected = self.game_dir.as_deref() == Some(row.target.as_path());
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
                            if selected {
                                theme::badge(ui, "已选中", theme::ACCENT);
                            }
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
                        if row.manual {
                            // 手动条目存的就是这个目录本身，没有「渲染 EXE 是哪一级」的推断
                            ui.label(theme::hint(format!("手动添加   部署目标 {}", row.target.display())));
                        } else {
                            ui.label(match &row.render_exe {
                                Some(p) => theme::path_text(format!("渲染 EXE   {}", p.display())),
                                None => theme::hint("渲染 EXE   未找到（可手动选择其所在目录）"),
                            });
                        }
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            // 关键：部署目标必须是「渲染 EXE 所在目录」，不是游戏根目录。
                            // mod 文件放错地方游戏根本不会加载；早先这里传的是根目录，
                            // 导致选中后部署卡片去根目录找文件，一律显示「未部署」。
                            // 部署目标由行构建时算好存进 row.target：
                            // 扫出来的游戏是渲染 EXE 所在目录，手动条目就是用户存的那个目录。
                            let target_dir = row.target.clone();
                            // 手动条目即使找不到渲染 EXE 也能部署 —— 目录是用户自己指的
                            let has_exe = row.render_exe.is_some() || row.manual;
                            if theme::primary_button(ui, "用作部署目录", !self.busy && has_exe)
                                .on_hover_text(if row.manual {
                                    "把 mod 部署到你存进游戏库的这个目录"
                                } else if has_exe {
                                    "把 mod 部署到渲染 EXE 所在目录"
                                } else {
                                    "没找到渲染 EXE，请手动选择它所在目录"
                                })
                                .clicked()
                            {
                                pick = Some((target_dir.clone(), row.entry.name.clone(), color));
                            }
                            if theme::ghost_button(ui, "打开文件夹", true).clicked() {
                                open = Some(target_dir.clone());
                            }
                            if row.manual
                                && theme::ghost_button(ui, "移除", !self.busy).clicked()
                            {
                                remove = Some(row.entry.install_dir.clone());
                            }
                        });
                    });

                    if pick.is_some() {
                        pick_rect = Some(rect);
                    }

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

                    // 选中的那一行描一圈主题色边 —— 这是长期可见的「就是这个游戏」提示
                    if selected {
                        ui.painter().rect_stroke(
                            rect,
                            egui::CornerRadius::same(theme::R_CARD),
                            egui::Stroke::new(1.5, theme::ACCENT),
                            egui::StrokeKind::Inside,
                        );
                    }

                    ui.add_space(10.0);
                }
            });

            if let Some((p, name, color)) = pick {
                // 先切换目标目录（点下去立刻生效），再放动画
                self.set_game_dir(p);
                if let Some(r) = pick_rect {
                    self.start_fly(r, name, color);
                }
            }
            if let Some(p) = open {
                open_in_explorer(&p);
            }
            if let Some(p) = remove {
                self.remove_from_library(&p);
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

        // ---------------- 检出内核级反作弊时的确认（不再直接拦死）
        if let Some(ac) = self.kernel_ac_pending.clone() {
            let ctx = self.ctx.clone();
            let mut go = false;
            let mut cancel = false;
            egui::Window::new("该游戏检测到内核级反作弊")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(&ctx, |ui| {
                    ui.set_max_width(520.0);
                    for h in &ac.hits {
                        ui.label(
                            egui::RichText::new(format!("· {}（{}）", h.name, h.tier.label()))
                                .size(12.5)
                                .color(theme::DANGER)
                                .strong(),
                        );
                        if !h.evidence.is_empty() {
                            ui.label(theme::hint(h.evidence.clone()));
                        }
                    }
                    ui.add_space(6.0);
                    ui.label(
                        "修改游戏文件可能被内核级反作弊判定为异常，**存在封号风险**。这一点由你自己判断。",
                    );
                    ui.add_space(4.0);
                    ui.label(theme::hint(
                        "如果这个游戏你并不在意，或者它只是装了反作弊但你没在玩它，继续一般没问题。",
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        // 默认动作是取消，所以把「取消部署」放在显眼位置
                        if theme::primary_button(ui, "取消部署", true).clicked() {
                            cancel = true;
                        }
                        if theme::danger_button(ui, "仍要部署", true).clicked() {
                            go = true;
                        }
                    });
                });
            if go {
                self.kernel_ac_pending = None;
                self.allow_kernel_ac = true;
                let names: Vec<String> =
                    ac.hits.iter().map(|h| h.name.clone()).collect();
                self.note(format!(
                    "⚠ 用户确认在内核级反作弊（{}）的情况下继续部署，风险自负",
                    names.join("、")
                ));
                let extras = self.deploy_extras.clone();
                self.do_deploy(extras);
            } else if cancel {
                self.kernel_ac_pending = None;
                self.allow_kernel_ac = false;
                self.deploy_extras.clear();
                self.status = "已取消部署".to_owned();
                log::line("用户在内核级反作弊确认弹窗里选择了取消");
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

        // ---------------- 手动导入：可疑项的确认弹窗
        if let Some(items) = self.import_pending.clone() {
            let ctx = self.ctx.clone();
            let (mut skip, mut force, mut cancel) = (false, false, false);
            let bad: Vec<&importer::Staged> = items.iter().filter(|i| !i.trusted).collect();
            egui::Window::new("有文件没有通过校验")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(&ctx, |ui| {
                    ui.set_max_width(600.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "{} 个文件没能通过校验：没签名、内容被改过，或者签名证书不认识。",
                            bad.len()
                        ))
                        .size(13.0)
                        .color(theme::DANGER)
                        .strong(),
                    );
                    ui.add_space(6.0);
                    for it in &bad {
                        ui.label(
                            egui::RichText::new(format!("· {}（{}）", it.name, it.kind.label()))
                                .strong(),
                        );
                        ui.label(theme::hint(it.note.clone()));
                        ui.label(theme::path_text(format!(
                            "sha256 {}",
                            &it.sha256[..it.sha256.len().min(16)]
                        )));
                    }
                    ui.add_space(6.0);
                    ui.label(theme::hint(
                        "推荐只导入通过校验的那些。如果你确定这些文件是自己从可信来源拿的，也可以全部导入 —— 这个选择会写进日志。",
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if theme::primary_button(ui, "跳过可疑项", true).clicked() {
                            skip = true;
                        }
                        if theme::danger_button(ui, "全部仍然导入", true).clicked() {
                            force = true;
                        }
                        if theme::ghost_button(ui, "取消本次导入", true).clicked() {
                            cancel = true;
                        }
                    });
                });
            if skip {
                self.import_pending = None;
                self.note("手动导入：用户选择跳过校验没过的文件".to_owned());
                self.finish_import(items, false);
            } else if force {
                self.import_pending = None;
                let names: Vec<String> = bad
                    .iter()
                    .map(|i| format!("{}（sha256 {}）", i.name, i.sha256))
                    .collect();
                self.note(format!(
                    "⚠ 手动导入：用户选择强行导入校验没过的文件：{}",
                    names.join("、")
                ));
                self.finish_import(items, true);
            } else if cancel {
                self.import_pending = None;
                importer::cleanup(&items);
                self.busy = false;
                self.import_busy = false;
                self.cancel = None;
                self.status = "已取消导入".to_owned();
            }
        }

        // ---------------- 手动导入：结果清单
        if let Some(text) = self.import_report.clone() {
            let ctx = self.ctx.clone();
            let mut close = false;
            egui::Window::new("导入结果")
                .collapsible(false)
                .resizable(true)
                .default_width(620.0)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(&ctx, |ui| {
                    ui.set_max_width(660.0);
                    egui::ScrollArea::vertical()
                        .max_height(420.0)
                        .show(ui, |ui| {
                            ui.label(text.clone());
                        });
                    ui.add_space(8.0);
                    if theme::primary_button(ui, "知道了", true).clicked() {
                        close = true;
                    }
                });
            if close {
                self.import_report = None;
            }
        }

        // ---------------- 选中动画：卡片从游戏库飞向「目标目录」
        self.draw_fly_and_flash();
    }
}

impl App {
    /// 画飞行动画和落地高亮。只在动画期间请求重绘，动画结束立刻停 —— 不做常驻动画，
    /// 空闲时一帧都不多画（这个程序的卖点之一是轻量）。
    fn draw_fly_and_flash(&mut self) {
        if let Some(f) = &self.fly {
            let t = (f.t0.elapsed().as_secs_f32() / FLY_SECS).clamp(0.0, 1.0);
            if t >= 1.0 {
                self.fly = None;
            } else {
                // smoothstep：起步慢、中间快、落地缓
                let e = t * t * (3.0 - 2.0 * t);
                let pos = fly_lerp(f.from, f.to, e);
                let alpha = ((1.0 - 0.25 * e) * 235.0) as u8;
                let fill = egui::Color32::from_rgba_unmultiplied(
                    f.color.r(),
                    f.color.g(),
                    f.color.b(),
                    alpha,
                );
                let painter = self.ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("fgm-fly"),
                ));
                painter.rect_filled(pos, egui::CornerRadius::same(theme::R_CARD), fill);
                painter.rect_stroke(
                    pos,
                    egui::CornerRadius::same(theme::R_CARD),
                    egui::Stroke::new(1.5, f.color),
                    egui::StrokeKind::Inside,
                );
                // 方块太小的时候不写字，免得挤成一团
                if pos.height() >= 20.0 {
                    painter.text(
                        pos.center(),
                        egui::Align2::CENTER_CENTER,
                        &f.label,
                        egui::FontId::proportional(12.0),
                        egui::Color32::WHITE,
                    );
                }
                self.ctx.request_repaint();
            }
        }

        if let Some(until) = self.flash_until {
            if std::time::Instant::now() < until {
                if let Some(r) = self.target_card_rect {
                    let painter = self.ctx.layer_painter(egui::LayerId::new(
                        egui::Order::Foreground,
                        egui::Id::new("fgm-flash"),
                    ));
                    painter.rect_stroke(
                        r,
                        egui::CornerRadius::same(theme::R_CARD),
                        egui::Stroke::new(2.0, theme::ACCENT),
                        egui::StrokeKind::Inside,
                    );
                }
                self.ctx.request_repaint();
            } else {
                self.flash_until = None;
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
