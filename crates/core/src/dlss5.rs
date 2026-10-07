//! DLSS5 接入（实验性）—— ReShade(Addon 版) + RenoDX DLSS5 插件 + 神经网络模型。
//!
//! 为什么单独一个模块、而不是塞进 deploy.rs：
//!   * 要放进去的东西跟帧生成**完全不同**：ReShade 的代理模块（dxgi/d3d11/d3d12/…）、
//!     ReShade.ini / ReShade.log、renodx-dlss5-*.addon64、nvngx_dlssnr.dll；
//!   * 帧生成那套的统一入口 deploy::deploy() 有一条「代理入口位置上的文件必须是本项目
//!     签名的」硬规则 —— ReShade 恰恰不是本项目签名的，混在一起只会互相打架；
//!   * **备份记录必须分开**：帧生成的「还原」收尾会把整个备份目录删掉
//!     （deploy.rs 的 restore_with），共用一份记录会让另一边的还原凭空失效。
//!     所以这里用 <备份目录>\<目录 key>-dlss5\manifest.json 这份独立记录。
//!
//! 布局（都落在**渲染 EXE 所在目录**，也就是游戏的部署目标）：
//!   dxgi.dll 等              ReShade 本体（由官方 Setup 安装，我们不自己解包铺文件）
//!   ReShade.ini / .log       ReShade 自己写的配置与日志（我们只做备份/还原）
//!   renodx-dlss5-*.addon64   DLSS5 插件：ReShade 会扫「本体所在目录」里的 *.addon64
//!   nvngx_dlssnr.dll         神经网络模型：插件要求「放在插件或游戏 exe 旁边」
//!
//! ReShade 安装器的命令行是从上游源码 setup/MainWindow.xaml.cs 里确认的（不是猜的）：
//!   ReShade_Setup.exe --headless --api <d3d9|d3d10|d3d11|d3d12|dxgi|opengl|vulkan> [--state update] "<游戏 exe>"
//!   * --headless：隐藏窗口；成功/失败都会把结果**打印到 stdout 并以 0/1 退出**
//!   * ReShade 本体是**内嵌在 Setup exe 尾部的一个 zip** 里的（离线可用），
//!     我们额外把 ReShade64.dll 解出来，只为了「装完到底成没成」能按哈希核对
//!   * --api 决定它占哪个代理文件名（dxgi -> dxgi.dll、d3d12 -> d3d12.dll …）
//!   * Vulkan 不一样：装到 %ProgramData%\ReShade 并注册隐式层，要 UAC，会弹安装窗口

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::deploy::DirLock;
use crate::scan::{self, GraphicsApi, GpuRoute};
use crate::util;

// ---------------------------------------------------------------- 常量

/// 资产目录下的子目录（用户要求：不和帧生成资产混在同一层）
pub const DIR_NAME: &str = "DLSS5";
/// 目前只有这一个渲染后端。列表将来变长时，界面直接读这里的常量。
pub const BACKEND_RESHADE: &str = "reshade";
pub const MODEL_NAME: &str = "nvngx_dlssnr.dll";
pub const ADDON_PREFIX: &str = "renodx-dlss5";
pub const ADDON_EXT: &str = ".addon64";
pub const SETUP_PREFIX: &str = "ReShade_Setup_";
pub const RE_INI: &str = "ReShade.ini";
pub const RE_LOG: &str = "ReShade.log";
pub const RE_PRESET: &str = "ReShadePreset.ini";
/// ReShade 的效果包目录（用户若在覆盖层里装过效果，就在这个目录里）
pub const SHADERS_DIR: &str = "reshade-shaders";
/// 20/30 系模型在资产目录里的子目录名（只内置了这一份，40/50 系已放弃）
pub const MODEL_SUBDIR: &str = "20和30系显卡";
/// Vulkan 全局层的落点：%ProgramData%\ReShade
pub const COMMON_DIR: &str = "ReShade";
/// 安装器最长等多久（要拉兼容性表、可能还要在 UAC 上等人点确认）
const SETUP_TIMEOUT: Duration = Duration::from_secs(240);

/// 所有支持的图形 API（界面下拉就照这个列表建，别在 JS 里再抄一份）。
pub const API_CHOICES: [(GraphicsApi, &str); 6] = [
    (GraphicsApi::Dx9, "DX9"),
    (GraphicsApi::Dx10, "DX10"),
    (GraphicsApi::Dx11, "DX11"),
    (GraphicsApi::Dx12, "DX12"),
    (GraphicsApi::Vulkan, "Vulkan"),
    (GraphicsApi::OpenGl, "OpenGL"),
];

/// 界面下拉的「自动」项用的 key（不是 GraphicsApi 的值，所以单独给一个）
pub const API_AUTO: &str = "auto";

/// 下拉里的 key -> GraphicsApi。认不出来就是 None（界面传了脏值时要能报错，不能瞎猜）。
pub fn api_from_key(key: &str) -> Option<GraphicsApi> {
    match key.trim().to_ascii_lowercase().as_str() {
        "" | API_AUTO => Some(GraphicsApi::Unknown),
        "dx9" | "d3d9" => Some(GraphicsApi::Dx9),
        "dx10" | "d3d10" => Some(GraphicsApi::Dx10),
        "dx11" | "d3d11" => Some(GraphicsApi::Dx11),
        "dx12" | "d3d12" => Some(GraphicsApi::Dx12),
        "vulkan" | "vk" => Some(GraphicsApi::Vulkan),
        "opengl" | "gl" | "opengl32" => Some(GraphicsApi::OpenGl),
        _ => None,
    }
}

/// GraphicsApi -> ReShade 的 --api 参数。None = 认不出来。
pub fn api_arg(api: GraphicsApi) -> Option<&'static str> {
    match api {
        GraphicsApi::Dx9 => Some("d3d9"),
        GraphicsApi::Dx10 => Some("d3d10"),
        GraphicsApi::Dx11 => Some("d3d11"),
        GraphicsApi::Dx12 => Some("d3d12"),
        GraphicsApi::Vulkan => Some("vulkan"),
        GraphicsApi::OpenGl => Some("opengl"),
        GraphicsApi::Unknown => None,
    }
}

/// --api 参数 -> 它要占用的代理文件名。Vulkan 没有代理文件（走全局层）。
pub fn module_of(arg: &str) -> Option<&'static str> {
    match arg {
        "d3d9" => Some("d3d9.dll"),
        "d3d10" => Some("d3d10.dll"),
        "d3d11" => Some("d3d11.dll"),
        "d3d12" => Some("d3d12.dll"),
        "dxgi" => Some("dxgi.dll"),
        "opengl" => Some("opengl32.dll"),
        _ => None,
    }
}

// ---------------------------------------------------------------- 资产

pub fn asset_dir() -> Result<PathBuf> {
    Ok(util::assets_dir()?.join(DIR_NAME))
}

/// 资产目录里「按前缀+后缀找最新的一个」。版本号写在文件名里（ReShade_Setup_6.8.0_Addon.exe），
/// 所以按里面的数字比大小，别按字典序 —— 6.10 会排在 6.9 前面。
fn newest_with(dir: &Path, prefix: &str, suffix: &str) -> Option<PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(u32, u32, u32, String, PathBuf)> = None;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let lower = name.to_ascii_lowercase();
        if !lower.starts_with(&prefix.to_ascii_lowercase())
            || !lower.ends_with(&suffix.to_ascii_lowercase())
        {
            continue;
        }
        let v = version_key(&name);
        let better = match &best {
            None => true,
            Some((a, b, c, n, _)) => v > (*a, *b, *c) || (v == (*a, *b, *c) && name > *n),
        };
        if better {
            best = Some((v.0, v.1, v.2, name, e.path()));
        }
    }
    best.map(|(_, _, _, _, p)| p)
}

