//! FrameGen Manager 的 Electron sidecar：Tauri 版那 28 个命令的**独立进程**外壳。
//!
//! 为什么要有它：Electron 主进程是 JS，碰不到 Rust，而业务逻辑（扫描 / 部署 /
//! 下载 / 还原 / 反作弊）全在 framegen-core 里，一行都不能重写。所以这里只做两件事：
//!
//!   1. 把 apps/desktop/src/main.rs 里的每个命令**原样**包一层：
//!      「app: tauri::AppHandle」换成「id: u64」的进度回调，其余调用顺序、参数校验、
//!      返回字段一字不改（前端 ui/app.js 认的就是那些字段名）。
//!   2. 用「一行一个 JSON 对象」的协议和 Electron 说话（对端见 apps/electron/main.js）。
//!
//! 协议：
//!   请求  {"id":1,"cmd":"list_games","args":{}}
//!   成功  {"id":1,"ok":true,"result":{…}}   // result 就是原来 Tauri 命令的返回值
//!   失败  {"id":1,"ok":false,"error":"中文原因"}
//!   进度  {"id":1,"event":"progress","payload":{"msg":"…","fraction":0.42}}
//!
//! **这个 exe 必须和主程序放在同一个目录里运行**：core 的配置 / assets / backups /
//! logs / 游戏库全按 util::exe_dir()（当前 exe 所在目录）解析，换个目录跑就会去读写
//! 另一套数据。也正因为如此，这里**不加** --app-dir 之类的参数 —— 和 Tauri 版一致。
//!
//! 阻塞式实现就够了：这是个独立进程，协议本来就是「一条请求等一条响应」，
//! 同一时刻只可能有一条命令在跑，再套一层 spawn_blocking 只是白白多一层线程。
//! 代价是命令执行期间不读 stdin —— 无所谓，Electron 那边本来就在等回复。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};

// ---------------------------------------------------------------- 协议出入口

/// stdout 是协议的唯一出口，所有行都从这里出去。
///
/// 加锁：目前是单线程，锁其实用不上，但协议最怕的就是两行 JSON 交叉写在一起
/// （对端按行解析，一交叉就整行报废），所以留一道保险。
/// **每行写完立刻 flush** —— Electron 是逐行读的，攒在缓冲区里就等于命令永远不返回。
fn write_line(v: &Value) {
    use std::io::Write;
    static OUT: Mutex<()> = Mutex::new(());
    let _g = OUT.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

/// 成功行。result 直接就是原来 Tauri 命令的返回值，不做任何包装 ——
/// 前端是按字段名取的（比如 r.pending / r.notes / r.canceled），多一层就全废。
fn ok_line(id: u64, result: Value) -> Value {
    json!({ "id": id, "ok": true, "result": result })
}

/// 失败行。error 是给人看的字符串，Electron 会直接把它当异常消息抛给界面。
fn err_line(id: u64, msg: &str) -> Value {
    json!({ "id": id, "ok": false, "error": msg })
}

/// 进度事件。core 的回调签名是 FnMut(String, f32)，这里统一写成一行 event JSON。
/// 不另开线程、不缓冲：命令就在主循环里同步跑，写出去的顺序天然和调用顺序一致。
fn progress(id: u64, msg: &str, fraction: f32) {
    write_line(&json!({
        "id": id,
        "event": "progress",
        "payload": { "msg": msg, "fraction": fraction as f64 },
    }));
}

// ---------------------------------------------------------------- 基础信息

fn gpu() -> String {
    framegen_core::gpu::nvidia_smi_gpu_name().unwrap_or_else(|| "（读不到显卡）".to_string())
}

/// 显卡 / 驱动 / 硬件加速 GPU 计划 / **路由判定**
///
/// 型号名走 core 的 detect_gpu（先问 nvidia-smi，回退时只用设备树里的适配器）——
/// 自己写「取第一个名字带 NVIDIA 的条目」会被旧卡留下的幽灵条目骗到，路由就会判错
/// （3050 被认成 40 系然后禁止部署，就是这么来的）。
/// Tauri 版这里是「异步 + spawn_blocking」：detect_gpu 要起 nvidia-smi.exe，
/// 而同步命令跑在 Tauri 主线程上会把 WebView 冻住。sidecar 没有 WebView，
/// 这一层也就没有存在的理由 —— 直接同步跑。
/// gpu_info 的短缓存。
///
/// 为什么加：detect_gpu 与 detect_driver 都要起一次 nvidia-smi.exe 进程 ——
/// 实测这条命令 105 ms/次（对比 list_games 3.6 ms），是界面里最慢的一条。
/// 而且下面原来把 detect_driver 调了**两次**（第二次只是取 source），
/// 等于一次请求起 3 个进程。显卡型号和驱动版本在运行期间不会变，所以缓存 20 秒；
/// 伪装应用/还原之后会主动作废。
static GPU_CACHE: std::sync::Mutex<Option<(std::time::Instant, Value)>> =
    std::sync::Mutex::new(None);
const GPU_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(20);

fn gpu_cache_clear() {
    if let Ok(mut g) = GPU_CACHE.lock() {
        *g = None;
    }
}

fn gpu_info(force: bool, fake_name: Option<String>) -> Result<Value, String> {
    if !force {
        if let Ok(g) = GPU_CACHE.lock() {
            if let Some((t, v)) = g.as_ref() {
                if t.elapsed() < GPU_CACHE_TTL {
                    return Ok(v.clone());
                }
            }
        }
    }
    let adapters = framegen_core::gpu::enumerate();
    // fake_name 只给开发/验证用（--fgm-fake-gpu）：拿一个假型号走**同一套真实分类逻辑**，
    // 好把「启动时的显卡警告弹窗」在没有那块卡的机器上也验证一遍。正常启动不传。
    let name = match fake_name.filter(|s| !s.trim().is_empty()) {
        Some(n) => n,
        None => framegen_core::scan::detect_gpu().unwrap_or_else(|| "（读不到）".to_string()),
    };
    let route = framegen_core::scan::classify_gpu(&name);
    // 只问一次 nvidia-smi（detect_driver 内部起进程）
    let drv = framegen_core::gpu::detect_driver(&adapters);
    let driver = drv
        .as_ref()
        .map(|d| d.marketing.clone())
        .unwrap_or_else(|| "未知".to_string());
    let driver_src = drv.as_ref().map(|d| d.source).unwrap_or("未知");
    let out = json!({
        "name": name,
        "driver": driver,
        "route": route.label(),
        "routeKey": route.key(),
        // null = 放行；有字符串 = 禁止部署，字符串本身就是给用户看的理由
        "gate": route.gate(),
        "hags": framegen_core::gpu::hags_state().label(),
        "driverSrc": driver_src,
    });
    if let Ok(mut g) = GPU_CACHE.lock() {
        *g = Some((std::time::Instant::now(), out.clone()));
    }
    Ok(out)
}

// ---------------------------------------------------------------- 资产

/// 标准 base64。core 里没有编码器（它不需要），前端要的是 data URL，
/// 所以这段小实现从 Tauri 版原样搬过来 —— 别为了「省事」换成十六进制，
/// 前端拼的是 data:image/jpeg;base64, 这个前缀。
fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

/// 找封面目录：优先用户实际的资产目录，再退回几个常见位置。
///
/// 封面按 <appid>.jpg 命名（Steam 网格图那套），前端拿 appId 直接查表。
/// 后面那两条写死的路径是开发/测试期的残留，和 Tauri 版保持一致 ——
/// 删掉的话 test-tauri 那种「资产目录单独放」的场景就取不到封面了。
fn cover_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(a) = framegen_core::util::assets_dir() {
        out.push(a.join("covers"));
    }
    if let Some(d) = framegen_core::util::exe_dir() {
        out.push(d.join("assets").join("covers"));
    }
    out.push(PathBuf::from(r"D:\FrameGen Manager\assets\covers"));
    out.push(PathBuf::from(r"D:\AI Work\FrameGen Manager\test-dlss5\assets\covers"));
    out.push(PathBuf::from(r"D:\AI Work\FrameGen Manager\test-tauri\assets\covers"));
    out
}

/// 封面（<appid>.jpg → base64 data URL）
fn covers() -> Vec<Value> {
    let mut out = Vec::new();
    for d in cover_dirs() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Some(n) = p.file_name().map(|x| x.to_string_lossy().to_string()) else { continue };
            if !n.to_ascii_lowercase().ends_with(".jpg") {
                continue;
            }
            let Ok(b) = std::fs::read(&p) else { continue };
            out.push(json!({
                "id": n[..n.len() - 4].to_string(),
                "data": format!("data:image/jpeg;base64,{}", b64(&b)),
            }));
        }
        if !out.is_empty() {
            break;
        }
    }
    out
}

