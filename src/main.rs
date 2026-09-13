// 关闭控制台窗口（仅 release）。调试时保留，方便 println! 排查。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod anticheat;
mod deploy;
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
    // 无界面自检：cargo run -- --selftest
    if std::env::args().any(|a| a == "--selftest") {
        selftest();
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
        for (prefix, tag, _dll, label) in update::DLSS_RUNTIME {
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
        match update::ensure_dlss_runtime(&c, &cancel, |msg, f| {
            if f >= 0.999 {
                println!("  {msg}");
            }
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

    // 离线验证目录列举解析：cargo run -- --dirtest
    if std::env::args().any(|a| a == "--dirtest") {
        // 真实响应结构的样本（字段和 GitHub contents API 一致）
        let root = r#"[
          {"name":"README.md","path":"README.md","sha":"c3444a0f22937949150799285d0bf5f3a39b96c3","size":13831,"type":"file"},
          {"name":"altnative","path":"altnative","sha":"abc","size":0,"type":"dir"},
          {"name":".DS_Store","path":".DS_Store","sha":"2503f617b9e9536f5bac009fe93a0aab71a1e28e","size":8196,"type":"file"},
          {"name":"dlssg_sm86.ini","path":"dlssg_sm86.ini","sha":"f68878e557c6be4babf678d16efabe93fd3492c0","size":581,"type":"file"},
          {"name":"version.dll","path":"version.dll","sha":"50c04d7f4b","size":15667520,"type":"file"}
        ]"#;
        let alt = r#"[
          {"name":"winmm.dll","path":"altnative/winmm.dll","sha":"7166b7feef","size":15678272,"type":"file"},
          {"name":"dxgi.dll","path":"altnative/dxgi.dll","sha":"6291c9bd39","size":15668032,"type":"file"}
        ]"#;

        let mut items = update::parse_dir_listing(root);
        items.extend(update::parse_dir_listing(alt));
        println!("解析出 {} 个文件（dir 和非文件应被跳过）", items.len());
        for it in &items {
            println!("  name={:<26} sha={} size={}", it.name, it.blob_sha, it.size);
        }

        // 模拟界面里挑文件的过程
        println!("
界面挑文件：");
        for want in ["version.dll", "dlssg_sm86.ini"] {
            match items.iter().find(|r| r.name == want) {
                Some(r) => println!("  [OK]   {want:<16} -> 本地名 {}", update::local_name(&r.name)),
                None => println!("  [FAIL] {want} 没找到"),
            }
        }
        for want in ["winmm.dll", "dinput8.dll", "winhttp.dll", "dxgi.dll"] {
            match items.iter().find(|r| r.name.ends_with(want)) {
                Some(r) => println!("  [OK]   {want:<16} -> 本地名 {}", update::local_name(&r.name)),
                None => println!("  (缺失) {want}"),
            }
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
        let remote = match update::fetch_remote(&c, path) {
            Ok(r) => r,
            Err(e) => {
                println!("[FAIL] 取远端信息失败: {e}");
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
        match update::download(&c, &remote, &dest, &url, &cancel, |_, _| {}) {
            Ok(()) => {
                let data = std::fs::read(&dest).unwrap_or_default();
                let blob = util::git_blob_sha1(&data);
                println!(
                    "  [{}] 下载 {} 字节，blob sha = {}",
                    if blob == remote.blob_sha { "PASS" } else { "FAIL" },
                    data.len(),
                    blob
                );
                println!("  期望 sha   = {}", remote.blob_sha);
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

    println!("\n--- 上游更新检查（注意：上游无 Releases，走 contents API 的 blob sha） ---");
    match update::client() {
        Ok(c) => {
            println!("  README 版本: {:?}", update::fetch_version(&c));
            let st = update::load_state();
            // 和界面里一样：两次目录列举代替六个单文件查询
            match update::fetch_dir_listing(&c, "") {
                Ok(root) => {
                    let alt = update::fetch_dir_listing(&c, "altnative").unwrap_or_default();
                    println!(
                        "  目录列举：根 {} 个文件 / altnative {} 个文件（共 2 次 API 调用）",
                        root.len(),
                        alt.len()
                    );
                    let wanted = [
                        "version.dll",
                        "dlssg_sm86.ini",
                        "winmm.dll",
                        "dinput8.dll",
                        "winhttp.dll",
                        "dxgi.dll",
                    ];
                    for r in root.iter().chain(alt.iter()) {
                        let local = update::local_name(&r.name);
                        if !wanted.contains(&local.as_str()) {
                            continue;
                        }
                        let short = &r.blob_sha[..r.blob_sha.len().min(10)];
                        println!(
                            "  {:<22} sha={} size={:>9} 需要下载={}",
                            local,
                            short,
                            r.size,
                            st.needs_update(&local, &r.blob_sha)
                        );
                    }
                }
                Err(e) => println!("  目录列举失败: {e}"),
            }

            println!("
  DLSS 运行库（界面里会单独分组显示）：");
            for (prefix, tag, dll_name, _label) in update::DLSS_RUNTIME {
                match update::find_release_zip(&c, update::DLSS_REPO, prefix, tag) {
                    Ok(a) => {
                        let dest = update::asset_path(dll_name).unwrap_or_default();
                        let ready = dest.is_file()
                            && scan::identify_dll(&dest) == scan::FileIdentity::Nvidia;
                        println!(
                            "  {:<22} {}  下载 {}  本地={}",
                            dll_name,
                            a.tag,
                            util::format_bytes(a.size),
                            if ready { "已就绪(NVIDIA 签名)" } else { "未下载" }
                        );
                    }
                    Err(e) => println!("  {:<22} 查询失败: {e}", dll_name),
                }
            }
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
    let remote = match update::fetch_remote(&c, update::INI_REPO_PATH) {
        Ok(r) => r,
        Err(e) => {
            println!("[FAIL] 查询远端失败: {e}");
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

    // 一开始就把取消标志置上，下载循环应当在第一次读取前就退出
    let cancel = AtomicBool::new(true);
    let url = update::official_url(update::INI_REPO_PATH);
    let r = update::download(&c, &remote, &dest, &url, &cancel, |_, _| {});

    println!(
        "  下载结果: {}",
        match &r {
            Ok(()) => "意外成功了".to_owned(),
            Err(e) => format!("如期失败 -> {e}"),
        }
    );
    println!("  [{}] 目标文件未被创建", if dest.exists() { "FAIL" } else { "PASS" });
    println!("  [{}] 没有 .part 残留", if part.exists() { "FAIL" } else { "PASS" });

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
    let remote = match update::fetch_remote(&c, repo_path) {
        Ok(r) => r,
        Err(e) => {
            println!("[FAIL] 查询远端失败: {e}");
            return;
        }
    };
    println!("  远端 {} sha={} size={}", remote.name, remote.blob_sha, remote.size);

    let dest = match update::asset_path(&update::local_name(repo_path)) {
        Ok(d) => d,
        Err(e) => {
            println!("[FAIL] 定位目标失败: {e}");
            return;
        }
    };

    let url = update::official_url(repo_path);
    let cancel = AtomicBool::new(false);
    match update::download(&c, &remote, &dest, &url, &cancel, |got, total| {
        if total > 0 && got >= total {
            println!("  已下载 {got} / {total} 字节");
        }
    }) {
        Ok(()) => {
            let data = std::fs::read(&dest).unwrap_or_default();
            let blob = util::git_blob_sha1(&data);
            let ok = blob == remote.blob_sha;
            println!("  [{}] 下载内容 blob sha = {}", if ok { "PASS" } else { "FAIL" }, blob);
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

    check!(
        deploy::state_of(&target) == deploy::DeployState::NotDeployed,
        "初始状态 = 未部署"
    );

    let dep_files = [
        deploy::DeployFile::new("version.dll", &dll_src),
        deploy::DeployFile::new(deploy::INI_NAME, &ini_src),
    ];
    match deploy::deploy(&target, "version.dll", &dep_files) {
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

    // 冲突：目录里已有第三方的 version.dll
    println!("
-- 入口冲突 --");
    let t2 = root.join("conflict");
    let _ = fs::create_dir_all(&t2);
    fs::write(t2.join("version.dll"), b"SOME_OTHER_MOD").unwrap();
    check!(
        deploy::deploy(&t2, "version.dll", &dep_files).is_err(),
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
        deploy::deploy(&t2, "winmm.dll", &dep_files_alt).is_ok(),
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
            let r = deploy::deploy(&t5, "version.dll", &dep_files);
            check!(r.is_ok(), "目标已有本项目文件时允许覆盖（不再误拒）");
            let _ = deploy::restore(&t5);
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
    /// 核心 Mod：远端 blob sha，用来和本地下载记录比对
    remote_blob: Option<String>,
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
    Progress(String, f32),
    Done(String),
    Failed(String),
    /// 下载失败。单独一个变体，是为了在界面上给出「改用备用源」的提示。
    DownloadFailed(String),
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

    // ---- 代理入口推荐
    advice: Option<scan::ProxyAdvice>,

    // ---- 显卡
    gpu_name: Option<String>,
    gpu_route: scan::GpuRoute,

    // ---- 下载控制
    cancel: Option<Arc<AtomicBool>>,
    use_backup: bool,
    backup_prefix: String,
    download_failed: bool,

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
        let gpu_name = scan::detect_gpu();
        let gpu_route = gpu_name
            .as_deref()
            .map(scan::classify_gpu)
            .unwrap_or(scan::GpuRoute::Unknown);
        let cfg = util::load_config();

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
            status: if cleaned > 0 {
                format!("就绪（已清理 {cleaned} 个未完成的下载残留）")
            } else {
                "就绪".to_owned()
            },
            progress: None,
            busy: false,
            logs: Vec::new(),
            autoscan_done: false,
            advice: None,
            gpu_name,
            gpu_route,
            cancel: None,
            use_backup: cfg.allow_backup_source,
            // 配置里没填过就用内置备用源，省得用户自己去查网址
            backup_prefix: if cfg.backup_prefix.trim().is_empty() {
                update::DEFAULT_BACKUP_PREFIX.to_owned()
            } else {
                cfg.backup_prefix
            },
            download_failed: false,
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

    /// 资产当前状态。**完全按本地文件判断，不查网络** ——
    /// 所以下载完立刻就能显示「已就绪」，也不必再消耗 GitHub 的调用配额。
    fn asset_state(&self, row: &AssetRow) -> AssetState {
        if let Some(name) = &row.runtime_file {
            let ok = util::assets_dir()
                .map(|d| {
                    let p = d.join(name);
                    p.is_file() && scan::identify_dll(&p) == scan::FileIdentity::Nvidia
                })
                .unwrap_or(false);
            return if ok {
                AssetState::Ready
            } else {
                AssetState::Missing
            };
        }
        match (&row.remote_blob, self.update_state.files.get(&row.label)) {
            (Some(remote), Some(local)) if local.blob_sha == *remote => AssetState::Ready,
            (Some(_), Some(_)) => AssetState::Outdated,
            _ => AssetState::Missing,
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
            Msg::Progress(text, f) => {
                self.progress = Some((text, f));
            }
            Msg::Done(m) => {
                self.logs.push(m.clone());
                self.status = m;
                self.busy = false;
                self.progress = None;
                self.cancel = None;
                self.download_failed = false;
                self.refresh();
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
        }
    }

    fn save_config(&mut self) {
        let cfg = util::AppConfig {
            asset_dir: util::load_config().asset_dir,
            allow_backup_source: self.use_backup,
            backup_prefix: self.backup_prefix.clone(),
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

                // 用两次目录列举代替六个单文件查询：contents API 每小时只有 60 次配额，
                // 列目录一次就能拿到该目录下所有文件的 blob sha。
                let root = update::fetch_dir_listing(&c, "")?;
                let alt = update::fetch_dir_listing(&c, "altnative").unwrap_or_default();

                let mut specs: Vec<(String, update::RemoteFile)> = Vec::new();
                for want in ["version.dll", update::INI_REPO_PATH] {
                    if let Some(r) = root.iter().find(|r| r.name == want) {
                        specs.push((want.to_owned(), r.clone()));
                    }
                }
                for want in ["winmm.dll", "dinput8.dll", "winhttp.dll", "dxgi.dll"] {
                    if let Some(r) = alt.iter().find(|r| r.name.ends_with(want)) {
                        specs.push((update::local_name(&r.name), r.clone()));
                    }
                }

                for (local, r) in specs {
                    rows.push(AssetRow {
                        group: "核心 Mod",
                        label: local,
                        detail: format!("sha {}", &r.blob_sha[..r.blob_sha.len().min(8)]),
                        bytes: r.size,
                        remote_blob: Some(r.blob_sha.clone()),
                        runtime_file: None,
                    });
                }

                // DLSS 运行库：两个 zip release
                for (prefix, tag, dll_name, label) in update::DLSS_RUNTIME {
                    let asset = update::find_release_zip(&c, update::DLSS_REPO, prefix, tag)?;
                    let dest = update::asset_path(dll_name)?;
                    let ready = dest.is_file()
                        && scan::identify_dll(&dest) == scan::FileIdentity::Nvidia;
                    let _ = (ready, label);
                    rows.push(AssetRow {
                        group: "DLSS 运行库",
                        label: dll_name.to_owned(),
                        detail: format!("{}（{}）", asset.tag, label),
                        bytes: asset.size,
                        remote_blob: None,
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
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        self.busy = true;
        self.download_failed = false;
        self.status = if use_backup {
            "正在从备用源下载并校验资产...".to_owned()
        } else {
            "正在下载并校验资产...".to_owned()
        };

        self.spawn(move |tx, ctx| {
            let res = (|| -> anyhow::Result<String> {
                let c = update::client()?;
                let mut state = update::load_state();
                let specs = [
                    update::proxy_repo_path(&proxy).to_owned(),
                    update::INI_REPO_PATH.to_owned(),
                ];
                for path in specs {
                    let remote = update::fetch_remote(&c, &path)?;
                    let local = update::local_name(&path);
                    let dest = update::asset_path(&local)?;

                    let official = update::official_url(&path);
                    let url = if use_backup {
                        format!("{prefix}{official}")
                    } else {
                        official
                    };

                    let tx2 = tx.clone();
                    let ctx2 = ctx.clone();
                    let label = local.clone();
                    update::download(&c, &remote, &dest, &url, &cancel, move |got, total| {
                        let f = if total > 0 {
                            got as f32 / total as f32
                        } else {
                            0.0
                        };
                        let _ = tx2.send(Msg::Progress(
                            format!(
                                "下载 {} {} / {}",
                                label,
                                util::format_bytes(got),
                                util::format_bytes(total)
                            ),
                            f,
                        ));
                        ctx2.request_repaint();
                    })?;

                    let data = std::fs::read(&dest)?;
                    state.files.insert(
                        local,
                        update::LocalFile {
                            blob_sha: remote.blob_sha.clone(),
                            sha256: util::sha256_hex(&data),
                            bytes: data.len() as u64,
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
                    update::ensure_dlss_runtime(&c, &cancel, move |msg, f| {
                        let _ = tx2.send(Msg::Progress(msg, f));
                        ctx2.request_repaint();
                    })?;
                }

                Ok(format!(
                    "资产已下载并校验完成 -> {}",
                    util::assets_dir()?.display()
                ))
            })();
            let _ = tx.send(match res {
                Ok(m) => Msg::Done(m),
                Err(e) => Msg::DownloadFailed(e.to_string()),
            });
            ctx.request_repaint();
        });
    }

    fn cancel_download(&mut self) {
        if let Some(c) = &self.cancel {
            c.store(true, Ordering::Relaxed);
            self.status = "正在取消下载...".to_owned();
        }
    }

    fn start_deploy(&mut self) {
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
        for (_prefix, _tag, dll_name, label) in update::DLSS_RUNTIME {
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
        self.status = "正在部署...".to_owned();
        self.spawn(move |tx, ctx| {
            let r = deploy::deploy(&dir, &proxy, &files);
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
}

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
        if self.busy {
            self.ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }

        // 调试开关：只为截图/排查用，正常启动不受影响
        if !self.autoscan_done && std::env::var_os("DLSSG_AUTOSCAN").is_some() {
            self.autoscan_done = true;
            self.start_scan();
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
                            });
                        }
                        None => {
                            ui.label(theme::hint(
                                "读不到显卡信息，将按上游默认 SM86 处理，请自行确认。",
                            ));
                        }
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

                    // 当前是否在用备用源 —— 明确显示，并能一键切回官方
                    if self.use_backup {
                        ui.horizontal(|ui| {
                            theme::badge(ui, "走备用源", theme::WARN);
                            ui.label(theme::hint(self.backup_prefix.clone()));
                            if theme::ghost_button(ui, "改回官方源", !self.busy).clicked() {
                                self.use_backup = false;
                                self.save_config();
                                self.status = "已改回官方下载源".to_owned();
                            }
                        });
                    }

                    // 进度条就放在按钮下面
                    if let Some((text, f)) = self.progress.clone() {
                        ui.add(egui::ProgressBar::new(f).text(text));
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
                                let st = self.asset_state(&row);
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
                        egui::ComboBox::from_label("")
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