/// 从文件名里取第一个 x.y.z（缺的位补 0）。
fn version_key(name: &str) -> (u32, u32, u32) {
    let chars: Vec<char> = name.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            let mut parts: Vec<u32> = Vec::new();
            let mut cur = String::new();
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                if chars[i] == '.' {
                    parts.push(cur.parse().unwrap_or(0));
                    cur.clear();
                    if parts.len() == 3 {
                        break;
                    }
                } else {
                    cur.push(chars[i]);
                }
                i += 1;
            }
            if !cur.is_empty() && parts.len() < 3 {
                parts.push(cur.parse().unwrap_or(0));
            }
            if parts.len() >= 2 {
                let g = |k: usize| parts.get(k).copied().unwrap_or(0);
                return (g(0), g(1), g(2));
            }
            i = start + 1;
            continue;
        }
        i += 1;
    }
    (0, 0, 0)
}

pub fn find_setup() -> Option<PathBuf> {
    let dir = asset_dir().ok()?;
    newest_with(&dir, SETUP_PREFIX, ".exe")
}

pub fn find_addon() -> Option<PathBuf> {
    let dir = asset_dir().ok()?;
    newest_with(&dir, ADDON_PREFIX, ADDON_EXT)
}

/// 神经网络模型：先看按系列分的子目录，再退回资产目录根（用户自己导入的包就摊在根上）。
pub fn find_model() -> Option<PathBuf> {
    let dir = asset_dir().ok()?;
    let sub = dir.join(MODEL_SUBDIR).join(MODEL_NAME);
    if sub.is_file() {
        return Some(sub);
    }
    let flat = dir.join(MODEL_NAME);
    if flat.is_file() {
        return Some(flat);
    }
    None
}

#[derive(Debug, Clone, Serialize)]
pub struct AssetItem {
    pub name: String,
    pub present: bool,
    pub bytes: u64,
    pub hint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssetsReport {
    pub dir: String,
    pub items: Vec<AssetItem>,
    /// 三样齐了才谈得上部署
    pub ready: bool,
    pub missing: Vec<String>,
}

/// DLSS5 三件套的到位情况（DLSS5 页与部署对话框都用这一份）。
pub fn assets_report() -> AssetsReport {
    let dir = asset_dir().unwrap_or_default();
    let mut items = Vec::new();
    let mut missing = Vec::new();

    let size_of = |p: &Option<PathBuf>| -> u64 {
        p.as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0)
    };
    let name_of = |p: &Option<PathBuf>, fallback: &str| -> String {
        p.as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| fallback.to_owned())
    };

    let setup = find_setup();
    items.push(AssetItem {
        name: name_of(&setup, &format!("{SETUP_PREFIX}*.exe")),
        present: setup.is_some(),
        bytes: size_of(&setup),
        hint: "ReShade（Addon 版）安装器".to_owned(),
    });
    if setup.is_none() {
        missing.push("ReShade 安装器".to_owned());
    }

    let addon = find_addon();
    items.push(AssetItem {
        name: name_of(&addon, &format!("{ADDON_PREFIX}*{ADDON_EXT}")),
        present: addon.is_some(),
        bytes: size_of(&addon),
        hint: "DLSS5 插件（ReShade addon）".to_owned(),
    });
    if addon.is_none() {
        missing.push("DLSS5 插件".to_owned());
    }

    let model = find_model();
    items.push(AssetItem {
        name: MODEL_NAME.to_owned(),
        present: model.is_some(),
        bytes: size_of(&model),
        hint: format!("神经网络模型（{MODEL_SUBDIR}）"),
    });
    if model.is_none() {
        missing.push("神经网络模型".to_owned());
    }

    AssetsReport {
        dir: dir.display().to_string(),
        ready: missing.is_empty(),
        missing,
        items,
    }
}

// ---------------------------------------------------------------- 显卡 -> 模型

/// 这块卡该配哪份模型。**只认 20/30 系**：40/50 系的模型用户明确决定不做
/// （内置资产里没有），其它卡（无 Tensor Core / 非 N 卡）本来也跑不起来。
pub fn model_supported(route: GpuRoute) -> bool {
    matches!(route, GpuRoute::Sm86 | GpuRoute::Sm75)
}

fn resolve_model(route: GpuRoute, gpu: &str) -> Result<PathBuf> {
    if !model_supported(route) {
        let why = match route {
            GpuRoute::NotNeeded => {
                "DLSS5 的神经网络模型只做了 20/30 系（RTX 40/50 系那份没有内置，本工具暂不支持）"
                    .to_owned()
            }
            other => other
                .gate()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("没能识别这块显卡（{gpu}），不敢乱铺模型")),
        };
        bail!("不能部署 DLSS5：{why}");
    }
    find_model().with_context(|| {
        format!(
            "资产目录里没有 {}（{}）—— 重新安装本程序会自动补齐内置资产，或在 DLSS5 页点「获取资源包」",
            MODEL_NAME,
            asset_dir().map(|p| p.display().to_string()).unwrap_or_default()
        )
    })
}

// ---------------------------------------------------------------- API 计划

/// 本次要按哪个 --api 装、会占哪个代理文件。
#[derive(Debug, Clone)]
pub struct ApiPlan {
    /// 解析出来的图形 API（自动模式 = 检测结果）
    pub api: GraphicsApi,
    /// 传给 ReShade Setup 的 --api 值
    pub arg: &'static str,
    /// 会占用的代理文件名（Vulkan = None，走全局层）
    pub module: Option<&'static str>,
    /// 界面上显示的 API 名
    pub label: &'static str,
    /// 给用户看的说明（自动判定 / 认不出来时的兜底）
    pub note: Option<String>,
}

/// 选定的 API -> 安装计划。
///
/// **自动与明确选择走的是两条路**（这是有意的）：
///   * 自动：跟着 core 检出的 API 走，DX10/11/12 都用 **dxgi.dll** —— 上游自己的分析器
///     对这三种也是选 DXGI；
///   * 明确选择：用那个 API **自己的**代理文件（DX12 -> d3d12.dll、DX11 -> d3d11.dll…），
///     这样 dxgi.dll 被别的 Mod（DXVK、帧生成代理）占了时，用户还有能用的选项。
pub fn plan_api(sel: GraphicsApi, detected: GraphicsApi) -> ApiPlan {
    if sel != GraphicsApi::Unknown {
        let arg = api_arg(sel).unwrap_or("dxgi");
        return ApiPlan {
            api: sel,
            arg,
            module: module_of(arg),
            label: sel.label(),
            note: None,
        };
    }
    // 「自动」= 跟着 core 检出的 API 走，DX10/11/12 统一用 dxgi（上游分析器的选择）。
    // 检出 Unknown 的情况在 prepare() 里就被拦下了，走不到这（这里的兜底只为防御）。
    let arg = match detected {
        GraphicsApi::Dx10 | GraphicsApi::Dx11 | GraphicsApi::Dx12 => "dxgi",
        other => api_arg(other).unwrap_or("dxgi"),
    };
    ApiPlan {
        api: detected,
        arg,
        module: module_of(arg),
        label: detected.label(),
        note: None,
    }
}

// ---------------------------------------------------------------- 记录（manifest）

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// 相对部署目录的文件名
    pub rel_path: String,
    /// 人话说明这一条是什么（界面/日志里直接显示）
    pub role: String,
    pub existed_before: bool,
    pub backup_name: Option<String>,
    pub original_sha256: Option<String>,
    /// 我们写进去以后的内容哈希。**可能为空** —— 安装中途失败时还没来得及算。
    pub deployed_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub game_key: String,
    pub game_dir: PathBuf,
    pub exe: String,
    /// 实际用的 --api 值（dxgi / d3d12 / vulkan …）
    pub api: String,
    /// 占用的代理文件名（Vulkan = None）
    pub module: Option<String>,
    pub backend: String,
    pub gpu: String,
    pub deployed_at: String,
    pub files: Vec<Entry>,
}