/// 资产目录里的文件清单（真实存在与否 / 大小）
/// 给若干进程打开 Windows 的「效率模式」（EcoQoS / Power Throttling）。
///
/// 用户要求：软件常驻低开销、效率模式**一直开**（不分前后台）。Electron 自己没有这个开关，
/// 所以让 sidecar 代劳 —— 同用户进程之间，只要拿到 PROCESS_SET_INFORMATION 句柄就能设。
/// 纯 FFI，不引第三方 crate（这个项目一直保持零额外依赖）。
#[cfg(windows)]
fn set_efficiency(pids: &[u32]) -> (usize, usize) {
    use std::os::raw::{c_int, c_void};
    #[repr(C)]
    struct Throttling { version: u32, control_mask: u32, state_mask: u32 }
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: c_int, pid: u32) -> *mut c_void;
        fn SetProcessInformation(h: *mut c_void, class: c_int, info: *mut c_void, size: u32) -> c_int;
        fn CloseHandle(h: *mut c_void) -> c_int;
    }
    const SET_INFORMATION: u32 = 0x0200;
    const QUERY_LIMITED: u32 = 0x1000;
    const CLASS_POWER_THROTTLING: c_int = 4;   // ProcessPowerThrottling
    const EXECUTION_SPEED: u32 = 0x1;          // PROCESS_POWER_THROTTLING_EXECUTION_SPEED
    let mut ok = 0usize;
    for &pid in pids {
        unsafe {
            let h = OpenProcess(SET_INFORMATION | QUERY_LIMITED, 0, pid);
            if h.is_null() { continue; }
            let mut st = Throttling { version: 1, control_mask: EXECUTION_SPEED, state_mask: EXECUTION_SPEED };
            if SetProcessInformation(h, CLASS_POWER_THROTTLING, &mut st as *mut _ as *mut c_void, std::mem::size_of::<Throttling>() as u32) != 0 {
                ok += 1;
            }
            CloseHandle(h);
        }
    }
    (ok, pids.len())
}
#[cfg(not(windows))]
fn set_efficiency(pids: &[u32]) -> (usize, usize) { (0, pids.len()) }

/// 自更新下载的取消标志。用户点「停止更新」时置位；core 的 stage_self_update 每收一块都会看它，
/// 取消后由这里把半截文件删掉（用户明确要求：停止后不能留残余）。
static SELFUPDATE_CANCEL: std::sync::OnceLock<std::sync::Arc<std::sync::atomic::AtomicBool>> =
    std::sync::OnceLock::new();
fn selfupdate_cancel() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    SELFUPDATE_CANCEL
        .get_or_init(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)))
        .clone()
}

fn assets_status() -> Value {
    let dir = framegen_core::util::assets_dir().unwrap_or_default();
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() {
                files.push(json!({
                    "name": p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    "size": e.metadata().map(|m| m.len()).unwrap_or(0),
                }));
            }
        }
    }
    files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    // 界面按「这一类是什么」分组列（帧生成配置 / 代理入口 / 帧生成运行库 / 超分运行库）。
    // 为什么分组：平铺时记录文件（update_state.json）和代理 DLL 会混进「运行库」里，
    // 看着莫名其妙（用户反馈）。名字全部取自 core 的常量，界面不抄第二份。
    let size_of = |name: &str| std::fs::metadata(dir.join(name)).map(|m| m.len()).unwrap_or(0);
    let ini = framegen_core::update::INI_REPO_PATH;
    let ini_present = dir.join(ini).is_file();
    let mut missing = 0usize;
    if !ini_present {
        missing += 1;
    }

    // 代理入口：**全部列出来**，每个都实时标已有/缺少（用户要求能一眼看出哪个有、哪个缺）。
    // 部署时只要有一个就行，所以"缺几个"不计入 missing（missing 只数必备且唯一的东西）。
    let mut proxy_files: Vec<Value> = Vec::new();
    let mut any_proxy = false;
    for n in framegen_core::scan::PROXY_PRIORITY.iter() {
        let present = dir.join(n).is_file();
        if present {
            any_proxy = true;
        }
        proxy_files.push(json!({
            "name": n,
            "present": present,
            "size": size_of(n),
            "hint": if n == &framegen_core::scan::PROXY_PRIORITY[0] { "上游默认入口" } else { "" },
        }));
    }
    if !any_proxy {
        missing += 1;
    }

    let mut fg_files: Vec<Value> = Vec::new();
    let mut sr_files: Vec<Value> = Vec::new();
    for (_prefix, _tag, _zip, dll, _label) in framegen_core::update::DLSS_RUNTIME {
        let present = dir.join(dll).is_file();
        if !present {
            missing += 1;
        }
        let item = json!({ "name": dll, "present": present, "size": size_of(dll), "hint": "" });
        // nvngx_dlssg = 帧生成，nvngx_dlss = 超分
        if dll.contains("dlssg") {
            fg_files.push(item);
        } else {
            sr_files.push(item);
        }
    }

    let groups = json!([
        { "title": "帧生成配置", "files": [ json!({ "name": ini, "present": ini_present, "size": size_of(ini), "hint": "" }) ] },
        { "title": "帧生成 Mod 代理入口", "files": proxy_files },
        { "title": "DLSS 帧生成运行库", "files": fg_files },
        { "title": "DLSS 超分运行库", "files": sr_files },
    ]);

    json!({
        "dir": dir.display().to_string(),
        "files": files,
        "groups": groups,
        "missing": missing,
    })
}

