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
    json!({ "id": id, "ok": false, "error": msg, "code": err_code(msg) })
}

/// 给错误分个**稳定代号**（消息原文不变）：用户截图报障时，一句 `code` 就能定位是哪一类。
fn err_code(msg: &str) -> &'static str {
    framegen_core::err::classify(msg)
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
    let driver_major = drv.as_ref().and_then(|d| {
        d.marketing
            .split('.')
            .next()
            .and_then(|s| s.trim().parse::<u32>().ok())
    });
    let out = json!({
        "name": name,
        "driver": driver,
        "route": route.label(),
        "routeKey": route.key(),
        // null = 放行；有字符串 = 禁止部署，字符串本身就是给用户看的理由
        "gate": route.gate(),
        "hags": framegen_core::gpu::hags_state().label(),
        "driverSrc": driver_src,
        // 主版本号（如 621.01 -> 621）：给前端做“R580 门槛”判断，避免前端解析各种格式
        "driverMajor": driver_major,
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

/// 「修复内置资产」：把安装目录 bundled-assets 里缺的（或大小不对的）补进资产目录。
///
/// 界面在发现资产缺文件时调用一次；补不回来的（源里也没有）原样返回，
/// 由界面提示「多半被杀毒软件删了，加白名单后重装」。
fn assets_repair() -> Result<Value, String> {
    let r = framegen_core::update::repair_assets_from_bundle()
        .map_err(|e| format!("{e:#}"))?;
    framegen_core::log::line(&format!(
        "[assets] 修复内置资产：补回 {}、原样 {}、两份都缺 {}、失败 {} ｜ 资产 {} ｜ 内置 {}",
        r.repaired.len(),
        r.present,
        r.missing.len(),
        r.failed.len(),
        r.dir.display(),
        r.bundle
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "（无）".to_owned())
    ));
    Ok(json!({
        "dir": r.dir.display().to_string(),
        "bundle": r.bundle.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        "repaired": r.repaired,
        "missing": r.missing,
        "failed": r.failed,
    }))
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

    /* 0.4.0 起运行库/后端/内核都在 <资产目录>\dlssg_sm86\ 里，**不在资产目录顶层**。
       以前这里是"在顶层找 nvngx_dlssg.dll / nvngx_dlss.dll"，于是新版资产一装上来
       卡片就报「缺 2 个运行库」——用户在界面上看到的是"资产不完整"，其实好好的。
       现在按分发表检查（只看在不在、大小对不对），并把整个目录摊开列给人看。 */
    let mut runtime_files: Vec<Value> = Vec::new();
    let mut sr_files: Vec<Value> = Vec::new();
    for (rel, size) in framegen_core::deploy::runtime_dir_files(&dir) {
        let item = json!({
            "name": rel,
            "present": true,
            "size": size,
            "hint": if rel.starts_with("sr\\") { "超分（可选，出厂关闭）" }
                    else if rel.starts_with("xefg\\") { "XeSS-FG（可选，出厂关闭）" }
                    else { "" },
        });
        if rel.starts_with("sr\\") {
            sr_files.push(item);
        } else {
            runtime_files.push(item);
        }
    }
    // 缺什么按分发表算：必备的进 missing，可选的（超分那两个）只列出来不报警
    let gaps = framegen_core::deploy::asset_inventory_gaps(&dir);
    missing += gaps.len();
    let mut groups: Vec<Value> = vec![
        json!({ "title": "帧生成配置", "files": [ json!({ "name": ini, "present": ini_present, "size": size_of(ini), "hint": "" }) ] }),
        json!({ "title": "帧生成 Mod 代理入口", "files": proxy_files }),
    ];
    if !runtime_files.is_empty() {
        groups.push(json!({
            "title": format!("运行库目录 {}\\", framegen_core::deploy::RUNTIME_DIR),
            "files": runtime_files,
        }));
    }
    if !sr_files.is_empty() {
        groups.push(json!({ "title": "超分文件（可选）", "files": sr_files }));
    }
    let groups = Value::Array(groups);

    json!({
        "dir": dir.display().to_string(),
        "files": files,
        "groups": groups,
        "missing": missing,
    })
}

/// Smooth Motion 适用性（**只读**）：显卡/驱动门槛 + 本机 NvPresent 构建。
/// 门槛按产品规定：驱动 >= 616.64 才允许部署。**全部只读**，不写任何驱动配置。
fn sm_info(args: &Value) -> Result<Value, String> {
    let gate = framegen_core::sm::gate();
    // NvPresent 只作为内部诊断写日志，**不上界面**（用户要求不出现"未被验证"之类的字样）。
    let nvp_all = framegen_core::sm::nvpresent_all();
    if let Some(first) = nvp_all.first() {
        framegen_core::log::line(&format!(
            "[sm] NvPresent {} 字节 sha256={} 已知构建={}",
            first.bytes, first.sha256, first.recognized
        ));
    }
    let nvp: Vec<Value> = nvp_all
        .into_iter()
        .map(|n| json!({ "path": n.path, "bytes": n.bytes, "sha256": n.sha256 }))
        .collect();
    // 驱动【基础/全局 profile】里 SM 开关的现状（只读；本工具绝不写这一层）
    let base = framegen_core::sm::base_sm_state();
    // 可选：给一个游戏 EXE，就读它在驱动里对应 profile 的 SM 值（只读）
    let exe = arg_opt_str(args, "exe")?;
    let exe_state = exe.map(|p| {
        let st = framegen_core::sm::exe_sm_state(std::path::Path::new(&p));
        json!({
            "exe": p,
            "found": st.found,
            "state": st.state.state,
            "value": st.state.value,
            "matched": st.matched,
            "profileCount": st.profile_count,
            "sampleApps": st.sample_apps,
            "diag": st.diag,
        })
    });
    Ok(json!({
        "minDriver": framegen_core::sm::MIN_SM_DRIVER_TEXT,
        "isSm86": gate.is_sm86,
        "gpu": gate.gpu_name,
        "driver": gate.driver,
        "driverVersion": gate.version.map(|(a, b)| format!("{a}.{b}")),
        "driverSource": gate.source,
        "allowed": gate.allowed,
        "reason": gate.reason,
        "nvpresent": nvp,
        "profileBase": { "state": base.state, "value": base.value },
        "profileExe": exe_state,
    }))
}


/// 开启 / 关闭某个游戏的 Smooth Motion。
///
/// 目标 profile = **这个 EXE 真正归属的那一个**（问驱动的 DRS_FindApplicationByName）：
/// NVIDIA 预置 profile 就直接写它；驱动里没有归属才自建我们自己的那个。
/// 写之前把旧值记进备份目录（dir 决定记录名），关闭/还原时写回。
fn sm_set_game(args: &Value) -> Result<Value, String> {
    let name = arg_str(args, "name")?;
    let exe = arg_str(args, "exe")?;
    let _ = checked_exe(&exe)?;
    let on = args.get("on").and_then(|v| v.as_bool()).unwrap_or(true);
    // 备份记录的名字取自**游戏目录**。界面传的是库里那个目录；没传时按 EXE 的上一级推 ——
    // 渲染 EXE 常常在子目录里（…\Binaries\Win64\x.exe）。
    // 两种算法对"EXE 就在游戏根目录"的游戏结果一样，对别的游戏也稳定（同一个键写、同一个键还原）。
    let dir = arg_opt_str(args, "dir")?.filter(|d| !d.is_empty()).unwrap_or_else(|| {
        std::path::Path::new(&exe)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    });
    let r = match framegen_core::sm::set_game_sm(
        &name,
        std::path::Path::new(&dir),
        std::path::Path::new(&exe),
        on,
    ) {
        Ok(r) => r,
        Err(e) => {
            // 成功/失败都要进日志：用户贴日志来排障时，这里就是答案
            framegen_core::log::line(&format!("[sm] set_game_sm name={name} on={on} 失败: {e}"));
            return Err(e);
        }
    };
    framegen_core::log::line(&format!(
        "[sm] set_game_sm name={name} dir={dir} on={on} profile={} ok={} readback={:?}",
        r.profile, r.ok, r.readback
    ));
    Ok(json!({
        "ok": r.ok,
        "readback": r.readback,
        "profile": r.profile,
        "message": r.message,
    }))
}


/// 【关闭】把某个游戏的 Smooth Motion **关掉**（写 0），不是"还原成旧值"。
/// 界面按钮写的是「关闭 Smooth Motion」，点下去就必须是关。
fn sm_turn_off_game(args: &Value) -> Result<Value, String> {
    let name = arg_str(args, "name")?;
    let dir = arg_opt_str(args, "dir")?
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            arg_opt_str(args, "exe")
                .ok()
                .flatten()
                .and_then(|e| std::path::Path::new(&e).parent().map(|p| p.to_path_buf()))
        })
        .unwrap_or_default();
    let exe = arg_opt_str(args, "exe")?.unwrap_or_default();
    let msg = match framegen_core::sm::turn_off_game_sm(&name, &dir, std::path::Path::new(&exe)) {
        Ok(m) => m,
        Err(e) => {
            framegen_core::log::line(&format!("[sm] turn_off_game name={name} 失败: {e}"));
            return Err(e);
        }
    };
    framegen_core::log::line(&format!(
        "[sm] turn_off_game name={name} dir={}",
        dir.display()
    ));
    Ok(json!({ "ok": true, "message": msg }))
}