/// 备份目录：和帧生成那份**分开**（同一目录 key 加后缀），共用会让两边互相删记录。
pub fn base_dir(deploy_dir: &Path) -> Result<PathBuf> {
    Ok(util::backups_dir()?.join(format!("{}-dlss5", util::dir_key(deploy_dir))))
}

pub fn manifest_path(deploy_dir: &Path) -> Result<PathBuf> {
    Ok(base_dir(deploy_dir)?.join("manifest.json"))
}

pub fn load_manifest(deploy_dir: &Path) -> Option<Manifest> {
    let p = manifest_path(deploy_dir).ok()?;
    let t = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&t).ok()
}

/// 「记录坏了」和「压根没部署过」必须分开：前者若当成后者，下次部署会把我们自己
/// 放进去的文件当成游戏原件备份，真正的原件就永久丢了（deploy.rs 里同样的教训）。
pub enum State {
    NotDeployed,
    Deployed(Box<Manifest>),
    Broken(String),
}

pub fn state_of(deploy_dir: &Path) -> State {
    let Ok(p) = manifest_path(deploy_dir) else {
        return State::NotDeployed;
    };
    if !p.is_file() {
        return State::NotDeployed;
    }
    match std::fs::read_to_string(&p) {
        Ok(t) => match serde_json::from_str::<Manifest>(&t) {
            Ok(m) => State::Deployed(Box::new(m)),
            Err(e) => State::Broken(e.to_string()),
        },
        Err(e) => State::Broken(e.to_string()),
    }
}

/// 界面上那一行状态文案（和帧生成的 DeployState::label 风格保持一致）。
pub fn state_label(deploy_dir: &Path) -> String {
    match state_of(deploy_dir) {
        State::NotDeployed => "未部署".to_owned(),
        State::Deployed(m) => {
            let module = m
                .module
                .clone()
                .unwrap_or_else(|| format!("Vulkan 全局层（{}）", m.api));
            format!("已部署 DLSS5（{module} + {}）", m.backend)
        }
        State::Broken(e) => format!("部署记录读不出来：{e}"),
    }
}

fn write_manifest(base: &Path, m: &Manifest) -> Result<()> {
    std::fs::create_dir_all(base)?;
    let tmp = base.join("manifest.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(m)?)?;
    util::atomic_replace(&tmp, &base.join("manifest.json"))
}

// ---------------------------------------------------------------- 小工具

/// PE 里读位数：DLSS5 插件只有 64 位的（*.addon64），32 位游戏直接说清楚。
fn pe_is_64bit(path: &Path) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    let mut dos = [0u8; 64];
    f.read_exact(&mut dos).context("读 DOS 头失败")?;
    if &dos[0..2] != b"MZ" {
        bail!("{} 不是有效的 PE 文件", path.display());
    }
    let off = u32::from_le_bytes([dos[0x3c], dos[0x3d], dos[0x3e], dos[0x3f]]) as u64;
    f.seek(SeekFrom::Start(off))?;
    let mut hdr = [0u8; 6];
    f.read_exact(&mut hdr).context("读 PE 头失败")?;
    if &hdr[0..4] != b"PE\0\0" {
        bail!("{} 的 PE 头不对", path.display());
    }
    let machine = u16::from_le_bytes([hdr[4], hdr[5]]);
    Ok(machine == 0x8664)
}

/// 读一个文件版本资源里的 ProductName（ReShade 本体写的是 "ReShade"）。
///
/// **为什么非要有它**：ReShade 安装器只挡「ProductName 非空、且不是 ReShade」的占用，
/// 而本项目的帧生成代理**根本没有版本资源**（ProductName 为空）—— 实测它会连问都不问
/// 就把那份 30 MB 的代理覆盖成 ReShade（备份还在，但静默覆盖不能接受）。
/// 所以「这个位置到底能不能装」这一道必须我们自己把住。
fn product_name(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut handle = 0u32;
    let size = unsafe { GetFileVersionInfoSizeW(wide.as_ptr(), &mut handle) };
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let ok = unsafe {
        GetFileVersionInfoW(wide.as_ptr(), 0, size, buf.as_mut_ptr() as *mut core::ffi::c_void)
    };
    if ok == 0 {
        return None;
    }
    // 取一段版本资源。is_text=true 时长度单位是**字符数**（UTF-16），必须乘 2 ——
    // 这里踩过一次：不乘 2 只读到一半，ProductName 变成 "ReSh"，于是
    // 「这个位置是不是 ReShade」判错、重装被自己的预检挡住。
    let query = |sub: &str, is_text: bool| -> Option<Vec<u8>> {
        let w: Vec<u16> = sub.encode_utf16().chain(std::iter::once(0)).collect();
        let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let ok = unsafe { VerQueryValueW(buf.as_ptr() as *const _, w.as_ptr(), &mut p, &mut len) };
        if ok == 0 || p.is_null() || len == 0 {
            return None;
        }
        let n = if is_text { len as usize * 2 } else { len as usize };
        Some(unsafe { std::slice::from_raw_parts(p as *const u8, n) }.to_vec())
    };
    // 语言/代码页：先问 \VarFileInfo\Translation，问不到就按常见的中英文试一遍
    let mut langs: Vec<(u16, u16)> = Vec::new();
    if let Some(t) = query("\\VarFileInfo\\Translation", false) {
        for c in t.as_chunks::<4>().0 {
            langs.push((
                u16::from_le_bytes([c[0], c[1]]),
                u16::from_le_bytes([c[2], c[3]]),
            ));
        }
    }
    langs.push((0x0409, 0x04B0));
    langs.push((0x0804, 0x04B0));
    for (lang, cp) in langs {
        let sub = format!("\\StringFileInfo\\{lang:04x}{cp:04x}\\ProductName");
        if let Some(b) = query(&sub, true) {
            let u: Vec<u16> = b
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            let s = String::from_utf16_lossy(&u)
                .trim_end_matches('\0')
                .trim()
                .to_owned();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// 目标位置现在的状态。ReShade 安装器只认它自己那套判据，这里再补一道我们的。
enum SlotState {
    /// 空着
    Free,
    /// 已经是 ReShade（我们装的或用户自己装的）—— 可以按「更新」装上去
    Reshade,
    /// 别的文件占着 —— 绝不覆盖
    Foreign(String),
}

fn slot_state(dir: &Path, module: &str) -> SlotState {
    let p = dir.join(module);
    if !p.is_file() {
        return SlotState::Free;
    }
    if let Some(n) = product_name(&p) {
        // 前缀匹配而不是全等：ReShade 本体的 ProductName 就是 "ReShade"，
        // 以后万一带了后缀（"ReShade 6.9"）也不该判成「别人的文件」。
        if n.trim().to_ascii_lowercase().starts_with("reshade") {
            return SlotState::Reshade;
        }
    }
    SlotState::Foreign(describe_occupant(&p))
}

/// 目录里那个位置上现在是什么东西（用来把「拒绝覆盖」的理由说清楚）。
fn describe_occupant(path: &Path) -> String {
    if !path.is_file() {
        return "（不存在）".to_owned();
    }
    let n = util::format_bytes(std::fs::metadata(path).map(|m| m.len()).unwrap_or(0));
    if scan::identify_dll(path).is_ours() {
        return format!("本项目的帧生成代理入口（{n}）");
    }
    match product_name(path).filter(|s| !s.is_empty()) {
        Some(p) => format!("另一个程序的文件（{n}，产品名「{p}」）"),
        None => format!("另一个没有版本信息的文件（{n}）"),
    }
}

/// 把 Setup exe 尾部内嵌的 zip 解出一个 ReShade 本体。
///
/// 上游是这么找的：每 512 字节看开头是不是 PK\x03\x04、且紧跟的 26 字节不全为 0
/// （PE 里偶尔也会出现 PK 字样的数据，所以还得真能当 zip 读出来才算数）。
/// 解这一份**只为了事后核对**：装进去的到底是不是它该装的那份。
fn extract_reshade_dll(setup: &Path, is64: bool, out: &Path) -> Result<()> {
    let bytes = std::fs::read(setup).with_context(|| format!("读取 {}", setup.display()))?;
    let want = if is64 { "ReShade64.dll" } else { "ReShade32.dll" };
    let tmp_zip = std::env::temp_dir().join(format!(
        "fgm-reshade-embed-{}-{}.zip",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let mut found = false;
    let mut i = 0usize;
    while i + 30 <= bytes.len() {
        if &bytes[i..i + 4] == b"PK\x03\x04" && bytes[i + 4..i + 30].iter().any(|b| *b != 0) {
            std::fs::write(&tmp_zip, &bytes[i..])?;
            if let Ok(list) = crate::update::zip_list(&tmp_zip) {
                if list.iter().any(|m| {
                    m.name
                        .rsplit(['/', '\\'])
                        .next()
                        .map(|n| n.eq_ignore_ascii_case(want))
                        .unwrap_or(false)
                }) {
                    found = true;
                    break;
                }
            }
        }
        i += 512;
    }
    if !found {
        let _ = std::fs::remove_file(&tmp_zip);
        bail!("安装器里没有找到内嵌的 {want}（{}）", setup.display());
    }
    let r = crate::update::zip_extract_dll(&tmp_zip, out, want);
    let _ = std::fs::remove_file(&tmp_zip);
    r.map(|_| ()).context("从安装器里解出 ReShade 本体失败")
}

/// 跑一次安装器。state = None（首次安装）/ Some("update") / Some("uninstall")。
/// 返回 (退出码, stdout+stderr, 是否超时)。
fn run_setup(setup: &Path, exe: &Path, api: &str, state: Option<&str>) -> Result<(i32, String, bool)> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut cmd = std::process::Command::new(setup);
    cmd.arg("--headless").arg("--api").arg(api);
    if let Some(s) = state {
        cmd.arg("--state").arg(s);
    }
    // exe 路径放最后：上游是按「这个参数是不是一个存在的文件」来认目标的
    cmd.arg(exe);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = cmd.spawn().with_context(|| format!("启动 {}", setup.display()))?;
    let deadline = Instant::now() + SETUP_TIMEOUT;
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(120));
            }
            Err(e) => bail!("等待安装器失败：{e}"),
        }
    }
    let out = child.wait_with_output().context("读取安装器输出失败")?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .trim()
    .to_owned();
    Ok((out.status.code().unwrap_or(-1), text, timed_out))
}