/// 备份目录（界面上显示）
fn backups_dir() -> String {
    framegen_core::util::backups_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// 日志尾部（给「复制诊断信息」用）
fn log_tail(lines: usize) -> String {
    let Some(p) = framegen_core::log::path() else { return String::new() };
    let Ok(t) = std::fs::read_to_string(p) else { return String::new() };
    let all: Vec<&str> = t.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// 用资源管理器打开一个目录（不存在就打开它的父目录）。
///
/// Tauri 版起进程一律 spawn_blocking（同步命令跑主线程时 WebView 会白屏）；
/// 这里没有 WebView，直接起。返回值是 null —— 和 Tauri 版的 Result<(), _> 一致。
fn open_path(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    let target = if p.is_dir() { p } else { p.parent().unwrap_or(p) };
    std::process::Command::new("explorer")
        .arg(target)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 在系统浏览器里打开一个链接（发布页那类）。
///
/// **只放行 http/https**：这个命令的入参最终会交给系统去「打开」，
/// 放行 file: 或自定义协议等于给了一个任意执行的口子。
/// 用 rundll32 而不是 cmd /c start：后者会把 URL 里的 & 当命令分隔符。
fn open_url(url: String) -> Result<(), String> {
    let u = url.trim().to_string();
    if !(u.starts_with("https://") || u.starts_with("http://")) {
        return Err(format!("只允许打开 http/https 链接（收到 {u}）"));
    }
    std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", &u])
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 更换资产目录（自己弹文件框）—— 改完写进配置，core 的缓存立刻失效。
///
/// 弹框这条路 Tauri 版是放在 spawn_blocking 里避免冻界面；sidecar 是独立进程，
/// 弹框期间这个进程本来就在等用户，主循环停在这儿正是我们要的语义。
fn pick_assets_dir() -> Result<Value, String> {
    let picked = rfd::FileDialog::new()
        .set_title("选择资产目录（存放代理 DLL 与配置）")
        .pick_folder();
    let Some(dir) = picked else {
        return Ok(json!({ "canceled": true }));
    };
    let mut cfg = framegen_core::util::load_config();
    cfg.asset_dir = Some(dir.clone());
    framegen_core::util::save_config(&cfg).map_err(|e| e.to_string())?;
    Ok(json!({ "canceled": false, "dir": dir.display().to_string() }))
}

// ---------------------------------------------------------------- 游戏库

/// 缓存里的一行 -> 前端要的形状。
///
/// 关键是 target：mod 文件要落在「渲染 EXE 所在目录」，不是游戏根目录。
/// 手动加的条目例外 —— 用户选的那个目录就是部署目标，不去猜是哪一级。
fn game_row(c: &framegen_core::scan::CachedRow, manual: bool) -> Value {
    let install = c.entry.install_dir.clone();
    let target = if manual {
        install.clone()
    } else {
        c.render_exe
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.to_path_buf())
            .unwrap_or_else(|| install.clone())
    };
    let source = serde_json::to_value(c.entry.source)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "未知".to_string());
    let ac = c.ac;
    json!({
        "name": c.entry.name,
        "dir": install.display().to_string(),
        "target": target.display().to_string(),
        "source": source,
        "appId": c.entry.app_id,
        "api": c.api.label(),
        "engine": c.engine.label(),
        "streamline": c.streamline,
        "techScanned": c.tech_scanned,
        "ac": ac.label(),
        "acKernel": ac == framegen_core::anticheat::AcTier::Kernel,
        "acAny": ac != framegen_core::anticheat::AcTier::None,
        "manual": manual,
        "exists": install.is_dir(),
        "deployed": framegen_core::deploy::state_of(&target).label(),
        // DLSS5 那一套的状态与它要用的启动 exe：部署对话框直接把这两项显示出来，
        // 免得再单独发一条命令去问（每行多读一个小 JSON，可以忽略）。
        "dlss5": framegen_core::dlss5::state_label(&target),
        "targetExe": c.render_exe.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
    })
}

/// 真实游戏库：scanned + manual 合并，去掉用户移除过的，算好部署目标与状态。
fn list_games() -> Result<Value, String> {
    let mut cache = framegen_core::scan::load_library();
    /* 判定结果会跟着库缓存走，而检测只在「添加 / 扫描」时算一次。
       于是有个坑：**检测逻辑本身升级了，老缓存还是旧结论** —— 用户装上新版打开程序，
       看到的仍是一年前的判定（实测：「霍格沃茨之遗」自带 DLSS 帧生成，却被旧逻辑判成
       「未检出」，装了修好的版本也还是错的）。

       这里做一次**廉价自愈**：只对「缓存说不自带 DLSS」的行重算。真没有的游戏，重算
       也就是再看一遍导入表 + 几个固定目录（每行几毫秒）；被旧逻辑漏掉的，打开程序就对了。
       缓存里已经是 true 的行不重算，所以正常启动不会多花时间。 */
    let mut healed = false;
    for row in cache.scanned.iter_mut().chain(cache.manual.iter_mut()) {
        if row.streamline {
            continue;
        }
        let Some(exe) = row.render_exe.clone() else { continue };
        if !exe.is_file() {
            continue;
        }
        let rep = framegen_core::scan::detect_tech_report(&exe);
        if rep.streamline {
            row.streamline = true;
            row.api = rep.api;
            row.engine = rep.engine;
            row.tech_scanned = true;
            healed = true;
            framegen_core::log::line(&format!(
                "重新检测到自带 DLSS 帧生成：{}（{}）",
                row.entry.name,
                exe.display()
            ));
        }
    }
    if healed {
        if let Err(e) = framegen_core::scan::save_library(&cache) {
            framegen_core::log::line(&format!("自愈后写回游戏库失败：{e}"));
        }
    }
    let ignored = cache.ignored.clone();
    let is_ignored = |d: &Path| {
        let k = framegen_core::scan::path_key(d);
        ignored.iter().any(|s| s == &k)
    };
    let mut games: Vec<Value> = Vec::new();
    for c in &cache.scanned {
        // 安装目录已经不在的（游戏卸载了）不列出来
        if !c.entry.install_dir.is_dir() || is_ignored(&c.entry.install_dir) {
            continue;
        }
        games.push(game_row(c, false));
    }
    for c in &cache.manual {
        if is_ignored(&c.entry.install_dir) {
            continue;
        }
        games.push(game_row(c, true));
    }
    // 「已被移除的游戏」：带上名字 / 目录 / 状态，界面才能像游戏库那样画**卡片**
    // （用户要求：不要列表）。缓存里找不到对应的行（比如游戏已经卸载、行被清掉）
    // 就退回用 key 当名字 —— 至少能显示出是哪一条，也能放回。
    let mut removed: Vec<Value> = Vec::new();
    for key in &cache.ignored {
        let hit = cache
            .scanned
            .iter()
            .map(|c| (c, false))
            .chain(cache.manual.iter().map(|c| (c, true)))
            .find(|(c, _)| &framegen_core::scan::path_key(&c.entry.install_dir) == key);
        match hit {
            Some((c, manual)) => {
                let mut v = game_row(c, manual);
                if let Some(o) = v.as_object_mut() {
                    o.insert("key".to_owned(), json!(key));
                    o.insert("removedFrom".to_owned(), json!("library"));
                }
                removed.push(v);
            }
            None => removed.push(json!({
                "key": key,
                "removedFrom": "library",
                "name": key,
                "dir": key,
                "source": "未知",
                "exists": Path::new(key.as_str()).is_dir(),
                "deployed": "未部署",
                "dlss5": "未部署",
                "targetExe": "",
                "manual": false,
            })),
        }
    }
    Ok(json!({
        "games": games,
        "removed": removed,
        "scannedAt": cache.scanned_at,
        "libraryPath": framegen_core::scan::library_path().display().to_string(),
    }))
}

/// 扫出来的条目 -> 缓存行：找渲染 EXE、判图形 API / 引擎 / 自带帧生成、判反作弊。
///
/// 与 egui 版 App::build_row 同一套调用顺序，不能省步骤 ——
/// 少一步就会写出「api/engine 是 Unknown 却标记成已算过」的坏缓存。
fn build_cached(entry: framegen_core::scan::GameEntry) -> framegen_core::scan::CachedRow {
    let render_exe = framegen_core::scan::find_render_exe(&entry.install_dir);
    let (api, engine, streamline, tech_scanned) = match render_exe.as_deref() {
        Some(p) => {
            let rep = framegen_core::scan::detect_tech_report(p);
            framegen_core::log::line(&format!(
                "图形/引擎 [{}] API={} 引擎={} 自带帧生成={} | {}",
                entry.name,
                rep.api.label(),
                rep.engine.label(),
                rep.streamline,
                rep.evidence.join("；")
            ));
            (rep.api, rep.engine, rep.streamline, true)
        }
        None => (
            framegen_core::scan::GraphicsApi::Unknown,
            framegen_core::scan::GameEngine::Unknown,
            false,
            // 连渲染 EXE 都没找到：不是「算过」，等以后找到再算
            false,
        ),
    };
    // 除了游戏根目录，还要看渲染 EXE 所在目录：BattlEye 经常埋在 Binaries\Win64\BattlEye
    let mut rep = framegen_core::anticheat::scan_game_dir(&entry.install_dir);
    if let Some(dir) = render_exe.as_ref().and_then(|p| p.parent()) {
        rep.merge(framegen_core::anticheat::scan_game_dir(dir));
    }
    framegen_core::scan::CachedRow {
        entry,
        render_exe,
        ac: rep.verdict(),
        api,
        engine,
        streamline,
        tech_scanned,
    }
}

/// 扫描游戏库（Steam / Epic / WeGame / 本地目录）并落盘。
///
/// 落盘这一步不能省：不写缓存的话下次启动还是空列表，
/// 用户看到的就是「扫描了但没生效」。手动条目与忽略清单原样保留。
fn scan_library(id: u64) -> Result<Value, String> {
    progress(id, "正在扫描游戏库…", 0.0);
    let (entries, notes) = framegen_core::scan::scan_all_notes();
    let old = framegen_core::scan::load_library();
    let rows: Vec<framegen_core::scan::CachedRow> = entries.into_iter().map(build_cached).collect();
    let count = rows.len();
    let cache = framegen_core::scan::LibraryCache {
        scanned_at: framegen_core::util::now_utc(),
        scanned: rows,
        manual: old.manual,
        ignored: old.ignored,
    };
    let saved = framegen_core::scan::save_library(&cache).is_ok();
    progress(id, &format!("扫描完成，共 {count} 个游戏"), 1.0);
    Ok(json!({ "count": count, "notes": notes, "saved": saved }))
}

/// 打开系统设置里「硬件加速 GPU 计划」那一页（只跳转，绝不写注册表）。
///
/// URI 由 core 决定（gpu::hags_settings_uri），和 egui 版的「去设置」是同一个去处。
fn open_hags_settings() -> Result<Value, String> {
    framegen_core::gpu::open_hags_settings().map_err(|e| e.to_string())?;
    Ok(json!({ "uri": framegen_core::gpu::hags_settings_uri() }))
}

/// 手动把一个目录加进游戏库（扫不出来的游戏、或者用户自己的目录）。
///
/// 与 egui 版 add_to_library 同一套顺序：manual_entry 建条目 → 找渲染 EXE →
/// detect_tech 判图形 API / 引擎 / 自带帧生成 → 反作弊扫一遍 → 存进 cache.manual。
/// **存 manual 而不是 scanned**：重新扫描不会把用户手动加的条目冲掉。
fn add_game() -> Result<Value, String> {
    let picked = rfd::FileDialog::new()
        .set_title("选择游戏目录（渲染 EXE 所在的那一层，或它的上层）")
        .pick_folder();
    let Some(dir) = picked else {
        return Ok(json!({ "canceled": true }));
    };
    add_library_dir(&dir, None)
}

/// 「手动选择游戏根目录 exe 启动文件」（DLSS5 页与部署对话框的按钮）。
///
/// 与「添加目录」的差别只有入口：这里选的是**启动 exe**，目录取它所在的那一层 ——
/// DLSS5 的四个文件全都铺在 exe 旁边，选错了目录等于白装。入库逻辑是同一份
/// （add_library_dir），所以两种加法的结果不会有第二套行为。
fn pick_game_exe() -> Result<Value, String> {
    let picked = rfd::FileDialog::new()
        .set_title("选择游戏的启动 exe（渲染 EXE 本体）")
        .add_filter("可执行文件", &["exe"])
        .pick_file();
    let Some(exe) = picked else {
        return Ok(json!({ "canceled": true }));
    };
    let Some(dir) = exe.parent().map(|p| p.to_path_buf()) else {
        return Err(format!("这个文件没有所在目录：{}", exe.display()));
    };
    let mut v = add_library_dir(&dir, Some(exe))?;
    if let Some(o) = v.as_object_mut() {
        o.insert("fromExe".to_owned(), json!(true));
    }
    Ok(v)
}

/// 把一个目录（以及可选的启动 exe）加进游戏库。
///
/// 目录已经在库里时**不报「已经有了」了事**：用户点「手动选择」本来就是要换/指定
/// 启动文件的，所以那一行会被就地更新（render_exe 与技术检测一起重算）。
fn add_library_dir(dir: &Path, exe: Option<PathBuf>) -> Result<Value, String> {
    let mut cache = framegen_core::scan::load_library();
    // 用户又把它加回来了：从忽略清单里去掉，免得下次扫描/显示又被藏起来
    let key = framegen_core::scan::path_key(dir);
    cache.ignored.retain(|s| s != &key);

    let render_exe = match exe {
        Some(e) => Some(e),
        None => framegen_core::scan::find_render_exe(dir),
    };
    let (api, engine, streamline) = render_exe
        .as_deref()
        .map(framegen_core::scan::detect_tech)
        .unwrap_or((
            framegen_core::scan::GraphicsApi::Unknown,
            framegen_core::scan::GameEngine::Unknown,
            false,
        ));
    let mut rep = framegen_core::anticheat::scan_game_dir(dir);
    if let Some(d) = render_exe.as_ref().and_then(|p| p.parent()) {
        rep.merge(framegen_core::anticheat::scan_game_dir(d));
    }

    // 已在库里（扫出来的或手动加的）：就地更新那一行
    let existing = cache
        .scanned
        .iter_mut()
        .chain(cache.manual.iter_mut())
        .find(|c| c.entry.install_dir == dir);
    let exe_out = render_exe
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    if let Some(row) = existing {
        let name = row.entry.name.clone();
        row.render_exe = render_exe;
        row.api = api;
        row.engine = engine;
        row.streamline = streamline;
        row.tech_scanned = true;
        row.ac = rep.verdict();
        framegen_core::scan::save_library(&cache).map_err(|e| e.to_string())?;
        framegen_core::log::line(&format!("更新游戏库条目：{}", dir.display()));
        return Ok(json!({
            "canceled": false,
            "added": false,
            "updated": true,
            "name": name,
            "dir": dir.display().to_string(),
            "exe": exe_out,
        }));
    }

    let entry = framegen_core::scan::manual_entry(dir);
    let name = entry.name.clone();
    cache.manual.push(framegen_core::scan::CachedRow {
        entry,
        render_exe,
        ac: rep.verdict(),
        api,
        engine,
        streamline,
        tech_scanned: true,
    });
    framegen_core::scan::save_library(&cache).map_err(|e| e.to_string())?;
    framegen_core::log::line(&format!("存进游戏库（手动）：{}", dir.display()));
    Ok(json!({
        "canceled": false,
        "added": true,
        "updated": false,
        "name": name,
        "dir": dir.display().to_string(),
        "exe": exe_out,
    }))
}

// ---------------------------------------------------------------- 部署目标候选 / 封面

/// 「部署目标」下拉里的候选 exe（按可信度排序，排第一的就是自动认出来的那个）。
///
/// 抽成命令而不是让界面自己扫目录：打分逻辑在 core（scan::candidate_exes），
/// 与 find_render_exe 共用同一份 —— 界面只负责把名字列出来。
fn list_exes(dir: String) -> Result<Value, String> {
    let p = PathBuf::from(&dir);
    if !p.is_dir() {
        return Err(format!("目录不存在：{dir}"));
    }
    let best = framegen_core::scan::find_render_exe(&p);
    let list: Vec<Value> = framegen_core::scan::candidate_exes(&p)
        .iter()
        .map(|e| {
            json!({
                "path": e.display().to_string(),
                "name": e.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                "size": std::fs::metadata(e).map(|m| m.len()).unwrap_or(0),
                "isBest": best.as_deref() == Some(e.as_path()),
            })
        })
        .collect();
    Ok(json!({
        "dir": dir,
        "best": best.map(|b| b.display().to_string()),
        "exes": list,
    }))
}

/// 封面：给这些 Steam appid 补齐竖版封面（下载与缓存都在 core 的
/// update::fetch_steam_covers 里，这里只转发参数与进度）。
fn fetch_covers(id: u64, args: &Value) -> Result<Value, String> {
    let ids: Vec<String> = args
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_owned()))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return Ok(json!({ "fetched": 0, "skipped": 0, "failed": 0 }));
    }
    let client = framegen_core::update::client().map_err(|e| e.to_string())?;
    let (fetched, skipped, failed) =
        framegen_core::update::fetch_steam_covers(&client, &ids, |msg, f| progress(id, &msg, f))
            .map_err(|e| e.to_string())?;
    Ok(json!({ "fetched": fetched, "skipped": skipped, "failed": failed }))
}