/// 【诊断】一个游戏目录里到底有没有我们的解锁组件、文件什么状态、删不删得掉。
///
/// 界面上的徽章「已被 Smooth Motion 占用」只看一件事：目录里的 version.dll 是不是
/// 我们那个组件（比对 SHA256）。这条命令把判断依据全部摊开，出问题时一次就能看清
/// 是"文件还在"还是"文件在别处"。
fn sm_component_check(args: &Value) -> Result<Value, String> {
    let dir = std::path::Path::new(&arg_str(args, "dir")?)
        .to_path_buf();
    let file = dir.join(framegen_core::sm::SM_ASSET_FILE);
    let exists = file.is_file();
    let sha = framegen_core::util::sha256_file(&file).unwrap_or_default();
    let ours = framegen_core::sm::has_our_asset(&dir);
    let readonly = std::fs::metadata(&file).map(|m| m.permissions().readonly()).unwrap_or(false);
    let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    // 能不能删：先试"独占打开"，被别的进程占着就会失败（share_mode 是 Windows 扩展）
    use std::os::windows::fs::OpenOptionsExt;
    let in_use = match std::fs::OpenOptions::new().read(true).write(true).share_mode(0).open(&file) {
        Ok(_) => false,
        Err(e) => matches!(e.raw_os_error(), Some(32) | Some(33)),
    };
    let record = framegen_core::util::backups_dir()
        .map(|b| b.join("sm-assets"))
        .ok()
        .filter(|d| d.is_dir())
        .map(|d| {
            std::fs::read_dir(d)
                .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    Ok(json!({
        "dir": dir.display().to_string(),
        "file": file.display().to_string(),
        "exists": exists,
        "ours": ours,
        "size": size,
        "sha256": sha,
        "expectedSha256": framegen_core::sm::SM_ASSET_SHA256,
        "readonly": readonly,
        "inUse": in_use,
        "smAssetRecords": record,
    }))
}

/// 把备份记录里的每个游戏都还原一遍（设置页的"全部还原"）。
/// 批量查询多个游戏现在的 SM 状态。
///
/// 状态来自**游戏真正归属的 profile**（NVIDIA 预置的那个也算），不再是"我们自建的
/// profile 里有没有值"。exes 里的顺序要和 names 对齐；缺 exe 的那些退回查我们自建
/// 的 profile（老记录还在的仍能显示）。
fn sm_game_states(args: &Value) -> Result<Value, String> {
    let names = args.get("names").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let exes = args.get("exes").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let sess = framegen_core::sm::open_session().ok();
    let mut out = Vec::new();
    for (i, n) in names.iter().enumerate() {
        let name = n.as_str().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let exe = exes
            .get(i)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let st = match (&sess, exe.is_empty()) {
            (Some(s), false) => framegen_core::sm::game_state_with(s, std::path::Path::new(&exe)),
            _ => {
                let mut g = framegen_core::sm::game_state(std::path::Path::new(&exe));
                if g.profile.is_empty() {
                    // 没有 EXE 路径（或驱动里没归属）时，退回按游戏名找我们自建的 profile
                    let ours_name = framegen_core::sm::game_profile_name(&name);
                    if let Ok(v) = framegen_core::sm::game_sm_value_in(&ours_name) {
                        g.profile = ours_name;
                        g.ours = true;
                        g.value = v;
                    }
                }
                g
            }
        };
        out.push(json!({
            "name": name,
            "exe": exe,
            "profile": st.profile,
            "ours": st.ours,
            "value": st.value,
            "detail": st.detail,
        }));
    }
    Ok(json!({ "states": out }))
}

/// 单个游戏的状态（诊断/自测）。
fn sm_game_state(args: &Value) -> Result<Value, String> {
    let exe = arg_str(args, "exe")?;
    let st = framegen_core::sm::game_state(std::path::Path::new(&exe));
    Ok(json!({
        "exe": st.exe,
        "profile": st.profile,
        "ours": st.ours,
        "value": st.value,
        "detail": st.detail,
    }))
}

/// 把内置解锁组件放进游戏目录（带备份与哈希复核）。
fn sm_asset_deploy(args: &Value) -> Result<Value, String> {
    let dir = arg_str(args, "dir")?;
    let msg = framegen_core::sm::asset_deploy(std::path::Path::new(&dir))?;
    Ok(json!({ "ok": true, "message": msg }))
}

/// 取回内置解锁组件（还原原文件或删除我们放的那份）。
fn sm_asset_restore(args: &Value) -> Result<Value, String> {
    let dir = arg_str(args, "dir")?;
    let _ = checked_dir(&dir)?;
    let exe = arg_opt_str(args, "exe")?.unwrap_or_default();
    let path = std::path::Path::new(&dir);
    let before = framegen_core::sm::has_our_asset(path);
    // 带 exe 时用"搜索版"：同一个游戏可能同时以安装根目录和 exe 所在子目录两条存在，
    // 组件放在子目录、用户在根目录那条点关闭时，只有搜索版才找得到它。
    let result = if exe.is_empty() {
        framegen_core::sm::asset_restore(path)
    } else {
        framegen_core::sm::asset_restore_searching(path, std::path::Path::new(&exe))
    };
    let after = framegen_core::sm::has_our_asset(path);
    // 徽章「已被 Smooth Motion 占用」只看这一件事：目录里的 version.dll 还是不是我们的组件。
    // 所以取回之后**必须**把前后状态写进日志 —— 徽章没清掉时，这一行就是答案。
    framegen_core::log::line(&format!(
        "[sm] 取回结果 dir={dir} 之前组件在={before} 之后组件在={after} ok={} msg={}",
        result.is_ok(),
        match &result {
            Ok(m) => m.clone(),
            Err(e) => e.clone(),
        }
    ));
    Ok(json!({ "ok": result.is_ok(), "message": result?, "oursBefore": before, "oursAfter": after }))
}


/// 【只读】问驱动：这个 exe 属于哪个 profile、那个 profile 里 SM 是什么值。
fn sm_profile_of(args: &Value) -> Result<Value, String> {
    let exe = arg_str(args, "exe")?;
    let sess = framegen_core::sm::open_session_for_probe()?;
    let p = std::path::Path::new(&exe);
    let prof = sess.find_profile_for_exe(p)?;
    match prof {
        None => Ok(json!({ "found": false })),
        Some(h) => {
            let name = sess.profile_name(h);
            let value = sess.read(h, framegen_core::sm::SM_SETTING_ID).ok();
            Ok(json!({ "found": true, "profile": name, "value": value }))
        }
    }
}

/// 备份目录（界面上显示）
fn backups_dir() -> String {
    framegen_core::util::backups_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// 备份占用情况：条目数 + 总字节（设置页显示「N 项 · X MB」）。
fn backups_info() -> Value {
    let (count, bytes) = framegen_core::util::backups_report();
    let entries = framegen_core::util::backups_entries()
        .into_iter()
        .map(|(name, kind, size, target)| {
            json!({ "name": name, "kind": kind, "bytes": size, "target": target })
        })
        .collect::<Vec<_>>();
    json!({
        "dir": backups_dir(),
        "count": count,
        "bytes": bytes,
        "entries": entries,
    })
}

/// 清理旧备份。**删了已经部署到游戏里的 mod 就还原不回去** —— 界面必须先确认。
fn clean_backups() -> Result<Value, String> {
    let (removed, freed) = framegen_core::util::clean_backups().map_err(|e| format!("{e:#}"))?;
    let (count, bytes) = framegen_core::util::backups_report();
    Ok(json!({
        "removed": removed,
        "freed": freed,
        "count": count,
        "bytes": bytes,
    }))
}

/// 日志尾部（给「复制诊断信息」用）。
///
/// 除了 sidecar 自己的 framegen.log，还附上主进程的 shell.log：资产铺设失败
/// （「[bundled] 复制失败 ...」「version.dll 不存在」）只记在那边，偏偏就是
/// 「资产目录里没有 version.dll」这类问题最关键的线索。
fn log_tail(lines: usize) -> String {
    let mut out = String::new();
    if let Some(p) = framegen_core::log::path() {
        if let Ok(t) = std::fs::read_to_string(p) {
            let all: Vec<&str> = t.lines().collect();
            let start = all.len().saturating_sub(lines);
            out.push_str(&all[start..].join("\n"));
        }
    }
    if let Some(dir) = framegen_core::log::dir() {
        let p = dir.join("shell.log");
        if let Ok(t) = std::fs::read_to_string(p) {
            let all: Vec<&str> = t.lines().collect();
            let start = all.len().saturating_sub(lines);
            if !out.is_empty() {
                out.push_str("\n\n=== 主进程日志（shell.log） ===\n");
            }
            out.push_str(&all[start..].join("\n"));
        }
    }
    out
}

/// 导出诊断包：日志 + 结构化事件 + 配置 + 版本，打成 zip（不含游戏库 / 存档）。
///
/// 为什么要做：用户往往只发一句「用不了」，日志页复制的 2000 行也可能截掉关键上下文。
/// 这里把 logs 目录里的当前/上一份日志、events.jsonl、shell.log 和脱敏后的配置一起收走，
/// 我们拿到 zip 就能还原「哪条命令、什么参数、多久、成功还是失败、什么错误码」。
fn diagnostics_export() -> Result<Value, String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let out = std::env::temp_dir().join(format!("fgm-diagnostics-{stamp}-{}", std::process::id()));
    std::fs::create_dir_all(&out).map_err(|e| format!("创建诊断目录失败：{e}"))?;

    let logs = framegen_core::log::dir();
    if let Some(d) = &logs {
        for name in [
            framegen_core::log::CUR_NAME,
            framegen_core::log::PREV_NAME,
            framegen_core::log::EVENTS_NAME,
            framegen_core::log::EVENTS_PREV_NAME,
            "shell.log",
            "shell.prev.log",
        ] {
            let src = d.join(name);
            if src.is_file() {
                let _ = std::fs::copy(&src, out.join(name));
            }
        }
    }
    if let Some(root) = framegen_core::util::data_root() {
        let cfg = root.join("framegen-manager.json");
        if cfg.is_file() {
            if let Ok(text) = std::fs::read_to_string(&cfg) {
                let _ = std::fs::write(
                    out.join("framegen-manager.json"),
                    framegen_core::util::redact(&text),
                );
            }
        }
    }

    let mut info = String::new();
    info.push_str(&format!(
        "FrameGen Manager sidecar v{}\n",
        framegen_core::update::SELF_VERSION
    ));
    info.push_str(&format!(
        "Windows build: {:?}\n",
        framegen_core::gpu::windows_build()
    ));
    if let Some(d) = &logs {
        info.push_str(&format!("Logs dir: {}\n", d.display()));
    }
    if let Some(root) = framegen_core::util::data_root() {
        info.push_str(&format!("Data root: {}\n", root.display()));
    }
    let _ = std::fs::write(out.join("version.txt"), framegen_core::util::redact(&info));
    let _ = std::fs::write(
        out.join("README.txt"),
        "FrameGen Manager 诊断包\n\n         内容：framegen.log / framegen.prev.log / events.jsonl / shell.log / 脱敏后的配置 / 版本信息。\n         不含：游戏存档、账号信息、游戏库列表。\n         日志里可能有 Windows 用户名与游戏安装路径；转发前请自行确认。\n",
    );

    // 用 Windows 自带的 tar.exe 打 zip（不引入新依赖）；失败就返回文件夹，界面仍可打开。
    let zip = std::env::temp_dir().join(format!("FrameGen-Manager-diagnostics-{stamp}.zip"));
    let _ = std::fs::remove_file(&zip);
    let mut cmd = std::process::Command::new("tar");
    cmd.args(["-a", "-c", "-f"])
        .arg(&zip)
        .arg("-C")
        .arg(&out)
        .arg(".");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW：不闪黑框
    }
    let zipped = cmd.status().map(|s| s.success()).unwrap_or(false) && zip.is_file();
    let path = if zipped { zip } else { out.clone() };
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    framegen_core::log::line(&format!(
        "[diagnostics] 导出诊断包：{}（zip={zipped}）",
        path.display()
    ));
    Ok(json!({
        "path": path.display().to_string(),
        "dir": out.display().to_string(),
        "zip": zipped,
        "bytes": bytes,
    }))
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
        .map_err(|e| format!("{e:#}"))
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
    json!({
        "name": c.entry.name,
        "dir": install.display().to_string(),
        "target": target.display().to_string(),
        "source": source,
        "appId": c.entry.app_id,
        "api": c.api.label(),
        "techScanned": c.tech_scanned,
        "manual": manual,
        "exists": install.is_dir(),
        "deployed": framegen_core::deploy::state_of(&target).label(),
        // DLSS5 那一套的状态与它要用的启动 exe：部署对话框直接把这两项显示出来，
        // 免得再单独发一条命令去问（每行多读一个小 JSON，可以忽略）。
        "dlss5": framegen_core::dlss5::state_label(&target),
        // 用户上次在 DLSS5 部署框里手动选的图形 API（没选过是 null）：
        // 界面打开对话框时按它预设下拉框。
        "dlss5Api": c.dlss5_api,
        // 帧生成资产版本（徽章用）：哪一套 + 运行库版本 + 还留着哪些旧文件。
        // 一次 stat 级别的判定，跟着游戏库一起返回，界面不用再单独问一遍。
        "fgAssets": fg_assets_json(&target),
        "targetExe": c.render_exe.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
    })
}

/// 帧生成资产版本的 JSON 形态（界面的徽章、重新部署、清理入口都用它）。
fn fg_assets_json(dir: &std::path::Path) -> Value {
    match framegen_core::deploy::deployed_assets(dir) {
        None => Value::Null,
        Some(a) => json!({
            "version": a.family.version(),
            "legacy": a.family.is_legacy(),
            "runtime": a.runtime_version,
            "proxy": a.proxy,
            "manual": a.manual,
            "leftovers": a.leftovers.iter().map(|(n, s)| json!({ "name": n, "size": s })).collect::<Vec<_>>(),
        }),
    }
}

/// 清掉目录里 0.3.x 的遗留文件（用户点按钮时才走这里，逐个核对哈希再删）。
fn clean_legacy(dir: String) -> Result<Value, String> {
    let _ = checked_dir(&dir)?;
    let p = std::path::Path::new(&dir);
    let (deleted, failed) = framegen_core::deploy::clean_legacy_leftovers(p);
    framegen_core::log::line(&format!(
        "[清理旧版] {} -> 删掉 {} 项，失败 {} 项",
        p.display(),
        deleted.len(),
        failed.len()
    ));
    let mut msg = if deleted.is_empty() {
        "没有可清理的旧版文件（都已经干净了）".to_owned()
    } else {
        format!("已清理 {} 项旧版文件：{}", deleted.len(), deleted.join("、"))
    };
    if !failed.is_empty() {
        msg.push_str(&format!("\n· 没删掉的：{}", failed.join("、")));
    }
    Ok(json!({ "msg": msg, "deleted": deleted, "failed": failed }))
}

/// 真实游戏库：scanned + manual 合并，去掉用户移除过的，算好部署目标与状态。
fn list_games() -> Result<Value, String> {
    let cache = framegen_core::scan::load_library();
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

/// 扫出来的条目 -> 缓存行：找渲染 EXE、判图形 API。
///
/// 反作弊 / 游戏引擎 / 是否自带 DLSS 三项检测已移除（用户要求）：
/// 它们要么误报多、要么对部署没有实际作用，还拖慢扫描。
fn build_cached(entry: framegen_core::scan::GameEntry) -> framegen_core::scan::CachedRow {
    let render_exe = framegen_core::scan::find_render_exe(&entry.install_dir);
    let (api, tech_scanned) = match render_exe.as_deref() {
        Some(p) => {
            let rep = framegen_core::scan::detect_tech_report(p);
            framegen_core::log::line(&format!(
                "图形 API [{}] {} | {}",
                entry.name,
                rep.api.label(),
                rep.evidence.join("；")
            ));
            (rep.api, true)
        }
        None => (
            framegen_core::scan::GraphicsApi::Unknown,
            // 连渲染 EXE 都没找到：不是「算过」，等以后找到再算
            false,
        ),
    };
    framegen_core::scan::CachedRow {
        entry,
        render_exe,
        api,
        tech_scanned,
        dlss5_api: None,
    }
}

/// 多线程探测（只对**新增 / 需要刷新**的游戏做）：找渲染 exe、PE 分析、反作弊扫描
/// 都是只读重活，串行做几百个游戏会要命（用户反馈启动等 20 分钟）。
fn parallel_build_cached(entries: Vec<framegen_core::scan::GameEntry>) -> Vec<framegen_core::scan::CachedRow> {
    if entries.len() <= 1 {
        return entries.into_iter().map(build_cached).collect();
    }
    let n = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(4)
        .min(4)
        .min(entries.len());
    let mut chunks: Vec<Vec<framegen_core::scan::GameEntry>> = (0..n).map(|_| Vec::new()).collect();
    for (i, e) in entries.into_iter().enumerate() {
        chunks[i % n].push(e);
    }
    std::thread::scope(|s| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|c| s.spawn(move || c.into_iter().map(build_cached).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

/// 游戏目录在上次扫描之后有没有被改过（游戏更新、换内容）。
fn dir_touched_since(dir: &std::path::Path, since_epoch: u64) -> bool {
    if since_epoch == 0 {
        return false;
    }
    let Ok(m) = std::fs::metadata(dir) else {
        return false;
    };
    let Ok(t) = m.modified() else {
        return false;
    };
    let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    d.as_secs() > since_epoch
}

/// 扫描游戏库并落盘。
///
/// **模式**（性能的关键）：
///   * `startup`：启动时的自动刷新。只读启动器清单（毫秒级）、不枚举散装目录、
///     老游戏零探测、直接用缓存 —— 启动秒开（用户反馈：游戏多时等 20 分钟）。
///     缓存 schema 过期时**不**在这里补探测，只返回 `stale:true`，由界面另发
///     一次静默的 `reprobe`（不阻塞启动）。
///   * `manual`：手动扫描（用户选了「连同散装目录」）。连散装目录一起枚举；
///     老游戏只在目录被改过 / 没找到过渲染 exe / 技术字段没算过时才重新探测。
///   * `manual_fast`：手动扫描（用户只选「仅启动器」），枚举启动器清单，不做散装枚举。
///   * `reprobe`：schema 升级后的静默重探测 —— 全部游戏重新探测，分批汇报进度。
///
/// 探测分批（每批 4 个）并汇报「正在识别（N/M）」；落盘不能省。手动条目与忽略清单原样保留。
fn scan_library(id: u64, mode: &str) -> Result<Value, String> {
    progress(id, "正在扫描游戏库…", 0.0);
    let startup = mode == "startup";
    let reprobe = mode == "reprobe";
    let with_loose = mode == "manual";
    let (entries, notes) = {
        let mut notes = Vec::new();
        let mut all = framegen_core::scan::scan_launcher_notes(&mut notes);
        if with_loose {
            all.extend(framegen_core::scan::scan_loose_notes(&mut notes));
        }
        (all, notes)
    };
    let old = framegen_core::scan::load_library();
    let schema_stale = old.schema != framegen_core::scan::LIBRARY_SCHEMA;
    if startup && schema_stale && !old.scanned.is_empty() {
        // 启动路径绝不在这里补探测：只报告过期，界面随后静默发 reprobe
        return Ok(json!({ "count": old.scanned.len(), "notes": notes, "saved": true, "stale": true }));
    }
    let full_probe = reprobe || schema_stale;
    let key_of = |p: &std::path::Path| p.display().to_string().to_lowercase();
    let mut by_dir: std::collections::HashMap<String, framegen_core::scan::CachedRow> = old
        .scanned
        .iter()
        .cloned()
        .map(|r| (key_of(&r.entry.install_dir), r))
        .collect();
    let mut rows: Vec<framegen_core::scan::CachedRow> = Vec::new();
    let mut probe: Vec<framegen_core::scan::GameEntry> = Vec::new();
    // 需要重探测的旧行：手动选的图形 API 要带回去（别让扫描冲掉用户的决定）
    let mut keep_api: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    for e in entries {
        let k = key_of(&e.install_dir);
        match by_dir.remove(&k) {
            Some(row) => {
                let verbose = full_probe
                    || ((mode == "manual" || mode == "manual_fast")
                        && (dir_touched_since(&e.install_dir, old.scanned_epoch)
                            || row.render_exe.is_none()
                            || !row.tech_scanned));
                if verbose {
                    keep_api.insert(k, row.dlss5_api.clone());
                    probe.push(e);
                } else {
                    // 元数据（名字/来源）可能变了，刷新它；探测结果与 dlss5_api 原样保留
                    rows.push(framegen_core::scan::CachedRow { entry: e, ..row });
                }
            }
            None => probe.push(e),
        }
    }
    // 分批探测：每批 4 个并行，批间汇报进度（手动扫描时用户能看到扫到第几个）
    let total = rows.len() + probe.len();
    let mut done = rows.len();
    let mut probe_entries = probe;
    while !probe_entries.is_empty() {
        let rest = probe_entries.split_off(probe_entries.len().min(4));
        let batch = std::mem::replace(&mut probe_entries, rest);
        let mut probed = parallel_build_cached(batch);
        for row in probed.iter_mut() {
            if let Some(api) = keep_api.get(&key_of(&row.entry.install_dir)) {
                row.dlss5_api = api.clone();
            }
        }
        done += probed.len();
        rows.extend(probed);
        if total > 0 {
            progress(
                id,
                &format!("正在识别游戏（{done}/{total}）…"),
                (done as f32 / total as f32).min(0.99),
            );
        }
    }
    let count = rows.len();
    let cache = framegen_core::scan::LibraryCache {
        schema: framegen_core::scan::LIBRARY_SCHEMA,
        scanned_at: framegen_core::util::now_utc(),
        scanned_epoch: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        scanned: rows,
        manual: old.manual,
        ignored: old.ignored,
    };
    let saved = framegen_core::scan::save_library(&cache).is_ok();
    progress(id, &format!("扫描完成，共 {count} 个游戏"), 1.0);
    Ok(json!({ "count": count, "notes": notes, "saved": saved, "stale": false }))
}

/// 打开系统设置里「硬件加速 GPU 计划」那一页（只跳转，绝不写注册表）。
///
/// URI 由 core 决定（gpu::hags_settings_uri），和 egui 版的「去设置」是同一个去处。
fn open_hags_settings() -> Result<Value, String> {
    framegen_core::gpu::open_hags_settings().map_err(|e| format!("{e:#}"))?;
    Ok(json!({ "uri": framegen_core::gpu::hags_settings_uri() }))
}

/// 手动把一个目录加进游戏库（扫不出来的游戏、或者用户自己的目录）。
///
/// 与 egui 版 add_to_library 同一套顺序：manual_entry 建条目 → 找渲染 EXE →
/// 由主进程弹好文件框、把选中的目录送进来（见 main.js 的说明：sidecar 弹的框没有父窗口，
/// Windows 会把它放到应用窗口后面，用户以为点了没反应）。
fn add_game_at(dir: String) -> Result<Value, String> {
    let d = PathBuf::from(dir);
    if !d.is_dir() {
        return Err(format!("这个目录不存在：{}", d.display()));
    }
    add_library_dir(&d, None)
}

/// 同上：主进程选好「启动 exe」，这里取它所在目录入库。
fn pick_game_exe_at(exe: String) -> Result<Value, String> {
    let e = PathBuf::from(exe);
    let Some(dir) = e.parent().map(|p| p.to_path_buf()) else {
        return Err(format!("这个文件没有所在目录：{}", e.display()));
    };
    let mut v = add_library_dir(&dir, Some(e))?;
    if let Some(o) = v.as_object_mut() {
        o.insert("fromExe".to_owned(), json!(true));
    }
    Ok(v)
}

/// 同上：主进程选好新的资产目录，这里写进配置（core 的缓存随即失效）。
/// detect_tech 判图形 API / 引擎 / 自带帧生成 → 反作弊扫一遍 → 存进 cache.manual。
/// **存 manual 而不是 scanned**：重新扫描不会把用户手动加的条目冲掉。
fn add_game() -> Result<Value, String> {
    // 用户定的规则：手动添加改为**选 exe**（不是选文件夹）——部署就落在 exe 所在的那一层，
    // 从根上避免"选了上层目录导致部署无效"的问题。
    let picked = rfd::FileDialog::new()
        .set_title("选择游戏实际启动的exe（通常在Win64文件夹下，根据实际情况选择exe！）")
        .add_filter("可执行文件", &["exe"])
        .pick_file();
    let Some(exe) = picked else {
        return Ok(json!({ "canceled": true }));
    };
    let Some(dir) = exe.parent().map(|p| p.to_path_buf()) else {
        return Ok(json!({ "canceled": true }));
    };
    add_library_dir(&dir, Some(exe))
}

/// 「手动选择游戏根目录 exe 启动文件」（DLSS5 页与部署对话框的按钮）。
///
/// 与「添加目录」的差别只有入口：这里选的是**启动 exe**，目录取它所在的那一层 ——
/// DLSS5 的四个文件全都铺在 exe 旁边，选错了目录等于白装。入库逻辑是同一份
/// （add_library_dir），所以两种加法的结果不会有第二套行为。
fn pick_game_exe() -> Result<Value, String> {
    let picked = rfd::FileDialog::new()
        .set_title("选择游戏实际启动的exe（通常在Win64文件夹下，根据实际情况选择exe！）")
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
    if let Some(e) = exe.as_deref() {
        check_exe_readable(e)?;
    }
    let mut cache = framegen_core::scan::load_library();
    // 用户又把它加回来了：从忽略清单里去掉，免得下次扫描/显示又被藏起来
    let key = framegen_core::scan::path_key(dir);
    cache.ignored.retain(|s| s != &key);

    let render_exe = match exe {
        Some(e) => Some(e),
        None => framegen_core::scan::find_render_exe(dir),
    };
    let api = render_exe
        .as_deref()
        .map(framegen_core::scan::detect_tech)
        .unwrap_or(framegen_core::scan::GraphicsApi::Unknown);

    // 已在库里（扫出来的或手动加的）：就地更新那一行。
    //
    // 三级查找，最后一级是修「同一游戏出现两张卡」：自动扫描记的是安装根目录
    // （…\HogwartsLegacy），而手动选 exe 拿到的是 exe 所在那一层
    // （…\Phoenix\Binaries\Win64）。只按"目录完全相同"匹配的话，手动选一次就会多出一张
    // 卡片，而且两张卡的"组件在不在"会各说各话 —— 所以先按目录找，再按"选中的 exe
    // 与这一行记的 render_exe 是同一个文件"找（在已入库条目的子目录里选 exe 就属于这种）。
    enum Hit { Scanned(usize), Manual(usize), New }
    let hit = if let Some(i) = cache.scanned.iter().position(|c| c.entry.install_dir == dir) {
        Hit::Scanned(i)
    } else if let Some(i) = cache.manual.iter().position(|c| c.entry.install_dir == dir) {
        Hit::Manual(i)
    } else if let Some(k) = render_exe.as_deref().map(framegen_core::scan::path_key) {
        if let Some(i) = cache
            .scanned
            .iter()
            .position(|c| c.render_exe.as_deref().map(framegen_core::scan::path_key).as_deref() == Some(k.as_str()))
        {
            Hit::Scanned(i)
        } else if let Some(i) = cache
            .manual
            .iter()
            .position(|c| c.render_exe.as_deref().map(framegen_core::scan::path_key).as_deref() == Some(k.as_str()))
        {
            Hit::Manual(i)
        } else {
            Hit::New
        }
    } else {
        Hit::New
    };
    let from_manual = matches!(hit, Hit::Manual(_));
    let existing = match hit {
        Hit::Scanned(i) => Some(&mut cache.scanned[i]),
        Hit::Manual(i) => Some(&mut cache.manual[i]),
        Hit::New => None,
    };
    let exe_out = render_exe
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    if let Some(row) = existing {
        let name = row.entry.name.clone();
        if from_manual {
            row.entry.source = framegen_core::scan::Launcher::Manual;
        }
        row.render_exe = render_exe;
        row.api = api;
        row.tech_scanned = true;
        framegen_core::scan::save_library(&cache).map_err(|e| format!("{e:#}"))?;
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
        api,
        tech_scanned: true,
        dlss5_api: None,
    });
    framegen_core::scan::save_library(&cache).map_err(|e| format!("{e:#}"))?;
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
    let client = framegen_core::update::client().map_err(|e| format!("{e:#}"))?;
    let (fetched, skipped, failed) =
        framegen_core::update::fetch_steam_covers(&client, &ids, |msg, f| progress(id, &msg, f))
            .map_err(|e| format!("{e:#}"))?;
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
    .map_err(|e| format!("{e:#}"))?;
    /* 用户手动选的图形 API 要跟着这个游戏存下来：只当本次参数用掉的话，下次打开部署框
       又回到「自动：未知」，得再选一遍。写缓存失败只记日志 —— 部署本身已经完成，
       不能因为这一步就把结果报成失败。 */
    if let Err(e) = remember_dlss5_api(&dir_p, &api_key) {
        framegen_core::log::line(&format!("记下 DLSS5 图形 API 失败：{e}"));
    }
    Ok(json!({ "msg": msg, "notes": notes }))
}

/// 把「这次在 DLSS5 部署框里选的图形 API」写进游戏库那一行。
///
/// 选「自动」= 回到自动识别，所以那一档是**清空**而不是写入一个 auto 值。
fn remember_dlss5_api(dir: &Path, api_key: &str) -> Result<(), String> {
    let mut cache = framegen_core::scan::load_library();
    let want = if api_key == framegen_core::dlss5::API_AUTO {
        None
    } else {
        Some(api_key.to_owned())
    };
    if let Some(c) = cache.scanned.iter_mut().find(|c| c.entry.install_dir == dir) {
        c.dlss5_api = want;
    } else if let Some(c) = cache.manual.iter_mut().find(|c| c.entry.install_dir == dir) {
        c.dlss5_api = want;
    } else {
        // 库里没有这一行（比如刚被移除）：不为了记一个选择去凭空造条目
        return Ok(());
    }
    framegen_core::scan::save_library(&cache).map_err(|e| format!("{e:#}"))
}

/// 探测这个目录里有没有 ReShade / DLSS5。
/// **不看我们的部署记录也认**：用户自己手动装过的同样能查出来（用户要求）。
fn dlss5_detect(dir: String) -> Result<Value, String> {
    let p = PathBuf::from(&dir);
    if !p.is_dir() {
        return Err(format!("目录不存在：{dir}"));
    }
    let f = framegen_core::dlss5::detect(&p);
    let mut v = serde_json::to_value(&f).map_err(|e| format!("{e:#}"))?;
    if let Some(o) = v.as_object_mut() {
        o.insert("label".to_owned(), json!(f.label()));
        o.insert("anything".to_owned(), json!(f.anything()));
        // 记录丢了的时候，界面要告诉用户"原件可能在哪儿"
        o.insert("backupsDir".to_owned(), json!(backups_dir()));
    }
    Ok(v)
}

/// 彻底卸载 ReShade + DLSS5（含效果包目录、插件、模型、插件运行时残渣）。
/// **永久删除、不进回收站** —— 界面必须先拿到用户二次确认才允许调这里。
fn dlss5_uninstall(id: u64, args: &Value) -> Result<Value, String> {
    let dir = arg_str(args, "dir")?;
    let _ = checked_dir(&dir)?;
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
    .map_err(|e| format!("{e:#}"))?;
    Ok(json!({ "msg": msg }))
}

/// 还原 DLSS5（只动本工具部署的那些文件；ReShade 本体按备份恢复/删除）。
fn dlss5_restore(dir: String) -> Result<Value, String> {
    let msg = framegen_core::dlss5::restore(Path::new(&dir)).map_err(|e| format!("{e:#}"))?;
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
    framegen_core::scan::save_library(&cache).map_err(|e| format!("{e:#}"))?;
    framegen_core::log::line(&format!("从游戏库移除：{}", p.display()));
    Ok(json!({ "ok": true }))
}

/// 把移除过的条目放回来（重新扫描 / 下次启动就会出现）
fn restore_removed(key: String) -> Result<Value, String> {
    let mut cache = framegen_core::scan::load_library();
    cache.ignored.retain(|s| s != &key);
    framegen_core::scan::save_library(&cache).map_err(|e| format!("{e:#}"))?;
    framegen_core::log::line(&format!("恢复被移除的条目：{key}"));
    Ok(json!({ "ok": true }))
}

// ---------------------------------------------------------------- 部署 / 还原

/// 部署状态（读现有 manifest）
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
        // 这个游戏疑似开着 Smooth Motion？帧生成部署前靠它弹那句提醒
        "smLikelyOn": a.sm_likely_on,
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
/// 把「超分预设」（0.4.1 五选一）落到配置；部署前调用，同一进程内先写后读。
/// 升级后**自动清除旧版帧生成部署**（0.3.x 布局或 0.4.0 资产）：还原原件 + 清残留。
/// 返回清掉的游戏清单；每个程序版本只做一次（写 fg_cleanup_version）。
fn framegen_cleanup_outdated() -> Result<Value, String> {
    let mut cfg = framegen_core::util::load_config();
    if cfg.fg_cleanup_version == framegen_core::update::SELF_VERSION {
        return Ok(json!({ "cleaned": [], "failed": [], "already": true }));
    }
    let cache = framegen_core::scan::load_library();
    let mut cleaned: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for row in cache.scanned.iter().chain(cache.manual.iter()) {
        let dir = row
            .render_exe
            .as_ref()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| row.entry.install_dir.clone());
        if !framegen_core::deploy::is_outdated(&dir) {
            continue;
        }
        // 用 restore_report：「部分还原」也算没清干净（文件被游戏 / 杀毒软件占着），
        // 不能报进 cleaned 里骗人。
        match framegen_core::deploy::restore_report(&dir) {
            Ok(o) if !o.complete() => failed.push(format!("{}：{}", row.entry.name, o.msg)),
            Ok(_) => {
                let (_del, bad) = framegen_core::deploy::clean_legacy_leftovers(&dir);
                if !bad.is_empty() {
                    failed.push(format!("{}：残留 {} 个文件", row.entry.name, bad.len()));
                }
                cleaned.push(row.entry.name.clone());
            }
            Err(e) => failed.push(format!("{}：{e}", row.entry.name)),
        }
    }
    cfg.fg_cleanup_version = framegen_core::update::SELF_VERSION.to_owned();
    let _ = framegen_core::util::save_config(&cfg);
    Ok(json!({ "cleaned": cleaned, "failed": failed }))
}

fn save_sr_mode(mode: &str) -> Result<(), String> {
    let v = mode.trim().to_owned();
    if v.is_empty() {
        return Ok(());
    }
    const OK: [&str; 5] = ["Off", "M-INT8", "L-INT8", "M-FP16", "L-FP16"];
    if !OK.iter().any(|x| x.eq_ignore_ascii_case(&v)) {
        return Err(format!("不支持的超分预设：{v}"));
    }
    let mut cfg = framegen_core::util::load_config();
    cfg.fg_sr_mode = v;
    framegen_core::util::save_config(&cfg).map_err(|e| format!("{e:#}"))
}

/// 所有“会往某个目录写/删文件”的命令都必须过这一关（P0-12）：
/// 必须是已存在、绝对路径、非系统目录。防止 IPC 参数被伪造或被脚本误调用时误伤系统。
fn checked_dir(dir: &str) -> Result<std::path::PathBuf, String> {
    if dir.trim().is_empty() {
        return Err("目标目录为空".to_owned());
    }
    let p = std::path::PathBuf::from(dir);
    if !p.is_absolute() {
        return Err(format!("目标目录必须是绝对路径：{dir}"));
    }
    let canon = p
        .canonicalize()
        .map_err(|e| format!("目标目录不存在或不可访问：{dir}（{e}）"))?;
    if !canon.is_dir() {
        return Err(format!("目标不是目录：{}", canon.display()));
    }
    let low = canon.to_string_lossy().to_ascii_lowercase();
    if low.contains("\\windows\\") {
        return Err(format!("拒绝在系统目录操作：{}", canon.display()));
    }
    Ok(canon)
}

/// 选中的 exe 必须能被当前进程读取。WindowsApps / XboxGames 受保护目录会直接拒绝，
/// 提前给用户一句能看懂的话，而不是让他添加成功后在部署时再撞墙。
fn check_exe_readable(p: &Path) -> Result<(), String> {
    match std::fs::File::open(p) {
        Ok(_) => Ok(()),
        Err(e) => {
            let s = p.to_string_lossy().to_lowercase();
            let protected = s.contains("\\windowsapps\\") || s.contains("\\xboxgames\\");
            if protected {
                Err(format!(
                    "这个 exe 在微软商店 / Xbox 的受保护目录里：{}\n\nWindows 拒绝访问，不允许普通程序读取它（提示「没有打开的权限」）。\n请不要修改 WindowsApps / XboxGames 的权限 —— 会破坏商店更新，也可能触发游戏反作弊。\n\n请改用 Steam / Epic / GOG 版，或把游戏装到普通文件夹后再添加。",
                    p.display()
                ))
            } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                Err(format!(
                    "这个 exe 现在读不了（Windows 拒绝访问）：{}\n\n如果是普通文件夹：右键文件 → 属性 → 安全，给当前用户「读取和执行」权限；\n或右键本程序「以管理员身份运行」再试。\n若游戏正在运行 / 被反作弊或杀毒软件占用，先完全退出游戏。",
                    p.display()
                ))
            } else {
                Err(format!("读取 exe 失败：{}（{e}）", p.display()))
            }
        }
    }
}

/// 校验 exe 路径（SM 命令按 exe 定位游戏）。
fn checked_exe(exe: &str) -> Result<std::path::PathBuf, String> {
    if exe.trim().is_empty() {
        return Err("缺少 exe 路径".to_owned());
    }
    let p = std::path::PathBuf::from(exe);
    if !p.is_absolute() {
        return Err(format!("exe 必须是绝对路径：{exe}"));
    }
    if !p.is_file() {
        return Err(format!("exe 不存在：{exe}"));
    }
    Ok(p)
}

// 参数一一对应界面上的部署档位下拉框；打包成结构体只会在这一层多绕一次。
#[allow(clippy::too_many_arguments)]
fn deploy_impl(
    dir: &str,
    proxy: &str,
    optimized: u8,
    frames: u8,
    preset: &str,
    sr: bool,
    engine: &str,
    xess_flow: &str,
    xess_hudless: &str,
) -> Result<(String, Vec<String>), String> {
    let _ = checked_dir(dir)?;
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
        return Err(format!(
            "倍率上限参数只能是 {}（=4X）或 5（=6X）（收到 {frames}）",
            framegen_core::update::DEFAULT_MAX_FRAMES
        ));
    }
    // 引擎 / 超分 / XeSS 附属键：白名单校验，别让前端传进来的怪值写进游戏 INI
    let engine = if engine.is_empty() { "DLSS" } else { engine };
    if engine != "DLSS" && engine != "XeSS" {
        return Err(format!("帧生成引擎只支持 DLSS / XeSS（收到 {engine}）"));
    }
    let flow = if xess_flow.is_empty() { "Middle" } else { xess_flow };
    if flow != "Coarse" && flow != "Middle" && flow != "Fine" {
        return Err(format!("光流分辨率只支持 Coarse / Middle / Fine（收到 {flow}）"));
    }
    framegen_core::log::line(&format!(
        "部署档位：优化等级 {optimized}、倍率上限 {frames}、超分 {}、引擎 {engine} ｜ {dir} ｜ 入口 {proxy}",
        if sr { "开" } else { "关" }
    ));
    framegen_core::deploy::deploy_game(
        Path::new(dir),
        proxy,
        optimized,
        frames,
        preset,
        sr,
        engine,
        flow,
        xess_hudless,
    )
    .map_err(|e| format!("{e:#}"))
}

// 同 deploy_impl：参数就是命令协议里的那一组字段。
#[allow(clippy::too_many_arguments)]
fn deploy_game(
    dir: String,
    proxy: String,
    optimized: u8,
    frames: u8,
    preset: Option<String>,
    sr: bool,
    engine: Option<String>,
    xess_flow: Option<String>,
    xess_hudless: Option<String>,
) -> Result<Value, String> {
    let preset = preset.unwrap_or_default();
    match deploy_impl(
        &dir,
        &proxy,
        optimized,
        frames,
        &preset,
        sr,
        engine.unwrap_or_default().as_str(),
        xess_flow.unwrap_or_default().as_str(),
        xess_hudless.as_deref().unwrap_or("Auto"),
    ) {
        Ok((msg, notes)) => Ok(json!({ "msg": msg, "notes": notes })),
        Err(e) => {
            /* 部署被拒的原因一律写进日志：用户报「装不上」时，这一行就是唯一的线索
               （界面上那句话未必会被完整转述过来）。 */
            framegen_core::log::line(&format!("部署被拒：{dir} ｜ 入口 {proxy} ｜ {e}"));
            Err(e)
        }
    }
}

/// 一键还原。
///
/// 两条路（用户要求都要能走通）：
///   * 有本工具的部署记录 -> deploy::restore_report：按 manifest 把游戏原件放回去、删掉我们写的文件；
///   * 没有记录、但目录里有**本项目的**帧生成文件（用户自己手动装的）-> cleanup_manual：
///     没有备份可恢复，所以只把认得出的那些文件删掉，删完复查并如实报告。
///
/// 返回 {msg, partial, leftover}：partial=true 表示目录里还有本工具的文件没删掉
/// （多半是游戏 / 杀毒软件占着），**部署记录仍然保留**。界面必须按「部分还原」显示，
/// 不能翻成「未部署」—— 否则用户只能去点「清理」，游戏原件就再也放不回来了。
fn restore(dir: String) -> Result<Value, String> {
    let _ = checked_dir(&dir)?;
    let p = Path::new(&dir);
    match framegen_core::deploy::restore_report(p) {
        Ok(o) => Ok(json!({
            "msg": o.msg,
            "partial": !o.complete(),
            "leftover": o.leftover,
        })),
        Err(e) => {
            // 只有确实找到「本项目的文件」时才走清理那条路；否则把原来的错误原样报出来
            if framegen_core::deploy::manual_files(p).is_empty() {
                return Err(e.to_string());
            }
            framegen_core::log::line(&format!(
                "没有部署记录，按「手动安装清理」处理：{}",
                p.display()
            ));
            let msg = framegen_core::deploy::cleanup_manual(p).map_err(|x| x.to_string())?;
            Ok(json!({ "msg": msg, "partial": false, "leftover": [] }))
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
/// 1.7.0 停更退场：清空所有游戏里的部署痕迹（只跑一次；失败下次启动重试）。
fn retire_cleanup() -> Result<Value, String> {
    let mut cfg = framegen_core::util::load_config();
    if cfg.retired {
        return Ok(json!({
            "ok": true, "already": true, "cleaned": [], "failed": [],
            "warnings": [], "targets": 0, "backupsRemoved": 0,
            "announced": cfg.retire_announced,
        }));
    }
    if !cfg.retire_pending {
        cfg.retire_pending = true;
        framegen_core::util::save_config(&cfg).map_err(|e| format!("{e:#}"))?;
    }
    framegen_core::log::line("[退场] 开始自动清理所有部署");
    let report = framegen_core::retire::cleanup_all();
    if report.ok() {
        cfg.retired = true;
        cfg.retire_pending = false;
        framegen_core::util::save_config(&cfg).map_err(|e| format!("{e:#}"))?;
    }
    Ok(json!({
        "ok": report.ok(),
        "already": false,
        "cleaned": report.cleaned,
        "failed": report.failed,
        "warnings": report.warnings,
        "targets": report.targets,
        "backupsRemoved": report.backups_removed,
        "backupsBytes": report.backups_bytes,
        "announced": cfg.retire_announced,
    }))
}

/// 公告已经展示过了（下次启动不再弹）。
fn retire_announce_seen() -> Result<Value, String> {
    let mut cfg = framegen_core::util::load_config();
    if !cfg.retire_announced {
        cfg.retire_announced = true;
        framegen_core::util::save_config(&cfg).map_err(|e| format!("{e:#}"))?;
    }
    Ok(json!({ "ok": true }))
}

fn get_config() -> Value {
    let cfg = framegen_core::util::load_config();
    let is_default = cfg.fg_optimized == framegen_core::update::DEFAULT_OPTIMIZED
        && cfg.fg_frames == framegen_core::update::DEFAULT_MAX_FRAMES
        && cfg.fg_preset.is_empty();
    json!({
        "optimized": cfg.fg_optimized,
        "frames": cfg.fg_frames,
        "preset": cfg.fg_preset,
        "autoScanOnStart": cfg.auto_scan_on_start,
        "srMode": cfg.fg_sr_mode,
        "assetsLegacyPrompted": cfg.assets_legacy_prompt_version,
        "retired": cfg.retired,
        "retireAnnounced": cfg.retire_announced,
        "legacyPrompted": cfg.legacy_prompt_version,
        "defaultPreset": framegen_core::update::DEFAULT_PRESET,
        "presets": framegen_core::update::PRESETS,
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

/// 扫一遍游戏库：哪些游戏的目录里还是**旧版** Smooth Motion 30 系组件。
///
/// 返回 { games: [名字…], prompt: 是否该弹提示 } —— prompt 按程序版本记一次，
/// 用户忽略后不会每次启动都烦他（换新版会再提醒一次）。
fn sm_legacy_scan() -> Result<Value, String> {
    let cache = framegen_core::scan::load_library();
    let mut games: Vec<String> = Vec::new();
    for row in cache.scanned.iter().chain(cache.manual.iter()) {
        if framegen_core::sm::has_legacy_asset(&row.entry.install_dir) {
            games.push(row.entry.name.clone());
        }
    }
    let cfg = framegen_core::util::load_config();
    let prompt = !games.is_empty()
        && !cfg.sm_legacy_prompt_version.eq(framegen_core::update::SELF_VERSION);
    Ok(json!({ "games": games, "prompt": prompt }))
}

/// 扫一遍游戏库：哪些游戏的目录里还是**旧版 DLSS5（RenoDX）**——需先卸载再重装。
fn dlss5_legacy_scan() -> Result<Value, String> {
    let cache = framegen_core::scan::load_library();
    let mut games: Vec<String> = Vec::new();
    for row in cache.scanned.iter().chain(cache.manual.iter()) {
        if framegen_core::dlss5::has_legacy_renodx(&row.entry.install_dir) {
            games.push(row.entry.name.clone());
        }
    }
    let cfg = framegen_core::util::load_config();
    let prompt = !games.is_empty()
        && !cfg
            .dlss5_legacy_prompt_version
            .eq(framegen_core::update::SELF_VERSION);
    Ok(json!({ "games": games, "prompt": prompt }))
}

/// 记录「DLSS5 更换提示」已经按当前版本弹过了。
fn dlss5_legacy_prompt_done() -> Result<Value, String> {
    let mut cfg = framegen_core::util::load_config();
    cfg.dlss5_legacy_prompt_version = framegen_core::update::SELF_VERSION.to_owned();
    framegen_core::util::save_config(&cfg).map_err(|e| format!("{e:#}"))?;
    Ok(json!({ "ok": true }))
}

/// 扫一遍游戏库：三类资产里哪些游戏的部署文件**旧于当前内置**（按文件指纹判定）。
/// 统一提示（assets_update_scan）用它一次性拿三类结果。
fn assets_update_scan() -> Result<Value, String> {
    let cache = framegen_core::scan::load_library();
    let mut fg: Vec<String> = Vec::new();
    let mut sm: Vec<String> = Vec::new();
    let mut d5: Vec<String> = Vec::new();
    for row in cache.scanned.iter().chain(cache.manual.iter()) {
        let d = &row.entry.install_dir;
        let o = framegen_core::assets_audit::scan(d);
        if o.fg {
            fg.push(row.entry.name.clone());
        }
        if o.sm {
            sm.push(row.entry.name.clone());
        }
        if o.dlss5 {
            d5.push(row.entry.name.clone());
        }
    }
    let cfg = framegen_core::util::load_config();
    let mut any = false;
    if !fg.is_empty() { any = true; }
    if !sm.is_empty() { any = true; }
    if !d5.is_empty() { any = true; }
    let mut prompt = any;
    if cfg.assets_legacy_prompt_version == framegen_core::update::SELF_VERSION {
        prompt = false;
    }
    Ok(json!({ "fg": fg, "sm": sm, "dlss5": d5, "prompt": prompt }))
}

/// 记录「三类资产有更新」这个统一提示已经按当前版本弹过了。
fn assets_legacy_prompt_done() -> Result<Value, String> {
    let mut cfg = framegen_core::util::load_config();
    cfg.assets_legacy_prompt_version = framegen_core::update::SELF_VERSION.to_owned();
    framegen_core::util::save_config(&cfg).map_err(|e| format!("{e:#}"))?;
    Ok(json!({ "ok": true }))
}

/// 记录「旧版组件提示」已经按当前版本弹过了。
fn sm_legacy_prompt_done() -> Result<Value, String> {
    let mut cfg = framegen_core::util::load_config();
    cfg.sm_legacy_prompt_version = framegen_core::update::SELF_VERSION.to_owned();
    framegen_core::util::save_config(&cfg).map_err(|e| format!("{e:#}"))?;
    Ok(json!({ "ok": true }))
}

/// 保存帧生成的超分预设（0.4.1 起五选一）。
fn set_sr_mode(mode: String) -> Result<Value, String> {
    const OK: [&str; 5] = ["Off", "M-INT8", "L-INT8", "M-FP16", "L-FP16"];
    if !OK.iter().any(|x| x.eq_ignore_ascii_case(mode.trim())) {
        return Err(format!("不支持的超分预设：{mode}"));
    }
    let mut cfg = framegen_core::util::load_config();
    cfg.fg_sr_mode = mode.trim().to_owned();
    framegen_core::util::save_config(&cfg).map_err(|e| format!("{e:#}"))?;
    Ok(json!({ "srMode": cfg.fg_sr_mode }))
}

/// 保存「启动时自动扫描游戏库」开关（默认**关**）。
fn set_auto_scan(on: bool) -> Result<Value, String> {
    let mut cfg = framegen_core::util::load_config();
    cfg.auto_scan_on_start = on;
    framegen_core::util::save_config(&cfg).map_err(|e| format!("{e:#}"))?;
    Ok(json!({ "autoScanOnStart": on }))
}

/// 改部署档位：只接受上游真正支持的取值 —— 别让前端传进来的怪数字写进配置
/// （0~3 是已知四档；倍率上限只有 3=4X 和 5=6X 两种）
fn set_config(
    optimized: u8,
    frames: u8,
    preset: String,
    legacy_prompted: Option<String>,
) -> Result<Value, String> {
    if optimized > 3 {
        return Err(format!("优化等级只能是 0~3（收到 {optimized}）"));
    }
    if frames != framegen_core::update::DEFAULT_MAX_FRAMES && frames != 5 {
        return Err(format!(
            "倍率上限只能是 4X（{}）或 6X（5）（收到 {frames}）",
            framegen_core::update::DEFAULT_MAX_FRAMES
        ));
    }
    // 空串 = 没选过，按出厂默认处理
    if !preset.is_empty()
        && !framegen_core::update::PRESETS.contains(&preset.as_str())
    {
        return Err(format!(
            "DLSS-G 预设只支持 {}（收到 {preset}）",
            framegen_core::update::PRESETS.join(" / ")
        ));
    }
    let mut cfg = framegen_core::util::load_config();
    cfg.fg_optimized = optimized;
    cfg.fg_frames = frames;
    cfg.fg_preset = if preset.is_empty() {
        framegen_core::update::DEFAULT_PRESET.to_owned()
    } else {
        preset
    };
    // 可选：界面记下"已经提示过某个版本"，下次启动就不再弹
    if let Some(v) = legacy_prompted {
        cfg.legacy_prompt_version = v;
    }
    framegen_core::util::save_config(&cfg).map_err(|e| format!("{e:#}"))?;
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
    let cfg = framegen_core::util::load_config();
    if cfg.retired {
        return Ok(json!({
            "current": framegen_core::update::SELF_VERSION,
            "latest": null,
            "notes": null,
            "hasUpdate": false,
            "unpublished": false,
            "retired": true,
        }));
    }
    let client = framegen_core::update::client().map_err(|e| format!("{e:#}"))?;
    let current = framegen_core::update::SELF_VERSION.to_string();
    let latest = framegen_core::update::fetch_latest_self_version(&client);
    // 「有新版本」= 版本号更大 **且** 那个版本的安装包真的在 release 里。
    // 只看版本号会出现"发现新版本 1.0.6"但 release 里没有、点下载 404 的情况
    // （版本号是先于 release 提交到 main 的）。真实存在才提示。
    let newer = latest
        .as_deref()
        .map(|l| framegen_core::update::is_newer(l, &current))
        .unwrap_or(false);
    let has_update = newer
        && latest
            .as_deref()
            .map(|l| framegen_core::update::self_release_exists(&client, l))
            .unwrap_or(false);
    if newer && !has_update {
        framegen_core::log::line(&format!(
            "[自更新] main 上是 {}，但 release 里没有对应的安装包，先不提示",
            latest.as_deref().unwrap_or("?")
        ));
    }
    Ok(json!({
        "current": current,
        // 有新版本时顺便把更新日志取回来（raw 小文本：不占 API 配额、镜像也拿得到）
        "notes": if has_update { framegen_core::update::fetch_release_notes(&client) } else { None },
        "latest": latest,
        "hasUpdate": has_update,
        /* 代码里已经写了更大的版本号、但 release 里还没有安装包。
           单独给界面一个字段：设置页要把这种情况显示成"1.1.2（未发布）"，
           否则用户会看到"最新版本 1.1.2"配一句"已是最新"，读成"有更新但你们没发"（真实反馈）。 */
        "unpublished": newer && !has_update,
        "releasesUrl": framegen_core::update::RELEASES_URL,
    }))
}

/// 下载新版安装包并校验 SHA256（core 的 stage_self_update），**但不安装**。
///
/// 为什么拆成两步：安装必须由外壳（Electron 主进程）来做 —— 它要弹原生确认框、
/// 启动安装程序、然后退出自己。这里只负责「下载 + 校验」，校验不过绝不返回路径
/// （哈希取自发布时的 SHA256SUMS.txt，且清单只走官方源，不走镜像）。
fn self_update_download(id: u64, args: &Value) -> Result<Value, String> {
    if framegen_core::util::load_config().retired {
        return Err("本项目已停止更新维护，请到新项目 FrameGen Manager Next 下载。".to_owned());
    }
    let client = framegen_core::update::client().map_err(|e| format!("{e:#}"))?;
    // version 现在**不再是必填**：万一界面/旧调用方漏传，后台自己取最新版本。
    // 这样即便前端再出现「少传参数」，用户也不会看到「缺少参数 version」。
    let version = match arg_opt_str(args, "version")? {
        Some(v) if !v.trim().is_empty() => v,
        _ => framegen_core::update::fetch_latest_self_version(&client).ok_or_else(|| {
            "无法确定最新版本（网络不可用或还没有发布版本）。请先点「检查更新」，或到发布页手动下载安装包。".to_owned()
        })?,
    };
    // 用**共享**的取消标志（cancel_self_update 命令置位的就是它），每次开始前复位。
    let cancel = selfupdate_cancel();
    cancel.store(false, std::sync::atomic::Ordering::SeqCst);
    // 实时下载速度：每 0.5 秒用「字节增量 / 时间增量」算一次，直接拼进进度文案
    // （用户要求下载弹窗显示速度；不新增事件字段，界面无需改动即可看到）。
    let mut last_done = 0u64;
    let mut last_at = std::time::Instant::now();
    let mut speed_bps = 0f64;
    let staged = match framegen_core::update::stage_self_update(
        &client,
        &version,
        &cancel,
        &mut |done, total, msg| {
            let f = if total > 0 { done as f32 / total as f32 } else { 0.0 };
            let now = std::time::Instant::now();
            let dt = now.duration_since(last_at).as_secs_f64();
            if dt >= 0.5 && done >= last_done {
                speed_bps = (done - last_done) as f64 / dt;
                last_done = done;
                last_at = now;
            }
            let text = if speed_bps > 0.0 {
                format!("{msg} · {:.1} MB/s", speed_bps / 1048576.0)
            } else {
                msg.to_string()
            };
            progress(id, &text, f);
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

const RETIRED_MSG: &str = "本项目已停止更新维护，请转移至新项目 FrameGen Manager Next（https://github.com/XiaoQAQ10086/FrameGen-Manager-Next）完成部署。";

/// 停更后仍然会往游戏里写东西的命令：统一挡住，杜绝用户又部署回来。
fn is_mutation_cmd(cmd: &str) -> bool {
    matches!(
        cmd,
        "deploy_game"
            | "clean_legacy"
            | "framegen_cleanup_outdated"
            | "sm_set_game"
            | "sm_asset_deploy"
            | "dlss5_deploy"
            | "gpu_spoof_apply"
    )
}

/// 命令名 -> 具体实现。表里的每个名字都和 apps/desktop/src/main.rs 里
/// tauri::generate_handler! 的那一份对应，一个不多一个不少
/// （外加一个 ping：Electron 用它做健康检查，Tauri 版不需要探活）。
fn dispatch(id: u64, cmd: &str, args: &Value) -> Result<Value, String> {
    if is_mutation_cmd(cmd) {
        let c = framegen_core::util::load_config();
        if c.retired || c.retire_pending {
            return Err(RETIRED_MSG.to_owned());
        }
    }
    match cmd {
        // 健康检查：进程起来了、stdin/stdout 通了
        "ping" => Ok(Value::String("pong".into())),

        // force=true 时穿透缓存（界面上的手动刷新用得到）
        "gpu_info" => gpu_info(
            args.get("force").and_then(|v| v.as_bool()).unwrap_or(false),
            arg_opt_str(args, "fakeName")?,
        ),
        "covers" => Ok(Value::Array(covers())),
        "assets_status" => Ok(assets_status()),
        "assets_repair" => assets_repair(),
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
        "sm_info" => sm_info(args),
        "sm_set_game" => sm_set_game(args),
        "sm_turn_off_game" => sm_turn_off_game(args),
        "sm_game_states" => sm_game_states(args),
        "sm_game_state" => sm_game_state(args),
        "sm_profile_of" => sm_profile_of(args),
        "sm_asset_deploy" => sm_asset_deploy(args),
        "sm_asset_restore" => sm_asset_restore(args),
        "sm_component_check" => sm_component_check(args),
        "backups_info" => Ok(backups_info()),
        "clean_backups" => clean_backups(),
        "log_tail" => Ok(Value::String(log_tail(arg_usize(args, "lines")?))),
        "diagnostics_export" => diagnostics_export(),
        "open_path" => {
            open_path(arg_str(args, "path")?)?;
            Ok(Value::Null)
        }
        "list_games" => list_games(),
        "scan_library" => scan_library(id, &arg_opt_str(args, "mode")?.unwrap_or_else(|| "manual".to_owned())),
        "add_game" => add_game(),
        "pick_game_exe" => pick_game_exe(),
        // 界面走的这三条：主进程弹的框（带父窗口，不会被窗口盖住）
        "add_game_at" => add_game_at(arg_str(args, "dir")?),
        "pick_game_exe_at" => pick_game_exe_at(arg_str(args, "exe")?),

        "game_icons" => game_icons(),
        "open_hags_settings" => open_hags_settings(),
        // Electron 主进程用：只拿 URI（由主进程 shell.openExternal 打开，设置页才在最前）
        "hags_settings_uri" => Ok(json!({ "uri": framegen_core::gpu::hags_settings_uri() })),
        "remove_game" => remove_game(arg_str(args, "dir")?),
        "restore_removed" => restore_removed(arg_str(args, "key")?),

        "clean_legacy" => clean_legacy(arg_str(args, "dir")?),
        "proxy_advice" => proxy_advice(arg_str(args, "dir")?),
        "framegen_cleanup_outdated" => framegen_cleanup_outdated(),
        "deploy_game" => {
            if let Some(m) = arg_opt_str(args, "srMode")? {
                save_sr_mode(&m)?;
            }
            deploy_game(
            arg_str(args, "dir")?,
            arg_str(args, "proxy")?,
            arg_u8(args, "optimized")?,
            arg_u8(args, "frames")?,
            arg_opt_str(args, "preset")?,
            args.get("sr").and_then(Value::as_bool).unwrap_or(false),
            arg_opt_str(args, "engine")?,
            arg_opt_str(args, "flow")?,
            args.get("hudless").and_then(Value::as_str).map(str::to_owned),
            )
        }
        "restore" => restore(arg_str(args, "dir")?),

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
        "dlss5_deploy" => dlss5_deploy(id, args),
        "dlss5_restore" => dlss5_restore(arg_str(args, "dir")?),
        "dlss5_detect" => dlss5_detect(arg_str(args, "dir")?),
        "dlss5_uninstall" => dlss5_uninstall(id, args),


        "get_config" => Ok(get_config()),
        "retire_cleanup" => retire_cleanup(),
        "retire_announce_seen" => retire_announce_seen(),
        "set_config" => set_config(
            arg_u8(args, "optimized")?,
            arg_u8(args, "frames")?,
            arg_opt_str(args, "preset")?.unwrap_or_default(),
            arg_opt_str(args, "legacyPrompted")?,
        ),
        "set_sr_mode" => set_sr_mode(arg_str(args, "mode").unwrap_or_default()),
        "set_auto_scan" => set_auto_scan(args.get("on").and_then(Value::as_bool).unwrap_or(false)),
        "sm_legacy_scan" => sm_legacy_scan(),
        "sm_legacy_prompt_done" => sm_legacy_prompt_done(),
        "assets_legacy_prompt_done" => assets_legacy_prompt_done(),
        "assets_update_scan" => assets_update_scan(),
        "dlss5_legacy_scan" => dlss5_legacy_scan(),
        "dlss5_legacy_prompt_done" => dlss5_legacy_prompt_done(),
        "check_self_update" => check_self_update(),
        "self_update_download" => self_update_download(id, args),


        other => Err(format!("unknown command: {other}")),
    }
}

// ---------------------------------------------------------------- 主循环

/// 处理一行请求。**任何情况都不许把进程带走**：
/// 不是合法 JSON、缺 cmd、参数不对、命令内部 panic —— 全都变成一行 error 回过去，
/// 然后接着读下一行。用户界面上点错东西顶多弹个提示，不该整个后台进程消失。
/// 命令参数的摘要：只记 key 和短值，数组/对象只记规模，避免把几百个 pids 写进日志。
fn args_summary(args: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(m) = args.as_object() {
        for (k, v) in m {
            let text = match v {
                Value::String(s) => {
                    let mut t: String = s.chars().take(160).collect();
                    if s.chars().count() > 160 {
                        t.push('…');
                    }
                    t
                }
                Value::Array(a) => format!("[{} 项]", a.len()),
                Value::Object(_) => "{…}".to_owned(),
                other => other.to_string(),
            };
            out.insert(k.clone(), Value::String(text));
        }
    }
    Value::Object(out)
}

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
    // 高频只读轮询（日志页每 10 秒一次）不写命令日志：否则 log_tail 自己会不停生成新行，
    // 日志永远在“变”，既费 I/O 又让界面看起来一直在刷新。失败仍然记。
    let quiet = matches!(cmd, "log_tail" | "ping");

    // 单条命令 panic 不许带走整个进程：Electron 那边还挂着一堆 pending 调用，
    // 进程一死它们全部失败。catch_unwind 之后回一条错误行，继续服务。
    let started = std::time::Instant::now();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(id, cmd, &args)));
    let ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(Ok(result)) => {
            if !quiet {
                framegen_core::log::line(&format!("[cmd:{cmd}] ok {ms}ms"));
                framegen_core::log::event(&json!({
                    "cmd": cmd,
                    "args": args_summary(&args),
                    "ok": true,
                    "ms": ms,
                    "code": "",
                }));
            }
            write_line(&ok_line(id, result))
        }
        Ok(Err(e)) => {
            // 诊断用：日志里留下“哪个命令 + 哪个错误码”，用户复制日志即可定位
            let code = err_code(&e);
            framegen_core::log::line(&format!("[cmd:{cmd}] 失败 [{code}] {e}"));
            framegen_core::log::event(&json!({
                "cmd": cmd,
                "args": args_summary(&args),
                "ok": false,
                "ms": ms,
                "code": code,
                "error": e,
            }));
            write_line(&err_line(id, &e))
        }
        Err(_) => {
            framegen_core::log::line(&format!("[cmd:{cmd}] panic（已捕获）"));
            framegen_core::log::event(&json!({
                "cmd": cmd,
                "args": args_summary(&args),
                "ok": false,
                "ms": ms,
                "code": "PANIC",
            }));
            write_line(&err_line(
                id,
                &format!("命令 {cmd} 内部异常（已捕获，进程继续）"),
            ))
        }
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
    .map_err(|e| format!("{e:#}"))
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
            framegen_core::gpu::run_elevated(&me, op).map_err(|e2| {
                // 提权后还是同一个原因（例如型号不在允许名单里）就别说两遍
                let (a, b) = (format!("{e:#}"), format!("{e2:#}"));
                if a == b {
                    a
                } else {
                    format!("{a}；提权重试也没成功：{b}")
                }
            })
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
            match deploy_impl(dir, "version.dll", 1, framegen_core::update::DEFAULT_MAX_FRAMES, "", false, "DLSS", "Middle", "Auto") {
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

#[cfg(test)]
mod err_code_tests {
    use super::err_code;

    /// 错误码是给用户报障用的，必须稳定：这几条是真实会出现的中文消息。
    #[test]
    fn codes_are_stable() {
        assert_eq!(err_code("帧生成需要 580.0 以上驱动"), "DRV-001");
        assert_eq!(err_code("文件正被占用，无法写入"), "FILE-002");
        assert_eq!(err_code("没有权限写入该目录"), "ACL-003");
        assert_eq!(err_code("内置组件不存在"), "AST-004");
        assert_eq!(err_code("没有找到本工具对该目录的部署记录"), "REC-005");
        assert_eq!(err_code("unknown command: nope"), "IPC-009");
        assert_eq!(err_code("其它问题"), "GEN-999");
    }
}