// ---------------------------------------------------------------- 部署

pub struct Prepared {
    pub dir: PathBuf,
    pub exe: PathBuf,
    pub plan: ApiPlan,
    pub setup: PathBuf,
    pub addon: PathBuf,
    pub model: PathBuf,
    pub gpu: String,
    pub notes: Vec<String>,
}

/// 部署前的全部检查与准备（纯计算，不动任何文件）。
pub fn prepare(dir: &Path, exe: Option<&Path>, api_sel: GraphicsApi) -> Result<Prepared> {
    if !dir.is_dir() {
        bail!("部署目录不存在：{}", dir.display());
    }
    let exe = match exe {
        Some(p) => {
            if !p.is_file() {
                bail!("指定的启动文件不存在：{}", p.display());
            }
            p.to_path_buf()
        }
        None => scan::find_render_exe(dir).with_context(|| {
            format!(
                "没能在 {} 里认出渲染 EXE —— 用「手动选择」指定游戏的启动 exe",
                dir.display()
            )
        })?,
    };
    // exe 必须就在部署目录里：DLSS5 的四个文件全都铺在 exe 旁边
    let parent = exe.parent().unwrap_or(dir);
    if scan::path_key(parent) != scan::path_key(dir) {
        bail!(
            "这个启动文件不在部署目录里：\n  exe：{}\n  目录：{}\n用「手动选择」重新指定，或先把游戏库里的目录改对",
            exe.display(),
            dir.display()
        );
    }
    if !pe_is_64bit(&exe)? {
        bail!(
            "{} 是 32 位程序，内置的 DLSS5 插件是 64 位版（*.addon64），暂不支持",
            exe.display()
        );
    }

    let setup = find_setup().with_context(|| {
        format!(
            "资产目录里没有 ReShade 安装器（{}）—— 重新安装本程序会自动补齐内置资产",
            asset_dir().map(|p| p.display().to_string()).unwrap_or_default()
        )
    })?;
    let addon = find_addon().context("资产目录里没有 DLSS5 插件（renodx-dlss5-*.addon64）")?;
    let gpu_raw = scan::detect_gpu().unwrap_or_default();
    let route = scan::classify_gpu(&gpu_raw);
    let model = resolve_model(route, &gpu_raw)?;

    // **「自动」档认不出图形 API 时不替用户猜**（用户要求）：装错代理等于白装一次，
    // 所以直接拒绝，并明确告诉他去上面手动选一个。界面上点「部署」就会看到这句。
    let detected = if api_sel == GraphicsApi::Unknown {
        scan::detect_tech(&exe).0
    } else {
        GraphicsApi::Unknown
    };
    if api_sel == GraphicsApi::Unknown && detected == GraphicsApi::Unknown {
        crate::log::line(&format!(
            "已阻止 DLSS5 部署：图形 API 自动档认不出（{}）",
            exe.display()
        ));
        bail!(
            "图形 API 选的是「自动」，但没能认出这个游戏用的是哪个 API。\n\
             请在「图形 API」里手动选一个（DX9 / DX10 / DX11 / DX12 / Vulkan / OpenGL）再部署 ——\
             装错代理等于白装一次。"
        );
    }
    let mut plan = plan_api(api_sel, detected);
    let mut notes = Vec::new();
    if let Some(n) = &plan.note {
        notes.push(n.clone());
    }

    // 代理位置预检。两条规矩：
    //   1) 位置被非 ReShade 的文件占着 -> 绝不装（安装器自己判不出来，见 product_name 的说明）；
    //   2) **自动模式**下改用它自己那个代理名再试一次（DX12 -> d3d12.dll …），
    //      明确选择时不动 —— 那是用户点的，装不上就把理由说清楚。
    if let Some(m) = plan.module {
        if let SlotState::Foreign(what) = slot_state(dir, m) {
            let alt = if api_sel == GraphicsApi::Unknown {
                api_arg(plan.api)
                    .filter(|a| *a != plan.arg)
                    .and_then(|a| module_of(a).map(|mm| (a, mm)))
                    .filter(|(_, mm)| !matches!(slot_state(dir, mm), SlotState::Foreign(_)))
            } else {
                None
            };
            match alt {
                Some((a, mm)) => {
                    notes.push(format!("{m} 已经被{what}占了，自动改用 {mm}"));
                    plan.arg = a;
                    plan.module = Some(mm);
                }
                None => bail!(
                    "这个位置（{m}）已经被别的文件占了：{what}。\nReShade 装不进去，也不会去盖它。换一个「图形 API」再试（例如 dxgi 被占就用 DX12 -> d3d12.dll），或者先还原占用它的那个 Mod。"
                ),
            }
        }
    }
    notes.push(format!(
        "图形 API：{}（--api {}）{}",
        plan.label,
        plan.arg,
        match plan.module {
            Some(m) => format!("，占用 {m}"),
            None => "，全局 Vulkan 层（需要 UAC 同意）".to_owned(),
        }
    ));

    Ok(Prepared {
        dir: dir.to_path_buf(),
        exe,
        plan,
        setup,
        addon,
        model,
        gpu: gpu_raw,
        notes,
    })
}