// ---------------------------------------------------------------- DLSS5

/// DLSS5 三件套在不在（DLSS5 页与部署对话框的「就绪」判断）。
fn dlss5_assets() -> Value {
    serde_json::to_value(framegen_core::dlss5::assets_report()).unwrap_or(Value::Null)
}

/// 部署对话框里的两个下拉：图形 API 与渲染后端。
/// **列表来自 core 的常量**，界面不再抄第二份 —— 以后加了后端，只有这里要改。
fn dlss5_choices() -> Value {
    let apis: Vec<Value> = framegen_core::dlss5::API_CHOICES
        .iter()
        .map(|(_, label)| {
            let key = match *label {
                "DX9" => "dx9",
                "DX10" => "dx10",
                "DX11" => "dx11",
                "DX12" => "dx12",
                "Vulkan" => "vulkan",
                _ => "opengl",
            };
            json!({ "key": key, "label": label })
        })
        .collect();
    json!({
        "apis": apis,
        "autoKey": framegen_core::dlss5::API_AUTO,
        "backends": [ { "key": framegen_core::dlss5::BACKEND_RESHADE, "label": "ReShade（Addon）" } ],
        "backendKey": framegen_core::dlss5::BACKEND_RESHADE,
    })
}

/// DLSS5 的部署状态（对话框里那一行，比 list_games 里那份更详细）。
fn dlss5_state(dir: String) -> Result<Value, String> {
    let p = PathBuf::from(&dir);
    let label = framegen_core::dlss5::state_label(&p);
    match framegen_core::dlss5::state_of(&p) {
        framegen_core::dlss5::State::Deployed(m) => Ok(json!({
            "label": label,
            "deployed": true,
            "api": m.api,
            "backend": m.backend,
            "module": m.module,
            "exe": m.exe,
            "gpu": m.gpu,
            "deployedAt": m.deployed_at,
            "files": m.files.iter().map(|e| json!({
                "name": e.rel_path,
                "role": e.role,
                "existedBefore": e.existed_before,
            })).collect::<Vec<_>>(),
        })),
        framegen_core::dlss5::State::NotDeployed => Ok(json!({ "label": label, "deployed": false })),
        framegen_core::dlss5::State::Broken(e) => {
            Ok(json!({ "label": label, "deployed": false, "broken": e }))
        }
    }
}

/// 部署 DLSS5（ReShade + 插件 + 模型）。界面上的「部署」按钮走这里。
fn dlss5_deploy(id: u64, args: &Value) -> Result<Value, String> {
    let dir = arg_str(args, "dir")?;
    let exe = arg_opt_str(args, "exe")?;
    let backend = arg_opt_str(args, "backend")?.unwrap_or_else(|| framegen_core::dlss5::BACKEND_RESHADE.to_owned());
    let api_key = arg_opt_str(args, "api")?.unwrap_or_else(|| framegen_core::dlss5::API_AUTO.to_owned());
    let api = framegen_core::dlss5::api_from_key(&api_key)
        .ok_or_else(|| format!("不认识的图形 API：{api_key}"))?;
    let dir_p = PathBuf::from(&dir);
    let exe_p = exe.as_ref().map(PathBuf::from);
    framegen_core::log::line(&format!(
        "DLSS5 部署：{dir} ｜ 后端 {backend} ｜ API {api_key} ｜ exe {}",
        exe.as_deref().unwrap_or("（自动识别）")
    ));
    let (msg, notes) = framegen_core::dlss5::deploy(
        &dir_p,
        exe_p.as_deref(),
        api,
        &backend,
        &mut |m: String, f: f32| progress(id, &m, f),
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({ "msg": msg, "notes": notes }))
}

/// 探测这个目录里有没有 ReShade / DLSS5。
/// **不看我们的部署记录也认**：用户自己手动装过的同样能查出来（用户要求）。
fn dlss5_detect(dir: String) -> Result<Value, String> {
    let p = PathBuf::from(&dir);
    if !p.is_dir() {
        return Err(format!("目录不存在：{dir}"));
    }
    let f = framegen_core::dlss5::detect(&p);
    let mut v = serde_json::to_value(&f).map_err(|e| e.to_string())?;
    if let Some(o) = v.as_object_mut() {
        o.insert("label".to_owned(), json!(f.label()));
        o.insert("anything".to_owned(), json!(f.anything()));
    }
    Ok(v)
}

/// 彻底卸载 ReShade + DLSS5（含效果包目录、插件、模型、插件运行时残渣）。
/// **永久删除、不进回收站** —— 界面必须先拿到用户二次确认才允许调这里。
fn dlss5_uninstall(id: u64, args: &Value) -> Result<Value, String> {
    let dir = arg_str(args, "dir")?;
    let exe = arg_opt_str(args, "exe")?;
    let dir_p = PathBuf::from(&dir);
    let exe_p = exe.as_ref().map(PathBuf::from);
    framegen_core::log::line(&format!(
        "DLSS5 彻底卸载：{dir}（exe {}）",
        exe.as_deref().unwrap_or("（自动找）")
    ));
    let msg = framegen_core::dlss5::uninstall(&dir_p, exe_p.as_deref(), &mut |m: String, f: f32| {
        progress(id, &m, f)
    })
    .map_err(|e| e.to_string())?;
    Ok(json!({ "msg": msg }))
}

/// 还原 DLSS5（只动本工具部署的那些文件；ReShade 本体按备份恢复/删除）。
fn dlss5_restore(dir: String) -> Result<Value, String> {
    let msg = framegen_core::dlss5::restore(Path::new(&dir)).map_err(|e| e.to_string())?;
    Ok(json!({ "msg": msg }))
}

/// 每个游戏的 **exe 图标**（从渲染 EXE 里抠出来的那张）。
///
/// 为什么要单独一个命令、而不是塞进 list_games：图标是 RGBA 位图，一个 32×32 就是
/// 4 KB（base64 之后 5 KB 多），塞进列表会把每次刷新的包撑大一个数量级。这里只在
/// 列表出来之后拉一次，前端按目录缓存。
fn game_icons() -> Result<Value, String> {
    let cache = framegen_core::scan::load_library();
    let mut out: Vec<Value> = Vec::new();
    for row in cache.scanned.iter().chain(cache.manual.iter()) {
        let Some(exe) = row.render_exe.as_ref() else { continue };
        let Some(img) = framegen_core::icon::icon_of(exe) else { continue };
        out.push(json!({
            "dir": row.entry.install_dir.display().to_string(),
            "w": img.width,
            "h": img.height,
            // RGBA8 原样传，前端画进 canvas 再取 data URL —— 这样 core 不需要 PNG 编码器
            "rgba": b64(&img.rgba),
        }));
    }
    Ok(json!({ "icons": out }))
}

/// 从游戏库移除（只动列表，游戏目录里的文件一律不碰），并记进忽略清单。
///
/// **不删缓存里的那一行**。为什么：删了之后「放回」只把 key 从忽略清单里去掉，
/// 行却已经没了 —— 扫描出来的游戏要等下次扫描才回来，**手动添加的游戏则永远回不来**
/// （实测：移除 VTube Studio 后点「放回游戏库」，库里的卡片不回来）。
/// 列表隐藏本来就有 ignored 这一层负责，所以保留行是安全的、也是必须的。
fn remove_game(dir: String) -> Result<Value, String> {
    let p = PathBuf::from(&dir);
    let key = framegen_core::scan::path_key(&p);
    let mut cache = framegen_core::scan::load_library();
    if !cache.ignored.iter().any(|s| s == &key) {
        cache.ignored.push(key);
    }
    framegen_core::scan::save_library(&cache).map_err(|e| e.to_string())?;
    framegen_core::log::line(&format!("从游戏库移除：{}", p.display()));
    Ok(json!({ "ok": true }))
}

/// 把移除过的条目放回来（重新扫描 / 下次启动就会出现）
fn restore_removed(key: String) -> Result<Value, String> {
    let mut cache = framegen_core::scan::load_library();
    cache.ignored.retain(|s| s != &key);
    framegen_core::scan::save_library(&cache).map_err(|e| e.to_string())?;
    framegen_core::log::line(&format!("恢复被移除的条目：{key}"));
    Ok(json!({ "ok": true }))
}

// ---------------------------------------------------------------- 部署 / 还原

/// 部署状态（读现有 manifest）
fn deploy_state(dir: String) -> String {
    framegen_core::deploy::state_of(Path::new(&dir))
        .label()
        .to_string()
}

/// 入口推荐 —— 界面上的「重新检测入口」就是再调一次它。
///
/// 用的是 core 的 advise_proxy：**按导入表判，不是按游戏名猜**，也不是按目录里
/// 有哪些同名文件猜。判据三级：先排除被别的 mod 占用的入口 → 再按导入表匹配 →
/// 都判不出来就用上游默认并标 undetermined。
fn proxy_advice(dir: String) -> Result<Value, String> {
    let a = framegen_core::scan::advise_proxy(Path::new(&dir));
    Ok(json!({
        "recommended": a.recommended,
        "reason": a.reason,
        "undetermined": a.undetermined,
        "scanned": a.scanned,
        "occupied": a.occupied.iter().map(|e| json!({
            "name": e.name,
            "bytes": e.bytes,
            "identity": e.identity.label(),
        })).collect::<Vec<_>>(),
        "ownExisting": a.own_existing.iter().map(|e| json!({
            "name": e.name,
            "bytes": e.bytes,
        })).collect::<Vec<_>>(),
    }))
}

/// 部署的实际动作（两道闸门 + core 调用）。
///
/// 抽出来是为了让**命令行自测**（--deploytest）和界面走的是同一段代码 ——
/// 否则「自测过了」和「界面上能用」是两件事。sidecar 里这条同样成立。
fn deploy_impl(dir: &str, proxy: &str, optimized: u8, frames: u8) -> Result<(String, Vec<String>), String> {
    let gpu = framegen_core::scan::detect_gpu().unwrap_or_default();
    let route = framegen_core::scan::classify_gpu(&gpu);
    if let Some(why) = route.gate() {
        framegen_core::log::line(&format!(
            "已阻止部署：{why}（路由 {}，显卡 {gpu}）",
            route.label()
        ));
        return Err(format!("已阻止部署：{why}"));
    }
    // 档位由界面在**这次部署时**选（每个游戏一个），所以这里只做校验
    if optimized > 3 {
        return Err(format!("优化等级只能是 0~3（收到 {optimized}）"));
    }
    if frames != framegen_core::update::DEFAULT_MAX_FRAMES && frames != 5 {
        return Err(format!("倍率上限只能是 4X 或 6X（收到 {frames}）"));
    }
    framegen_core::log::line(&format!(
        "部署档位：优化等级 {optimized}、倍率上限 {frames} ｜ {dir} ｜ 入口 {proxy}"
    ));
    framegen_core::deploy::deploy_game(Path::new(dir), proxy, optimized, frames).map_err(|e| e.to_string())
}

fn deploy_game(dir: String, proxy: String, optimized: u8, frames: u8) -> Result<Value, String> {
    let (msg, notes) = deploy_impl(&dir, &proxy, optimized, frames)?;
    Ok(json!({ "msg": msg, "notes": notes }))
}

/// 一键还原。
///
/// 两条路（用户要求都要能走通）：
///   * 有本工具的部署记录 -> deploy::restore：按 manifest 把游戏原件放回去、删掉我们写的文件；
///   * 没有记录、但目录里有**本项目的**帧生成文件（用户自己手动装的）-> cleanup_manual：
///     没有备份可恢复，所以只把认得出的那些文件删掉，删完复查并如实报告。
fn restore(dir: String) -> Result<String, String> {
    let p = Path::new(&dir);
    match framegen_core::deploy::restore(p) {
        Ok(m) => Ok(m),
        Err(e) => {
            // 只有确实找到「本项目的文件」时才走清理那条路；否则把原来的错误原样报出来
            if framegen_core::deploy::manual_files(p).is_empty() {
                return Err(e.to_string());
            }
            framegen_core::log::line(&format!(
                "没有部署记录，按「手动安装清理」处理：{}",
                p.display()
            ));
            framegen_core::deploy::cleanup_manual(p).map_err(|x| x.to_string())
        }
    }
}

// ---------------------------------------------------------------- 下载 / 更新（已移除）

// 这里原来有三条命令：check_update / download_assets / import_archive（配合旧界面的
// 「检查更新 / 下载资产 / 导入压缩包」三个按钮）。1.0.0 起帧生成资产已随安装包内置、
// 界面也不再暴露这三个入口，2026-10 按用户要求删除。core 里的 update:: 下载/校验实现
// 仍然保留（自更新与版本检查共用其中一部分）。

// ---------------------------------------------------------------- 用户备注（踩坑记录）

// （这里原来有 get_tips / set_tips：「我的备注」那一页。用户决定不要「使用说明」这一页，
//  2026-10 连同界面代码一起删除。老用户数据目录里的 tips.md 仍会被主进程的搬数据逻辑带着走。）

// ---------------------------------------------------------------- 设置 / 自更新

/// 「优化等级」下拉框里显示的一行字（与 egui 版同一套说法）
fn optimized_label(v: u8) -> &'static str {
    match v {
        0 => "0 · 原厂不加速",
        2 => "2 · 更快（轻微有损）",
        3 => "3 · 最快（有损）",
        _ => "1 · 与官方逐位一致",
    }
}

fn frames_label(v: u8) -> &'static str {
    if v >= 5 {
        "6X"
    } else {
        "4X"
    }
}

/// 设置 + 只读路径：设置页要显示的东西一次拿全，省得前端发四五个 invoke
fn get_config() -> Value {
    let cfg = framegen_core::util::load_config();
    let is_default = cfg.fg_optimized == framegen_core::update::DEFAULT_OPTIMIZED
        && cfg.fg_frames == framegen_core::update::DEFAULT_MAX_FRAMES;
    json!({
        "optimized": cfg.fg_optimized,
        "frames": cfg.fg_frames,
        "optimizedLabel": optimized_label(cfg.fg_optimized),
        "framesLabel": frames_label(cfg.fg_frames),
        "defaultOptimized": framegen_core::update::DEFAULT_OPTIMIZED,
        "defaultFrames": framegen_core::update::DEFAULT_MAX_FRAMES,
        "isDefault": is_default,
        "version": framegen_core::update::SELF_VERSION,
        "releasesUrl": framegen_core::update::RELEASES_URL,
        // 代理入口名单从 core 拿：界面抄一份迟早会跟 PROXY_ALL 对不上
        "proxies": framegen_core::scan::PROXY_PRIORITY,
        "configPath": framegen_core::util::config_path().display().to_string(),
        "assetsDir": framegen_core::util::assets_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        "backupsDir": framegen_core::util::backups_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        "libraryPath": framegen_core::scan::library_path().display().to_string(),
        "logsDir": framegen_core::log::dir().map(|p| p.display().to_string()).unwrap_or_default(),
    })
}