/// 部署：备份 -> 跑官方安装器 -> 铺插件与模型 -> 逐个核对 -> 写记录。
///
/// 任何一步失败都会尽量回滚（把备份放回去、删掉我们刚建的文件），并把原因原样带出去 ——
/// 不能让界面显示「已部署」而目录里其实是半截。
pub fn deploy(
    target_dir: &Path,
    target_exe: Option<&Path>,
    api_sel: GraphicsApi,
    backend: &str,
    on: &mut dyn FnMut(String, f32),
) -> Result<(String, Vec<String>)> {
    if backend != BACKEND_RESHADE {
        bail!("暂不支持渲染后端「{backend}」，目前只有 {BACKEND_RESHADE}");
    }
    let prep = prepare(target_dir, target_exe, api_sel)?;
    let Prepared {
        dir,
        exe,
        plan,
        setup,
        addon,
        model,
        gpu,
        mut notes,
    } = prep;

    // 记录读不出来就**不要**接着部署（同 deploy.rs 的理由：会把我们自己放进去的文件
    // 当成游戏原件备份，真正的原件就没了）
    let existing = match state_of(&dir) {
        State::NotDeployed => None,
        State::Deployed(m) => Some(*m),
        State::Broken(e) => bail!(
            "这个目录的 DLSS5 记录读不出来（{e}）。为免把原件当成本工具的文件覆盖掉，已停止部署：请把 {} 改名或删除后再试。",
            manifest_path(&dir)
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ),
    };

    let base = base_dir(&dir)?;
    let files_dir = base.join("files");
    std::fs::create_dir_all(&files_dir)?;
    let _lock = DirLock::acquire(&base)?;

    let module = plan.module.map(str::to_owned);
    // 本次要落的文件 + 安装器会自己创建的两个文件
    let mut want_files: Vec<(String, String)> = Vec::new();
    if let Some(m) = &module {
        want_files.push((m.clone(), "ReShade 本体（代理）".to_owned()));
    }
    want_files.push((RE_INI.to_owned(), "ReShade 配置".to_owned()));
    want_files.push((RE_LOG.to_owned(), "ReShade 日志".to_owned()));
    // 安装器还会顺手建一个空的预设文件（实测 0 字节）。**也要记上** ——
    // 不然「还原」删掉 ReShade 之后，它还会孤零零留在游戏目录里。
    want_files.push((RE_PRESET.to_owned(), "ReShade 预设（安装器建的占位文件）".to_owned()));
    let addon_name = addon
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .context("插件文件名读不出来")?;
    want_files.push((addon_name.clone(), "DLSS5 插件".to_owned()));
    want_files.push((MODEL_NAME.to_owned(), "神经网络模型".to_owned()));

    // 占用检查：正被游戏占着的文件写不进去，早点说清楚
    for (name, _) in &want_files {
        let p = dir.join(name);
        if p.exists() {
            match util::lock_state(&p) {
                util::LockState::InUse => bail!("{name} 正被占用，请先完全退出游戏。"),
                util::LockState::NoAccess => bail!(
                    "{name} 现在写不进去：文件带了只读属性，或者权限不足。去掉只读 / 换个可写的目录再试。"
                ),
                util::LockState::Free => {}
            }
        }
    }

    // 1) 备份 + 先落 manifest（后面任何失败都能按它回滚）
    let prev: Vec<Entry> = existing.as_ref().map(|m| m.files.clone()).unwrap_or_default();
    let mut entries: Vec<Entry> = Vec::new();
    for (name, role) in &want_files {
        // 上次部署过、备份还在 -> 沿用**第一次**的备份，别拿我们自己的文件当原件
        if let Some(p) = prev.iter().find(|e| &e.rel_path == name) {
            let backup_intact = match &p.backup_name {
                Some(bn) => files_dir.join(bn).is_file(),
                None => !p.existed_before,
            };
            if backup_intact {
                entries.push(Entry {
                    rel_path: name.clone(),
                    role: role.clone(),
                    existed_before: p.existed_before,
                    backup_name: p.backup_name.clone(),
                    original_sha256: p.original_sha256.clone(),
                    deployed_sha256: None,
                });
                continue;
            }
        }
        let dst = dir.join(name);
        let existed = dst.is_file();
        let mut backup_name = None;
        let mut original_sha256 = None;
        if existed {
            let orig = std::fs::read(&dst).with_context(|| format!("备份 {}", dst.display()))?;
            original_sha256 = Some(util::sha256_hex(&orig));
            let bn = format!("{}.bak", name);
            std::fs::write(files_dir.join(&bn), &orig)?;
            backup_name = Some(bn);
        }
        entries.push(Entry {
            rel_path: name.clone(),
            role: role.clone(),
            existed_before: existed,
            backup_name,
            original_sha256,
            deployed_sha256: None,
        });
    }
    // 上一次装过、这次不装的（比如换了图形 API，旧代理文件不铺了）：记录必须传下去 ——
    // 否则「还原」再也放不回那些原件（deploy.rs 里同样的规矩）。
    for p in &prev {
        if !entries.iter().any(|e| e.rel_path == p.rel_path) {
            entries.push(p.clone());
        }
    }
    // 同一个目录里留两个 DLSS5 插件会让插件自己「站下」（它日志里明说要留一个）：
    // 把旧的挑出来，装完再清。
    let stale_addons: Vec<String> = entries
        .iter()
        .filter(|e| {
            let n = e.rel_path.to_ascii_lowercase();
            n.starts_with(ADDON_PREFIX) && n.ends_with(ADDON_EXT) && e.rel_path != addon_name
        })
        .map(|e| e.rel_path.clone())
        .collect();

    let mut manifest = Manifest {
        game_key: util::dir_key(&dir),
        game_dir: dir.clone(),
        exe: exe.display().to_string(),
        api: plan.arg.to_owned(),
        module: module.clone(),
        backend: backend.to_owned(),
        gpu: gpu.clone(),
        deployed_at: util::now_utc(),
        files: entries.clone(),
    };
    write_manifest(&base, &manifest)?;

    // 2) 跑官方安装器
    on("正在安装 ReShade（Addon 版）…".to_owned(), 0.25);
    let update_mode = module
        .as_ref()
        .map(|m| dir.join(m).is_file())
        .unwrap_or(false);
    let mut run = run_setup(&setup, &exe, plan.arg, if update_mode { Some("update") } else { None })?;
    // 已经装过 ReShade（同 API）时，非 update 模式会被安装器自己挡下来，它明说了
    // 「已存在，请先卸载」；这时用官方给的 --state update 再来一次。
    if run.0 != 0 && looks_like_existing_reshade(&run.1) {
        on("目录里已有 ReShade，按「更新」方式再装一次…".to_owned(), 0.4);
        run = run_setup(&setup, &exe, plan.arg, Some("update"))?;
    }
    if run.2 {
        let rb = rollback_entries(&dir, &base, &entries);
        bail!(
            "ReShade 安装器超时（{} 秒没结束），已中止；回滚{}",
            SETUP_TIMEOUT.as_secs(),
            rb.map(|m| format!("：{m}"))
                .unwrap_or_else(|e| format!("失败：{e}"))
        );
    }
    if run.0 != 0 {
        let detail = friendly_setup_error(&run.1, &dir, &plan);
        let rb = rollback_entries(&dir, &base, &entries);
        bail!(
            "ReShade 安装失败：{detail}{}",
            rb.map(|m| format!("\n已回滚：{m}"))
                .unwrap_or_else(|e| format!("\n回滚失败：{e}（请点「还原」再试）"))
        );
    }
    on("ReShade 装好了，正在核对…".to_owned(), 0.55);

    // 3) 核对 ReShade 本体：解出内嵌的那份 ReShade64.dll 比哈希（比只看文件在不在可靠）
    let is64 = pe_is_64bit(&exe).unwrap_or(true);
    let tmp_dll = std::env::temp_dir().join(format!(
        "fgm-reshade-check-{}-{}.dll",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let expect = extract_reshade_dll(&setup, is64, &tmp_dll);
    let expect_sha = match &expect {
        Ok(()) => util::sha256_file(&tmp_dll).ok(),
        Err(_) => None,
    };
    let _ = std::fs::remove_file(&tmp_dll);

    let mut verify_problem: Option<String> = None;
    let mut verify_note = String::new();
    match (&module, &expect_sha) {
        (Some(m), Some(want)) => {
            let got = util::sha256_file(&dir.join(m)).ok();
            if got.as_deref() != Some(want.as_str()) {
                verify_problem = Some(format!(
                    "装完之后的 {m} 和安装器里内嵌的 ReShade 对不上（可能被别的程序又改了一次）"
                ));
            }
        }
        (None, _) => {
            // Vulkan：本体在全局层，不在游戏目录
            let common = vulkan_module_path(is64);
            if !common.is_file() {
                verify_problem = Some(format!(
                    "没有在 {} 找到 ReShade 本体 —— Vulkan 安装要 UAC 同意，弹窗被取消就是这种情况",
                    common.display()
                ));
            } else {
                notes.push(format!("Vulkan：ReShade 本体装在 {}", common.display()));
            }
        }
        (Some(_), None) => {
            // 解不出来只影响「核对」这一步，不影响安装本身：如实说明，不当成失败
            verify_note = "（没能从安装器里解出内嵌的 ReShade 做哈希核对，已跳过这一步）".to_owned();
        }
    }
    if let Some(why) = verify_problem {
        let rb = rollback_entries(&dir, &base, &entries);
        bail!(
            "ReShade 安装结果核对失败：{why}{}",
            rb.map(|m| format!("\n已回滚：{m}"))
                .unwrap_or_else(|e| format!("\n回滚失败：{e}（请点「还原」再试）"))
        );
    }
    if !verify_note.is_empty() {
        notes.push(verify_note);
    }

    // 4) 铺插件与模型（原子写入 + 逐个核对）
    on("正在铺 DLSS5 插件与模型…".to_owned(), 0.7);
    let mut placed: Vec<String> = Vec::new();
    for (src, name, role) in [
        (&addon, addon_name.clone(), "DLSS5 插件"),
        (&model, MODEL_NAME.to_owned(), "神经网络模型"),
    ] {
        let dst = dir.join(&name);
        let tmp = dir.join(format!(".{name}.fgm.tmp"));
        let step = std::fs::copy(src, &tmp)
            .with_context(|| format!("写入临时文件 {}", tmp.display()))
            .and_then(|_| util::atomic_replace(&tmp, &dst));
        if let Err(e) = step {
            let _ = std::fs::remove_file(&tmp);
            let rb = rollback_entries(&dir, &base, &entries);
            bail!(
                "铺 {name}（{role}）失败：{e}；回滚{}",
                rb.map(|m| format!("成功（{m}）"))
                    .unwrap_or_else(|e| format!("失败：{e}"))
            );
        }
        util::clear_motw(&dst);
        let want = util::sha256_file(src)?;
        let got = util::sha256_file(&dst)?;
        if got != want {
            let rb = rollback_entries(&dir, &base, &entries);
            bail!(
                "{name} 写入后校验失败；回滚{}",
                rb.map(|m| format!("成功（{m}）"))
                    .unwrap_or_else(|e| format!("失败：{e}"))
            );
        }
        placed.push(name);
    }

    // 5) 记录我们写进去的哈希（失败时留空，还原时只会保守处理）
    for e in manifest.files.iter_mut() {
        if want_files.iter().any(|(n, _)| n == &e.rel_path) {
            e.deployed_sha256 = util::sha256_file(&dir.join(&e.rel_path)).ok();
        }
    }

    // 6) 清掉旧的 DLSS5 插件（插件自己要求「只留一个」）
    let mut removed_addons: Vec<String> = Vec::new();
    for name in &stale_addons {
        let p = dir.join(name);
        if !p.is_file() {
            continue;
        }
        let Ok(cur) = util::sha256_file(&p) else { continue };
        let still_ours = prev
            .iter()
            .find(|e| &e.rel_path == name)
            .and_then(|r| r.deployed_sha256.clone())
            .map(|d| d == cur)
            .unwrap_or(false);
        if still_ours && std::fs::remove_file(&p).is_ok() {
            removed_addons.push(name.clone());
        }
    }
    if !removed_addons.is_empty() {
        notes.push(format!(
            "已移出旧的 DLSS5 插件：{}（插件要求同一目录只留一个）",
            removed_addons.join("、")
        ));
    }
    write_manifest(&base, &manifest)?;

    let mut names: Vec<String> = placed.clone();
    if let Some(m) = &module {
        names.insert(0, m.clone());
    }
    let mut msg = format!("已部署 {} -> {}", names.join("、"), dir.display());
    if let Some(n) = &plan.note {
        msg.push_str(&format!("（{n}）"));
    }
    notes.push(format!(
        "部署记录：{}（「还原」就按它恢复）",
        manifest_path(&dir)
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    ));
    notes.push("首次进游戏请确认 ReShade 的插件列表里能看到 DLSS5；模型只对 20/30 系有效。".to_owned());
    Ok((msg, notes))
}

/// 安装器的失败文案 -> 给用户看的中文理由。
fn friendly_setup_error(out: &str, dir: &Path, plan: &ApiPlan) -> String {
    let low = out.to_ascii_lowercase();
    let module = plan.module.unwrap_or("（Vulkan 全局层）");
    if low.contains("existing reshade installation") {
        return format!(
            "目录里已经有一份 ReShade，但安装器不接受这次更新（{}）。先用 ReShade 自带的卸载把它清掉，或点「还原」再试。\n安装器原话：{out}",
            dir.join(module).display()
        );
    }
    if low.contains("does not belong to reshade") || low.contains("for another api found") {
        let occ = describe_occupant(&dir.join(module));
        return format!(
            "这个位置（{module}）已经被别的文件占了：{occ}。\n换一个「图形 API」再试（例如原来用 dxgi，可以改成 DX12 -> d3d12.dll），或者先还原占用它的那个 Mod。\n安装器原话：{out}"
        );
    }
    if low.contains("banned") {
        return format!("上游把这款游戏列进了「禁止注入」名单，装不上。\n安装器原话：{out}");
    }
    if out.trim().is_empty() {
        return "安装器没有给出任何说明（进程可能被安全软件拦了）".to_owned();
    }
    out.to_owned()
}

fn looks_like_existing_reshade(out: &str) -> bool {
    out.to_ascii_lowercase().contains("existing reshade installation")
}

/// Vulkan 全局层的本体路径。
fn vulkan_module_path(is64: bool) -> PathBuf {
    let base = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".to_owned());
    PathBuf::from(base)
        .join(COMMON_DIR)
        .join(if is64 { "ReShade64.dll" } else { "ReShade32.dll" })
}

// ---------------------------------------------------------------- 还原

/// 把备份放回去、删掉我们写进去的文件。**必须先把备份读完校验完再动目录**
/// （同 deploy.rs：备份坏了要中止，而不是删一半留下一地鸡毛）。回滚与「还原」共用它。
fn rollback_entries(dir: &Path, base: &Path, entries: &[Entry]) -> Result<String> {
    let files_dir = base.join("files");
    let mut plan: Vec<(&Entry, Vec<u8>)> = Vec::new();
    for e in entries {
        if !e.existed_before {
            continue;
        }
        let Some(bn) = &e.backup_name else { continue };
        let src = files_dir.join(bn);
        let bytes = std::fs::read(&src).with_context(|| format!("读取备份 {}", src.display()))?;
        if let Some(orig) = &e.original_sha256 {
            if util::sha256_hex(&bytes) != *orig {
                bail!("备份 {bn} 自身已损坏，中止（目录一个字都没动）");
            }
        }
        plan.push((e, bytes));
    }
    let mut restored = 0usize;
    for (e, bytes) in &plan {
        let dst = dir.join(&e.rel_path);
        let tmp = dir.join(format!(".{}.fgm.tmp", e.rel_path));
        std::fs::write(&tmp, bytes)?;
        util::atomic_replace(&tmp, &dst)?;
        restored += 1;
    }
    let mut removed = 0usize;
    let mut forced: Vec<String> = Vec::new();
    for e in entries {
        if e.existed_before {
            continue;
        }
        // 只删「确实还是我们写进去的那一份」（哈希对得上）；对不上就留着，别替用户丢文件。
        // **例外**：ReShade 自己的配置/日志/预设这三个名字（见 is_reshade_own_text）。
        // 它们是我们装 ReShade 时创建的，跑过一次游戏就会被 ReShade / 插件改写 ——
        // 若按哈希跳过，用户点完「还原」目录里还杵着 ReShade.ini，看着就是"没卸干净"。
        // DLL / 插件 / 模型绝不走这条：用户换过的必须留下。
        let p = dir.join(&e.rel_path);
        if !p.is_file() {
            continue;
        }
        let ours = e
            .deployed_sha256
            .as_ref()
            .map(|d| util::sha256_file(&p).map(|h| &h == d).unwrap_or(false))
            .unwrap_or(false);
        if ours && std::fs::remove_file(&p).is_ok() {
            removed += 1;
        } else if is_reshade_own_text(&e.rel_path) && std::fs::remove_file(&p).is_ok() {
            removed += 1;
            forced.push(e.rel_path.clone());
        }
    }
    // 插件运行时自己写的残渣（ReShade.ini 备份 / 支持报告 / 崩溃与追踪文件）：
    // 它们不在 manifest 里，不扫一遍就永远留着。
    let swept = sweep_addon_leftovers(dir);
    let mut msg = format!("恢复 {restored} 个原件、删除 {removed} 个本工具写入的文件");
    if !forced.is_empty() {
        msg.push_str(&format!(
            "（其中 {} 已被 ReShade/插件改写，按名字一并删除）",
            forced.join("、")
        ));
    }
    if !swept.is_empty() {
        msg.push_str(&format!(
            "；另清理插件运行时留下的 {} 个文件：{}",
            swept.len(),
            swept.join("、")
        ));
    }
    Ok(msg)
}

/// ReShade 自己写的三个文本文件（我们装它时创建的）。还原时按名字删，不看哈希。
fn is_reshade_own_text(name: &str) -> bool {
    let low = name.to_ascii_lowercase();
    low == "reshade.ini" || low == "reshade.log" || low == "reshadepreset.ini"
}

/// 插件运行时在游戏目录里留下的东西（名字取自插件二进制里的字符串）：
///   ReShade.ini.renodx-dlss5-*    它给 ReShade.ini 做的备份
///   renodx-dlss5-report*.zip      支持报告（内含 logs/、capture/、report.json）
///   RenoDX-DLSS5-crash*.log/.dmp  崩溃日志与转储（开了 NRCrashDump=1 才有）
///   RenoDX-DLSS5-*trace.csv       NR 追踪（开了才有）
/// 这些都不在部署记录里，所以「还原」时要按前缀扫一遍，否则目录里会留一堆残渣。
/// 列出（不删）插件运行时留下的残渣文件名。
fn addon_leftover_names(dir: &Path) -> Vec<String> {
    const PREFIXES: [&str; 6] = [
        "reshade.ini.renodx-dlss5",
        "renodx-dlss5-report",
        "renodx-dlss5-crash",
        "renodx-dlss5-normtrace",
        "renodx-dlss5-edittrace",
        "renodx-dlss5-",
    ];
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        let low = name.to_ascii_lowercase();
        // 插件本体（*.addon64）不归这里管：它由部署记录或卸载流程单独处理
        if low.ends_with(".addon") || low.ends_with(".addon32") || low.ends_with(".addon64") {
            continue;
        }
        if PREFIXES.iter().any(|p| low.starts_with(p)) {
            out.push(name);
        }
    }
    out
}

fn sweep_addon_leftovers(dir: &Path) -> Vec<String> {
    let mut removed = Vec::new();
    for name in addon_leftover_names(dir) {
        if std::fs::remove_file(dir.join(&name)).is_ok() {
            removed.push(name);
        }
    }
    removed
}

/// 目录里最新的那个 DLSS5 插件（renodx-dlss5*.addon64）。
fn first_addon_in(dir: &Path) -> Option<String> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(u32, u32, u32, String)> = None;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let low = name.to_ascii_lowercase();
        if !low.starts_with(ADDON_PREFIX) || !low.ends_with(ADDON_EXT) {
            continue;
        }
        let v = version_key(&name);
        if best.as_ref().map(|(a, b, c, _)| v > (*a, *b, *c)).unwrap_or(true) {
            best = Some((v.0, v.1, v.2, name));
        }
    }
    best.map(|(_, _, _, n)| n)
}