/// 改部署档位：只接受上游真正支持的取值 —— 别让前端传进来的怪数字写进配置
/// （0~3 是已知四档；倍率上限只有 3=4X 和 5=6X 两种）
fn set_config(optimized: u8, frames: u8) -> Result<Value, String> {
    if optimized > 3 {
        return Err(format!("优化等级只能是 0~3（收到 {optimized}）"));
    }
    if frames != framegen_core::update::DEFAULT_MAX_FRAMES && frames != 5 {
        return Err(format!(
            "倍率上限只能是 4X（{}）或 6X（5）（收到 {frames}）",
            framegen_core::update::DEFAULT_MAX_FRAMES
        ));
    }
    let mut cfg = framegen_core::util::load_config();
    cfg.fg_optimized = optimized;
    cfg.fg_frames = frames;
    framegen_core::util::save_config(&cfg).map_err(|e| e.to_string())?;
    framegen_core::log::line(&format!(
        "部署档位改为：优化等级 {optimized}（{}）、倍率上限 {}",
        optimized_label(optimized),
        frames_label(frames)
    ));
    Ok(get_config())
}

/// 检查我们自己的新版本：**只查，不下载安装**。
/// 真正换 exe 的那一步（stage_self_update + swap_in_place）还没接 —— 宁可不做，
/// 也不做一个会把自己换坏的半成品。
fn check_self_update() -> Result<Value, String> {
    let client = framegen_core::update::client().map_err(|e| e.to_string())?;
    let current = framegen_core::update::SELF_VERSION.to_string();
    let latest = framegen_core::update::fetch_latest_self_version(&client);
    let has_update = latest
        .as_deref()
        .map(|l| framegen_core::update::is_newer(l, &current))
        .unwrap_or(false);
    Ok(json!({
        "current": current,
        "latest": latest,
        "hasUpdate": has_update,
        "releasesUrl": framegen_core::update::RELEASES_URL,
    }))
}

/// 下载新版安装包并校验 SHA256（core 的 stage_self_update），**但不安装**。
///
/// 为什么拆成两步：安装必须由外壳（Electron 主进程）来做 —— 它要弹原生确认框、
/// 启动安装程序、然后退出自己。这里只负责「下载 + 校验」，校验不过绝不返回路径
/// （哈希取自发布时的 SHA256SUMS.txt，且清单只走官方源，不走镜像）。
fn self_update_download(id: u64, args: &Value) -> Result<Value, String> {
    let version = arg_str(args, "version")?;
    let client = framegen_core::update::client().map_err(|e| e.to_string())?;
    // 用**共享**的取消标志（cancel_self_update 命令置位的就是它），每次开始前复位。
    let cancel = selfupdate_cancel();
    cancel.store(false, std::sync::atomic::Ordering::SeqCst);
    let staged = match framegen_core::update::stage_self_update(
        &client,
        &version,
        &cancel,
        &mut |done, total, msg| {
            let f = if total > 0 { done as f32 / total as f32 } else { 0.0 };
            progress(id, msg, f);
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                // 用户点了「停止更新」：把半截文件删干净（用户明确要求不能留残余）
                let _ = std::fs::remove_dir_all(std::env::temp_dir().join("fgm-selfupdate"));
                return Ok(json!({ "canceled": true }));
            }
            return Err(e.to_string());
        }
    };
    Ok(json!({
        "installer": staged.installer.display().to_string(),
        "sha256": staged.sha256,
        "bytes": staged.bytes,
        "version": version,
    }))
}

// ---------------------------------------------------------------- 请求参数

/// 取字符串参数。
///
/// Tauri 版靠 serde 反序列化，缺参数时抛的是「invalid args ...」那种英文机器话；
/// Electron 会把 error 原样显示到 toast 上，所以这里换成中文，并且说清是哪个参数。
fn arg_str(args: &Value, key: &str) -> Result<String, String> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(format!("参数 {key} 必须是字符串")),
        None => Err(format!("缺少参数 {key}")),
    }
}

/// 可选字符串参数（import_archive 的 path：没给 / 给 null 都表示「弹文件框」）
fn arg_opt_str(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("参数 {key} 必须是字符串或 null")),
    }
}

/// 取整数参数。数字和纯数字字符串都收：界面上 select 的值天生是字符串，
/// 哪天某处忘了 Number() 也不该变成「参数类型不对」这种莫名其妙的报错。
fn arg_int(args: &Value, key: &str) -> Result<i64, String> {
    match args.get(key) {
        Some(Value::Number(n)) => n
            .as_i64()
            .ok_or_else(|| format!("参数 {key} 必须是整数（收到 {n}）")),
        Some(Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("参数 {key} 必须是整数（收到 {s}）")),
        Some(_) => Err(format!("参数 {key} 必须是整数")),
        None => Err(format!("缺少参数 {key}")),
    }
}

/// u8 参数（optimized / frames）。范围检查交给命令本身做 ——
/// 这里只保证「能装进 u8」。
fn arg_u8(args: &Value, key: &str) -> Result<u8, String> {
    let n = arg_int(args, key)?;
    u8::try_from(n).map_err(|_| format!("参数 {key} 超出范围（{n}）"))
}

fn arg_usize(args: &Value, key: &str) -> Result<usize, String> {
    let n = arg_int(args, key)?;
    usize::try_from(n).map_err(|_| format!("参数 {key} 超出范围（{n}）"))
}

// ---------------------------------------------------------------- 命令分发

/// 命令名 -> 具体实现。表里的每个名字都和 apps/desktop/src/main.rs 里
/// tauri::generate_handler! 的那一份对应，一个不多一个不少
/// （外加一个 ping：Electron 用它做健康检查，Tauri 版不需要探活）。
fn dispatch(id: u64, cmd: &str, args: &Value) -> Result<Value, String> {
    match cmd {
        // 健康检查：进程起来了、stdin/stdout 通了
        "ping" => Ok(Value::String("pong".into())),

        "gpu" => Ok(Value::String(gpu())),
        // force=true 时穿透缓存（界面上的手动刷新用得到）
        "gpu_info" => gpu_info(
            args.get("force").and_then(|v| v.as_bool()).unwrap_or(false),
            arg_opt_str(args, "fakeName")?,
        ),
        "covers" => Ok(Value::Array(covers())),
        "assets_status" => Ok(assets_status()),
        "cancel_self_update" => {
            // 置位取消标志；下载循环下一块就会退出，然后由 self_update_download 清理半截文件。
            selfupdate_cancel().store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({ "canceling": true }))
        }
        "set_efficiency" => {
            // 参数：{"pids":[123,456]}。返回实际设置成功的个数，便于验证。
            let pids: Vec<u32> = args
                .get("pids")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_u64().map(|n| n as u32)).collect())
                .unwrap_or_default();
            let (ok, total) = set_efficiency(&pids);
            Ok(json!({ "ok": ok, "total": total }))
        }
        "backups_dir" => Ok(Value::String(backups_dir())),
        "log_tail" => Ok(Value::String(log_tail(arg_usize(args, "lines")?))),
        "open_path" => {
            open_path(arg_str(args, "path")?)?;
            Ok(Value::Null)
        }
        "open_url" => {
            open_url(arg_str(args, "url")?)?;
            Ok(Value::Null)
        }
        "pick_assets_dir" => pick_assets_dir(),

        "list_games" => list_games(),
        "scan_library" => scan_library(id),
        "add_game" => add_game(),
        "pick_game_exe" => pick_game_exe(),
        "game_icons" => game_icons(),
        "open_hags_settings" => open_hags_settings(),
        "remove_game" => remove_game(arg_str(args, "dir")?),
        "restore_removed" => restore_removed(arg_str(args, "key")?),

        "deploy_state" => Ok(Value::String(deploy_state(arg_str(args, "dir")?))),
        "proxy_advice" => proxy_advice(arg_str(args, "dir")?),
        "deploy_game" => deploy_game(
            arg_str(args, "dir")?,
            arg_str(args, "proxy")?,
            arg_u8(args, "optimized")?,
            arg_u8(args, "frames")?,
        ),
        "restore" => Ok(Value::String(restore(arg_str(args, "dir")?)?)),

        "gpu_spoof_state" => gpu_spoof_state(),
        "gpu_spoof_apply" => {
            let name = arg_str(args, "name")?;
            run_gpu_op(&framegen_core::gpu::Op::Apply(name)).map(|m| json!({ "msg": m }))
        }
        "gpu_spoof_restore" => {
            let to = arg_opt_str(args, "to")?.unwrap_or_else(|| "driver".to_owned());
            let to = if to == "backup" {
                framegen_core::gpu::RestoreTo::BackupOriginal
            } else {
                framegen_core::gpu::RestoreTo::DriverName
            };
            run_gpu_op(&framegen_core::gpu::Op::Restore(to)).map(|m| json!({ "msg": m }))
        }

        "list_exes" => list_exes(arg_str(args, "dir")?),
        "fetch_covers" => fetch_covers(id, args),

        "dlss5_assets" => Ok(dlss5_assets()),
        "dlss5_choices" => Ok(dlss5_choices()),
        "dlss5_state" => dlss5_state(arg_str(args, "dir")?),
        "dlss5_deploy" => dlss5_deploy(id, args),
        "dlss5_restore" => dlss5_restore(arg_str(args, "dir")?),
        "dlss5_detect" => dlss5_detect(arg_str(args, "dir")?),
        "dlss5_uninstall" => dlss5_uninstall(id, args),


        "get_config" => Ok(get_config()),
        "set_config" => set_config(arg_u8(args, "optimized")?, arg_u8(args, "frames")?),
        "check_self_update" => check_self_update(),
        "self_update_download" => self_update_download(id, args),


        other => Err(format!("unknown command: {other}")),
    }
}