/// 代理文件名 -> ReShade 的 api 参数（卸载时要靠它告诉安装器删哪个文件）。
fn api_of_module(module: &str) -> Option<&'static str> {
    match module.to_ascii_lowercase().as_str() {
        "d3d9.dll" => Some("d3d9"),
        "d3d10.dll" => Some("d3d10"),
        "d3d11.dll" => Some("d3d11"),
        "d3d12.dll" => Some("d3d12"),
        "dxgi.dll" => Some("dxgi"),
        "opengl32.dll" => Some("opengl"),
        _ => None,
    }
}

// ---------------------------------------------------------------- 探测 / 彻底卸载

/// 目标目录里现在的 DLSS5 / ReShade 状况。
///
/// **不看我们的记录也认**：用户自己手动装过的 ReShade / 插件 / 模型同样要能发现 ——
/// 用户要求「能检测出已经装过」，并给一个彻底卸载的入口。
#[derive(Debug, Clone, Serialize)]
pub struct Found {
    /// 本工具的部署记录在（这时「还原」才是推荐路径：能恢复原件）
    pub ours: bool,
    /// ReShade 本体（代理文件名，按 ProductName=ReShade 判定，不看签名）
    pub reshade_module: Option<String>,
    pub reshade_ini: bool,
    pub reshade_log: bool,
    pub preset: bool,
    /// ReShade 的效果包目录
    pub shaders_dir: bool,
    /// DLSS5 插件文件名
    pub addon: Option<String>,
    /// 神经网络模型在不在
    pub model: bool,
    /// 插件运行时留下的残渣（文件名）
    pub leftovers: Vec<String>,
}

impl Found {
    pub fn anything(&self) -> bool {
        self.reshade_module.is_some()
            || self.reshade_ini
            || self.reshade_log
            || self.preset
            || self.shaders_dir
            || self.addon.is_some()
            || self.model
            || !self.leftovers.is_empty()
    }

    /// 界面上那一行文案。
    pub fn label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(m) = &self.reshade_module {
            parts.push(format!("ReShade 本体 {m}"));
        }
        if self.reshade_ini {
            parts.push(RE_INI.to_owned());
        }
        if self.reshade_log {
            parts.push(RE_LOG.to_owned());
        }
        if self.preset {
            parts.push(RE_PRESET.to_owned());
        }
        if self.shaders_dir {
            parts.push(format!("{SHADERS_DIR}\\_（整个目录）", ));
        }
        if let Some(a) = &self.addon {
            parts.push(format!("DLSS5 插件 {a}"));
        }
        if self.model {
            parts.push(MODEL_NAME.to_owned());
        }
        if !self.leftovers.is_empty() {
            parts.push(format!("插件残留 {} 个", self.leftovers.len()));
        }
        if parts.is_empty() {
            return "未检测到".to_owned();
        }
        let who = if self.ours { "本工具部署的" } else { "非本工具安装的" };
        format!("{who}：{}", parts.join("、"))
    }
}