// ---------------------------------------------------------------- 主循环

/// 处理一行请求。**任何情况都不许把进程带走**：
/// 不是合法 JSON、缺 cmd、参数不对、命令内部 panic —— 全都变成一行 error 回过去，
/// 然后接着读下一行。用户界面上点错东西顶多弹个提示，不该整个后台进程消失。
fn handle_line(raw: &str) {
    // 行尾换行、以及从某些管道（PowerShell 的 echo）混进来的 BOM 都要剥掉，
    // 否则 serde 会把它当非法 JSON —— 表现出来就是「明明是对的请求却说解析失败」。
    let text = raw.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return;
    }
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            write_line(&err_line(0, &format!("请求不是合法 JSON：{e}")));
            return;
        }
    };
    // 缺 cmd 的请求连「回给谁」都说不准，按协议一律用 id 0 回一条
    let Some(cmd) = v.get("cmd").and_then(|c| c.as_str()) else {
        write_line(&err_line(0, "请求缺少 cmd 字段"));
        return;
    };
    let id = v.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
    // args 缺失 / 是 null / 不是对象都当空参数处理：所有命令都有自己的「缺参数」报错
    let args = match v.get("args") {
        Some(a) if a.is_object() => a.clone(),
        _ => json!({}),
    };

    // 单条命令 panic 不许带走整个进程：Electron 那边还挂着一堆 pending 调用，
    // 进程一死它们全部失败。catch_unwind 之后回一条错误行，继续服务。
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(id, cmd, &args)));
    match outcome {
        Ok(Ok(result)) => write_line(&ok_line(id, result)),
        Ok(Err(e)) => write_line(&err_line(id, &e)),
        Err(_) => write_line(&err_line(
            id,
            &format!("命令 {cmd} 内部异常（已捕获，进程继续）"),
        )),
    }
}

/// stdio 服务循环：读一行、办一件事、回一行。stdin 关闭（EOF）就正常退出。
fn serve() {
    use std::io::BufRead;
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        // 用 read_until 读**字节**而不是 read_line 读字符串：
        // 控制台按 GBK 之类的编码管道过来时，read_line 会因为不是合法 UTF-8 直接报错，
        // 那就成了「一条乱码把服务打死」。lossy 转一下，交给 serde 去判它不合法，
        // 结果是回一条错误行然后继续 —— 这正是协议要的。
        match reader.read_until(b'\n', &mut buf) {
            // EOF：Electron 关掉 stdin 就是在说「收工」
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                framegen_core::log::line(&format!("[sidecar] 读取 stdin 失败：{e}"));
                break;
            }
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        handle_line(&text);
    }
    framegen_core::log::line("[sidecar] stdin 已关闭，正常退出");
}

// ---------------------------------------------------------------- 命令行自测

/// 提权子进程入口：--gpuspoof-apply "<型号>" <结果文件> / --gpuspoof-restore driver|backup <结果文件>
///
/// 和 egui 版是同一套约定：父进程用 ShellExecuteW("runas") 拉起本 exe，子进程改完注册表
/// 把 {"ok":…,"msg":…} 写进结果文件就退出（不开窗口、不进 stdio 服务循环）。
/// 之所以让 **sidecar 自己**当那个子进程：Electron 版里它是唯一常驻的 exe，
/// 再拉一个主程序既没意义又可能再弹一个窗口。
fn gpuspoof_child(argv: &[String]) -> bool {
    let apply = argv.iter().position(|a| a == "--gpuspoof-apply");
    let restore = argv.iter().position(|a| a == "--gpuspoof-restore");
    let (op, out_path) = if let Some(i) = apply {
        let Some(name) = argv.get(i + 1) else { return false };
        let Some(out) = argv.get(i + 2) else { return false };
        (framegen_core::gpu::Op::Apply(name.clone()), out.clone())
    } else if let Some(i) = restore {
        let Some(mode) = argv.get(i + 1) else { return false };
        let Some(out) = argv.get(i + 2) else { return false };
        let to = if mode == "backup" {
            framegen_core::gpu::RestoreTo::BackupOriginal
        } else {
            framegen_core::gpu::RestoreTo::DriverName
        };
        (framegen_core::gpu::Op::Restore(to), out.clone())
    } else {
        return false;
    };

    let r = exec_gpu_op(&op);
    let payload = match &r {
        Ok(m) => json!({ "ok": true, "msg": m }),
        Err(e) => json!({ "ok": false, "msg": e }),
    };
    let body = payload.to_string();
    if std::fs::write(&out_path, body.as_bytes()).is_err() {
        // 结果文件写不出去，父进程会超时 —— 至少把原因写进日志
        framegen_core::log::line(&format!("[gpuspoof] 结果文件写入失败：{out_path}"));
    }
    match &r {
        Ok(m) => framegen_core::log::line(&format!("[gpuspoof] 成功：{m}")),
        Err(e) => framegen_core::log::line(&format!("[gpuspoof] 失败：{e}")),
    }
    true
}

/// 真正去改注册表（不提权）。找不到显卡实例、或没有权限都会返回中文原因。
fn exec_gpu_op(op: &framegen_core::gpu::Op) -> Result<String, String> {
    let adapters = framegen_core::gpu::enumerate();
    let a = framegen_core::gpu::primary(&adapters)
        .ok_or_else(|| "没有找到可操作的 NVIDIA 显卡注册表实例".to_owned())?;
    match op {
        framegen_core::gpu::Op::Apply(name) => framegen_core::gpu::apply(a, name),
        framegen_core::gpu::Op::Restore(to) => framegen_core::gpu::restore(a, *to),
    }
    .map_err(|e| e.to_string())
}

/// 跑一次显卡名操作：**先直接做**（用户可能就是管理员启动的，那就不弹 UAC），
/// 权限不够再用 ShellExecuteW("runas") 拉起自己重试一次。
fn run_gpu_op(op: &framegen_core::gpu::Op) -> Result<String, String> {
    // 伪装会改掉设备名：gpu_info 的缓存必须作废，否则卡片还显示旧名字
    gpu_cache_clear();
    match exec_gpu_op(op) {
        Ok(m) => Ok(m),
        Err(e) => {
            framegen_core::log::line(&format!("[gpuspoof] 直接写入失败（{e}），改用提权子进程"));
            let me = std::env::current_exe().map_err(|x| format!("取不到自己的路径：{x}"))?;
            framegen_core::gpu::run_elevated(&me, op)
                .map_err(|e2| format!("{e}；提权重试也没成功：{e2}"))
        }
    }
}

/// 显卡名伪装的状态（卡片显示 + 下拉选项）。
fn gpu_spoof_state() -> Result<Value, String> {
    let adapters = framegen_core::gpu::enumerate();
    let Some(a) = framegen_core::gpu::primary(&adapters) else {
        return Ok(json!({
            "available": false,
            "reason": "没有找到可操作的 NVIDIA 显卡注册表实例，这个功能在本机不可用。",
            "presets": framegen_core::gpu::PRESETS_UI,
            "warnings": framegen_core::gpu::SPOOF_WARNINGS,
        }));
    };
    Ok(json!({
        "available": true,
        "current": a.current_name(),
        "driver": a.driver_name,
        "spoofed": a.spoofed(),
        "enumKey": a.enum_key,
        "deviceDesc": a.device_desc,
        "presets": framegen_core::gpu::PRESETS_UI,
        "backupOriginal": framegen_core::gpu::backup_original_of(a),
        "backupPath": framegen_core::gpu::backup_path().map(|p| p.display().to_string()).unwrap_or_default(),
        "warnings": framegen_core::gpu::SPOOF_WARNINGS,
    }))
}