pub fn detect(dir: &Path) -> Found {
    let ours = matches!(state_of(dir), State::Deployed(_));
    let mut reshade_module = None;
    for name in [
        "d3d9.dll",
        "d3d10.dll",
        "d3d11.dll",
        "d3d12.dll",
        "dxgi.dll",
        "opengl32.dll",
    ] {
        let p = dir.join(name);
        if p.is_file() {
            if let Some(pn) = product_name(&p) {
                if pn.trim().to_ascii_lowercase().starts_with("reshade") {
                    reshade_module = Some(name.to_owned());
                    break;
                }
            }
        }
    }
    Found {
        ours,
        reshade_module,
        reshade_ini: dir.join(RE_INI).is_file(),
        reshade_log: dir.join(RE_LOG).is_file(),
        preset: dir.join(RE_PRESET).is_file(),
        shaders_dir: dir.join(SHADERS_DIR).is_dir(),
        addon: first_addon_in(dir),
        model: dir.join(MODEL_NAME).is_file(),
        leftovers: addon_leftover_names(dir),
    }
}

/// **彻底卸载** ReShade + DLSS5：先让官方卸载器删本体 / ReShade.ini / ReShade.log / 效果包目录，
/// 我们再补掉插件、模型、插件运行时残渣。
///
/// 全部是永久删除（remove_file / remove_dir_all，不进回收站）—— 所以**调用方必须先拿到用户二次确认**。
pub fn uninstall(dir: &Path, exe: Option<&Path>, on: &mut dyn FnMut(String, f32)) -> Result<String> {
    if !dir.is_dir() {
        bail!("目录不存在：{}", dir.display());
    }
    let found = detect(dir);
    if !found.anything() {
        bail!("没有在这个目录里检测到 ReShade / DLSS5 的文件，无需卸载。");
    }
    let mut notes: Vec<String> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();

    // 1) 有本体 + 有游戏 exe + 有安装器：先用官方卸载器。
    //    它会删本体、ReShade.ini、ReShade.log，并按 ini 里记的搜索路径删掉效果包目录 ——
    //    这份清单比我们自己硬编码的可靠。
    let exe = exe
        .map(|p| p.to_path_buf())
        .filter(|p| p.is_file())
        .or_else(|| scan::find_render_exe(dir));
    if let (Some(module), Some(exe), Some(setup)) = (
        found.reshade_module.as_deref(),
        exe.as_deref(),
        find_setup(),
    ) {
        if let Some(api) = api_of_module(module) {
            on("正在用官方卸载器移除 ReShade…".to_owned(), 0.2);
            match run_setup(&setup, exe, api, Some("uninstall")) {
                Ok((0, _out, false)) => {
                    // 官方卸载器删掉的东西也记进清单，否则「已彻底卸载 2 项」会让人以为
                    // 本体/ini/log 还在（其实它们已经被官方卸载器清掉了）。
                    deleted.push(format!("{module}（官方卸载器）"));
                    if found.reshade_ini {
                        deleted.push(format!("{RE_INI}（官方卸载器）"));
                    }
                    if found.reshade_log {
                        deleted.push(format!("{RE_LOG}（官方卸载器）"));
                    }
                    if found.shaders_dir {
                        deleted.push(format!("{SHADERS_DIR}\\（官方卸载器）"));
                    }
                    notes.push(format!(
                        "官方卸载器已移除 {module}（含 {RE_INI} / {RE_LOG} / 效果包目录）"
                    ))
                }
                Ok((code, out, timed)) => notes.push(format!(
                    "官方卸载器没成功（退出码 {code}{}）：{}；改用直接删除",
                    if timed { "，超时" } else { "" },
                    if out.trim().is_empty() { "无输出" } else { out.trim() }
                )),
                Err(e) => notes.push(format!("官方卸载器跑不起来：{e}；改用直接删除")),
            }
        }
    } else if found.reshade_module.is_some() {
        notes.push("没找到 ReShade 安装器或游戏 exe，直接用文件删除的方式清理".to_owned());
    }

    // 2) 自己再扫一遍：插件、模型、残渣，以及官方卸载器没删干净的 ReShade 文件
    on("正在删除插件 / 模型与运行时残留…".to_owned(), 0.65);
    let mut names: Vec<String> = Vec::new();
    if let Some(m) = &found.reshade_module {
        names.push(m.clone());
    }
    names.push(RE_INI.to_owned());
    names.push(RE_LOG.to_owned());
    names.push(RE_PRESET.to_owned());
    if let Some(a) = &found.addon {
        names.push(a.clone());
    }
    names.push(MODEL_NAME.to_owned());
    for name in names {
        let p = dir.join(&name);
        if p.is_file() && std::fs::remove_file(&p).is_ok() {
            deleted.push(name);
        }
    }
    let shaders = dir.join(SHADERS_DIR);
    if shaders.is_dir() && std::fs::remove_dir_all(&shaders).is_ok() {
        deleted.push(format!("{SHADERS_DIR}\\（整个目录）"));
    }
    for n in sweep_addon_leftovers(dir) {
        deleted.push(n);
    }

    // 3) 是本工具部署的：把部署记录/备份一起清掉（和「还原」收尾一致）
    if found.ours {
        if let Ok(base) = base_dir(dir) {
            if std::fs::remove_dir_all(&base).is_ok() {
                notes.push("本工具的部署记录与备份也一并删除了".to_owned());
            }
        }
    }

    // 4) 复查：还有没有漏网的（多半是被游戏占着）
    let left = detect(dir);
    if left.anything() {
        notes.push(format!(
            "注意：还有没删掉的东西（可能正被游戏或其它程序占用）：{}",
            left.label()
        ));
    } else {
        notes.push("复查通过：这个目录里已经没有 ReShade / DLSS5 的任何文件".to_owned());
    }
    notes.push("以上都是直接删除（未进回收站），不可恢复；需要的话重新点「部署 DLSS5」即可装回来".to_owned());

    let mut msg = format!(
        "已彻底卸载 {} 项：{}",
        deleted.len(),
        if deleted.is_empty() {
            "（没有可删的文件）".to_owned()
        } else {
            deleted.join("、")
        }
    );
    for n in notes {
        msg.push_str(&format!("\n· {n}"));
    }
    Ok(msg)
}

pub fn restore(dir: &Path) -> Result<String> {
    let m = match state_of(dir) {
        State::Deployed(m) => *m,
        State::NotDeployed => {
            bail!("没有找到本工具对该目录的 DLSS5 部署记录（{}）", dir.display())
        }
        State::Broken(e) => bail!("这个目录的 DLSS5 记录读不出来（{e}），不敢按它还原"),
    };
    let base = base_dir(dir)?;
    let _lock = DirLock::acquire(&base)?;

    // Vulkan 是全局安装：本体不在游戏目录，得让官方安装器去注销（要 UAC）
    let mut vulkan_note = String::new();
    if m.api == "vulkan" {
        if let Some(setup) = find_setup() {
            let exe = PathBuf::from(&m.exe);
            if exe.is_file() {
                match run_setup(&setup, &exe, "vulkan", None) {
                    Ok((0, _out, false)) => {
                        vulkan_note = "；Vulkan 全局层已按官方方式注销".to_owned();
                    }
                    Ok((code, out, timed)) => {
                        vulkan_note = format!(
                            "；Vulkan 全局层没能注销（退出码 {code}{}）：{}。需要的话可以到 %ProgramData%\\ReShade 手动清理",
                            if timed { "，超时" } else { "" },
                            if out.is_empty() { "无输出".to_owned() } else { out }
                        );
                    }
                    Err(e) => vulkan_note = format!("；Vulkan 全局层注销失败：{e}"),
                }
            }
        }
    }

    let msg = rollback_entries(dir, &base, &m.files)?;
    // 备份目录整个删掉（记录+备份都清干净），和帧生成那边的收尾一致
    let _ = std::fs::remove_dir_all(&base);
    Ok(format!("已还原 DLSS5：{msg}{vulkan_note}"))
}