/// --deploytest <目录> / --restoretest <目录> / --downloadtest：
/// 和界面上「部署 / 还原 / 下载」按钮走**同一段代码**，结果写日志 + stdout 就退出。
///
/// 用途：用户报「点部署没反应」时，让他带上参数跑一下把这个输出发过来；
/// 也让验证不必依赖点界面（顺便保证 sidecar 的部署/还原能力和 Tauri 版一致）。
fn selftest(argv: &[String]) -> bool {
    if let Some(i) = argv.iter().position(|a| a == "--deploytest") {
        if let Some(dir) = argv.get(i + 1) {
            match deploy_impl(dir, "version.dll", 1, framegen_core::update::DEFAULT_MAX_FRAMES) {
                Ok((msg, notes)) => {
                    framegen_core::log::line(&format!("[deploytest] 部署成功：{msg}"));
                    for n in &notes {
                        framegen_core::log::line(&format!("[deploytest]   · {n}"));
                    }
                    println!("部署成功：{msg}");
                    for n in notes {
                        println!("  · {n}");
                    }
                }
                Err(e) => {
                    framegen_core::log::line(&format!("[deploytest] 部署失败：{e}"));
                    println!("部署失败：{e}");
                    std::process::exit(1);
                }
            }
            std::process::exit(0);
        }
    }

    // --downloadtest：把资产下全（代理入口 + INI + 两个 DLSS 运行库）。
    if argv.iter().any(|a| a == "--downloadtest") {
        let work = (|| -> Result<String, String> {
            let client = framegen_core::update::client().map_err(|e| e.to_string())?;
            let state = framegen_core::update::load_state();
            let plan = framegen_core::update::plan_download(&client, "version.dll", &state, |msg| {
                framegen_core::log::line(&format!("[downloadtest] {msg}"));
            })
            .map_err(|e| e.to_string())?;
            let runtime = framegen_core::update::dlss_runtime_plan(&client);
            let total = plan.total + runtime.iter().map(|s| s.size).sum::<u64>();
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let n = framegen_core::update::sync_assets(
                &client,
                &plan.items,
                &cancel,
                "https://gh-proxy.com/",
                if total > 0 { total } else { 1 },
                |msg, f| {
                    framegen_core::log::line(&format!("[downloadtest] {:>3.0}% {msg}", f * 100.0));
                },
            )
            .map_err(|e| e.to_string())?;
            let ctx = framegen_core::update::ProgressCtx {
                base_bytes: plan.total,
                total_bytes: if total > 0 { total } else { 1 },
                base_step: plan.items.len(),
                total_steps: plan.items.len() + runtime.len(),
            };
            let paths = framegen_core::update::ensure_dlss_runtime(&client, &cancel, &runtime, ctx, |msg, f| {
                framegen_core::log::line(&format!("[downloadtest] {:>3.0}% {msg}", f * 100.0));
            })
            .map_err(|e| e.to_string())?;
            let names: Vec<String> = paths
                .iter()
                .map(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                })
                .collect();
            Ok(format!(
                "下载 {n} 个文件（跳过 {} 个已是最新）｜运行库：{}",
                plan.skipped,
                names.join("、")
            ))
        })();
        match work {
            Ok(m) => {
                framegen_core::log::line(&format!("[downloadtest] 成功：{m}"));
                println!("下载成功：{m}");
            }
            Err(e) => {
                framegen_core::log::line(&format!("[downloadtest] 失败：{e}"));
                println!("下载失败：{e}");
                std::process::exit(1);
            }
        }
        std::process::exit(0);
    }

    // --dlss5test <目录> [api] [--restore]：DLSS5 的部署 / 还原链路走一遍。
    // 和界面按钮走的是同一段 core 代码，用户报「点了没反应」时让他带上参数跑一次即可。
    if let Some(i) = argv.iter().position(|a| a == "--dlss5test") {
        if let Some(dir) = argv.get(i + 1) {
            let rest: Vec<String> = argv[i + 2..].to_vec();
            let want_restore = rest.iter().any(|a| a == "--restore");
            let api_key = rest
                .iter()
                .find(|a| !a.starts_with("--"))
                .cloned()
                .unwrap_or_else(|| framegen_core::dlss5::API_AUTO.to_owned());
            let dir_p = PathBuf::from(dir);
            if want_restore {
                match framegen_core::dlss5::restore(&dir_p) {
                    Ok(m) => println!("DLSS5 还原成功：{m}"),
                    Err(e) => {
                        framegen_core::log::line(&format!("[dlss5test] 还原失败：{e}"));
                        println!("DLSS5 还原失败：{e}");
                        std::process::exit(1);
                    }
                }
                std::process::exit(0);
            }
            let api = framegen_core::dlss5::api_from_key(&api_key).unwrap_or(framegen_core::scan::GraphicsApi::Unknown);
            let r = framegen_core::dlss5::deploy(
                &dir_p,
                None,
                api,
                framegen_core::dlss5::BACKEND_RESHADE,
                &mut |m: String, f: f32| {
                    framegen_core::log::line(&format!("[dlss5test] {:>3.0}% {m}", f * 100.0));
                },
            );
            match r {
                Ok((msg, notes)) => {
                    framegen_core::log::line(&format!("[dlss5test] 部署成功：{msg}"));
                    println!("DLSS5 部署成功：{msg}");
                    for n in notes {
                        println!("  · {n}");
                    }
                }
                Err(e) => {
                    framegen_core::log::line(&format!("[dlss5test] 部署失败：{e}"));
                    println!("DLSS5 部署失败：{e}");
                    std::process::exit(1);
                }
            }
            std::process::exit(0);
        }
    }

    if let Some(i) = argv.iter().position(|a| a == "--restoretest") {
        if let Some(dir) = argv.get(i + 1) {
            match framegen_core::deploy::restore(Path::new(dir)) {
                Ok(msg) => {
                    framegen_core::log::line(&format!("[restoretest] 还原成功：{msg}"));
                    println!("还原成功：{msg}")
                }
                Err(e) => {
                    framegen_core::log::line(&format!("[restoretest] 还原失败：{e}"));
                    println!("还原失败：{e}");
                    std::process::exit(1);
                }
            }
            std::process::exit(0);
        }
    }

    false
}

fn main() {
    // 提权子进程模式（--gpuspoof-*）要在任何别的事情之前处理：它不读 stdin、不建日志轮转，
    // 干完写个结果文件就走 —— 这样 ShellExecuteW("runas") 拉起来的那个进程不会一直挂着。
    let argv: Vec<String> = std::env::args().collect();
    if gpuspoof_child(&argv) {
        std::process::exit(0);
    }

    // 一次性搬迁：老版本的备份在 %APPDATA%，现在放到程序同级（和 egui 版一致）
    let _ = framegen_core::util::migrate_backups();
    // 更老那一代（程序还叫 DLSSG-Manager）的部署记录：搬过来，否则老用户升级后
    // 只能看到「已安装，本工具无记录」，还原按钮永远是灰的
    if let Some(msg) = framegen_core::util::migrate_legacy_dlssg_backups() {
        framegen_core::log::line(&msg);
    }

    // 日志：程序同级 logs\。**这一块不能少** —— core 里所有 log::line 都靠它，
    // 没 init 的话 log_tail 永远是空的，用户报「扫不出 / 下不动」时一句线索都没有。
    if let Some(p) = framegen_core::log::init() {
        framegen_core::log::line(&format!(
            "FrameGen Manager (Electron sidecar) v{}",
            framegen_core::update::SELF_VERSION
        ));
        framegen_core::log::line(&format!("日志文件: {}", p.display()));
        framegen_core::log::line(&format!(
            "Windows 构建号: {:?}",
            framegen_core::gpu::windows_build()
        ));
        framegen_core::log::line(&format!(
            "exe: {}",
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|e| format!("未知（{e}）"))
        ));
        framegen_core::log::line(&format!(
            "assets: {:?}",
            framegen_core::util::assets_dir().map(|p| p.display().to_string())
        ));
        framegen_core::log::line(&format!(
            "backups: {:?}",
            framegen_core::util::backups_dir().map(|p| p.display().to_string())
        ));
        framegen_core::log::line(&format!(
            "游戏库缓存: {}",
            framegen_core::scan::library_path().display()
        ));
        framegen_core::log::line(&format!(
            "显卡: {:?}",
            framegen_core::gpu::nvidia_smi_gpu_name()
        ));
    }

    // panic 只可能来自某条命令内部（外面已经 catch_unwind 兜住）。默认 hook 会把
    // 信息打到 stderr（Electron 会收进它自己的日志），这里再补一行到我们的日志文件：
    // 用户报「某个按钮点了没反应」时，logs\framegen.log 里就能直接看到原因。
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        framegen_core::log::line(&format!("[sidecar] panic: {info}"));
        default_hook(info);
    }));

    // 命令行自测分支（和 Tauri 版同一套参数、同一套输出）
    let argv: Vec<String> = std::env::args().collect();
    if selftest(&argv) {
        return;
    }

    // 清掉**很久没动过**（7 天）的 .part 与自更新残留；最近下到一半的 .part 会留着 ——
    // 断点续传就靠它（clean_stale_partials 里有 7 天的门槛，别指望它每次启动都清干净）
    let _ = framegen_core::update::clean_stale_partials();
    let _ = framegen_core::update::cleanup_self_update_leftovers();

    // Tauri 版这里是 tauri::Builder...run()；sidecar 换成一个 stdio 循环：
    // 进程活到 stdin 关闭（或被打断）为止。
    serve();
    // 正常退出码 0：Electron 关窗口时会先 end() stdin，不该看到非零退出的痕迹。
    std::process::exit(0);
}
