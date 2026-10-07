//! Steam / Epic 游戏库扫描 + 渲染 EXE 探测。全流程只读，不写注册表、不改 VDF。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
use winreg::RegKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Launcher {
    Steam,
    Epic,
    /// 腾讯 WeGame。扫描判定很保守，见 scan_wegame() 的注释。
    WeGame,
    /// 用户自己「存到游戏库」的目录，不来自任何平台扫描
    Manual,
    /// 在「像游戏库的目录」里扫到的游戏（绿色版 / 手工解压 / 学习版都算）。
    /// 这类游戏不属于任何平台，只能靠目录结构和 exe 认出来。
    Loose,
}

impl Launcher {
    pub fn label(self) -> &'static str {
        match self {
            Launcher::Steam => "Steam",
            Launcher::Epic => "Epic",
            Launcher::WeGame => "WeGame",
            Launcher::Manual => "手动",
            Launcher::Loose => "本地目录",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameEntry {
    pub source: Launcher,
    pub app_id: String,
    pub name: String,
    pub install_dir: PathBuf,
}

// ---------------------------------------------------------------- 游戏库缓存

/// 缓存里的一行：条目本身 + 扫描时算出来的结果。
///
/// 为什么要缓存：找渲染 EXE 要遍历游戏目录、反作弊要扫一遍目录，都很花时间。
/// 启动时拿这份缓存直接显示列表，不用用户再点一次「扫描」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedRow {
    pub entry: GameEntry,
    /// 上次找到的渲染 EXE。找到过就记住，免得每次启动重新遍历游戏目录。
    #[serde(default)]
    pub render_exe: Option<PathBuf>,
    /// 上次判定的反作弊等级
    #[serde(default = "tier_none")]
    pub ac: crate::anticheat::AcTier,
    /// 渲染 EXE 用的图形 API（DX12 / Vulkan / …）
    #[serde(default)]
    pub api: GraphicsApi,
    /// 游戏引擎（认不出来就是 Unknown）
    #[serde(default)]
    pub engine: GameEngine,
    /// 游戏自带的导入表里有没有 Streamline（= 自带 DLSS 帧生成）
    #[serde(default)]
    pub streamline: bool,
    /// 上面三项**是不是真的算过**。
    ///
    /// 加这个字段是因为踩过坑：老缓存里没有 api/engine 字段，反序列化后是默认值
    /// （Unknown），而代码把「Unknown」当成「已经算过」→ 游戏库那一行永远不显示
    /// 图形 API 和引擎，重新扫描也一样（用户看到的就是「功能没生效」）。
    #[serde(default)]
    pub tech_scanned: bool,
}

fn tier_none() -> crate::anticheat::AcTier {
    crate::anticheat::AcTier::None
}

/// 整个游戏库：扫出来的 + 用户手动存的。
///
/// 两者分开存，是为了「重新扫描」不会把用户手动加的条目冲掉。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LibraryCache {
    /// 上次扫描完成的时间（UTC，空表示没扫过）
    #[serde(default)]
    pub scanned_at: String,
    #[serde(default)]
    pub scanned: Vec<CachedRow>,
    #[serde(default)]
    pub manual: Vec<CachedRow>,
    /// 用户主动移除过的条目（按安装目录记）。重新扫描时不会再列出来。
    #[serde(default)]
    pub ignored: Vec<String>,
}

const LIBRARY_NAME: &str = "game_library.json";

/// 缓存文件位置：和配置文件放在一起（便携版就在 exe 旁边）。
pub fn library_path() -> PathBuf {
    crate::util::config_path().with_file_name(LIBRARY_NAME)
}

pub fn load_library() -> LibraryCache {
    load_library_from(&library_path())
}

pub fn save_library(c: &LibraryCache) -> anyhow::Result<()> {
    save_library_from(&library_path(), c)
}

/// 指定路径的版本，自测用（不去碰用户真正的游戏库文件）。
pub fn load_library_from(p: &Path) -> LibraryCache {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_library_from(p: &Path, c: &LibraryCache) -> anyhow::Result<()> {
    let t = serde_json::to_string_pretty(c)?;
    std::fs::write(p, t)?;
    Ok(())
}

/// 手动条目：用户把当前目录存进游戏库时构造。
/// 名字取目录名，装的就是用户选的那个目录本身（不去猜渲染 EXE 在哪一级）。
pub fn manual_entry(dir: &Path) -> GameEntry {
    let name = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| dir.display().to_string());
    GameEntry {
        source: Launcher::Manual,
        app_id: String::new(),
        name,
        install_dir: dir.to_path_buf(),
    }
}

// ---------------------------------------------------------------- 最小 VDF 解析

enum Tok {
    Str(String),
    Open,
    Close,
}

fn tokenize(text: &str) -> Vec<Tok> {
    let chars: Vec<char> = text.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '"' => {
                let mut s = String::new();
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        s.push(chars[i + 1]);
                        i += 2;
                    } else {
                        s.push(chars[i]);
                        i += 1;
                    }
                }
                i += 1;
                toks.push(Tok::Str(s));
            }
            '{' => {
                toks.push(Tok::Open);
                i += 1;
            }
            '}' => {
                toks.push(Tok::Close);
                i += 1;
            }
            _ => i += 1,
        }
    }
    toks
}

/// 取出所有 "key" "value"。键后面跟 { 的块会被跳过，
/// 所以 "0" { "path" "..." } 不会把 "0" 和 "path" 错配成一对。
pub fn vdf_pairs(text: &str) -> Vec<(String, String)> {
    let toks = tokenize(text);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        if let Tok::Str(k) = &toks[i] {
            if let Some(Tok::Str(v)) = toks.get(i + 1) {
                out.push((k.clone(), v.clone()));
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------- Steam

fn reg_str(root: RegKey, sub: &str, value: &str) -> Option<String> {
    root.open_subkey(sub).ok()?.get_value::<String, _>(value).ok()
}

/// Steam 注册表里的路径常带正斜杠（d:/steam），会导致显示难看，
/// 而且 SHGetFileInfoW 这类 Shell API 遇到混合分隔符会失败。统一成反斜杠。
fn normalize_win_path(s: &str) -> PathBuf {
    PathBuf::from(s.replace('/', "\\"))
}

fn norm_key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase().replace('/', "\\")
}

/// 卸载项里那条是不是 Steam 本体（SteamVR / Steamworks 之类的名字不算）。
pub fn uninstall_is_steam(display: &str) -> bool {
    display.trim().eq_ignore_ascii_case("steam")
}

/// 从「卸载」列表里找出 Steam 安装目录。
///
/// 为什么要这一步：有些机器上 Valve\Steam 那两个键是缺的（换过盘、装过两份、
/// 或是用别的方式装的），但「卸载」项里一定有 InstallLocation。装了两份 Steam 时，
/// 也只有这里能同时看到两处。
fn steam_from_uninstall() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    for root in [
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
        "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
    ] {
        let Ok(k) = hklm.open_subkey(root) else { continue };
        for name in k.enum_keys().flatten() {
            let Ok(gk) = k.open_subkey(&name) else { continue };
            let disp = gk.get_value::<String, _>("DisplayName").unwrap_or_default();
            if !uninstall_is_steam(&disp) {
                continue;
            }
            let Ok(loc) = gk.get_value::<String, _>("InstallLocation") else {
                continue;
            };
            let p = normalize_win_path(&loc);
            // 卸载项里的路径不一定靠谱，得能看出这是 Steam 才算
            if p.join("steamapps").is_dir() || p.join("steam.exe").is_file() {
                out.push(p);
            }
        }
    }
    out
}

/// 找出这台机器上所有 Steam 安装目录。
///
/// 来源顺序（全部都会试，最后去重）：
///   1. HKCU\Software\Valve\Steam\SteamPath（最常见的那个）
///   2. HKLM\...\Valve\Steam\InstallPath（32 位视图和原生视图各试一次）
///   3. HKCU\Software\Valve\Steam\SteamExe 的所在目录（前两个都缺时的兜底）
///   4. 「卸载」项里的 InstallLocation（装了两份 Steam 时只有这里都能看到）
///   5. 两个默认安装路径（只在目录真的存在时才用）
pub fn steam_roots() -> Vec<PathBuf> {
    let mut cands: Vec<(PathBuf, &'static str)> = Vec::new();
    // 注：RegKey 不是 Clone，需要哪个就现场 predef 一个（predef 只是包一个预定义句柄，很便宜）
    if let Some(p) = reg_str(
        RegKey::predef(HKEY_CURRENT_USER),
        "Software\\Valve\\Steam",
        "SteamPath",
    ) {
        cands.push((normalize_win_path(&p), "HKCU SteamPath"));
    }
    for (sub, from) in [
        ("SOFTWARE\\WOW6432Node\\Valve\\Steam", "HKLM 32 位 InstallPath"),
        ("SOFTWARE\\Valve\\Steam", "HKLM 原生 InstallPath"),
    ] {
        if let Some(p) = reg_str(RegKey::predef(HKEY_LOCAL_MACHINE), sub, "InstallPath") {
            cands.push((normalize_win_path(&p), from));
        }
    }
    if let Some(exe) = reg_str(
        RegKey::predef(HKEY_CURRENT_USER),
        "Software\\Valve\\Steam",
        "SteamExe",
    ) {
        if let Some(dir) = normalize_win_path(&exe).parent() {
            cands.push((dir.to_path_buf(), "HKCU SteamExe"));
        }
    }
    for p in steam_from_uninstall() {
        cands.push((p, "卸载项 InstallLocation"));
    }
    if let Some(pf) = std::env::var_os("ProgramFiles(x86)") {
        cands.push((PathBuf::from(pf).join("Steam"), "默认路径"));
    }
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        cands.push((PathBuf::from(pf).join("Steam"), "默认路径"));
    }

    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (p, from) in cands {
        let k = norm_key(&p);
        if !p.is_dir() || seen.contains(&k) {
            continue;
        }
        seen.push(k);
        crate::log::line(&format!("发现 Steam：{}（来源：{from}）", p.display()));
        out.push(p);
    }
    if out.is_empty() {
        crate::log::line("没找到任何 Steam 安装目录（注册表和卸载项里都没有）");
    }
    out
}

/// 记一条扫描说明：既写进日志，也带回去给界面显示。
///
/// 扫描「扫不出来」这类反馈要靠日志才能定位 —— 每个跳过都记下理由，
/// 用户把 logs 目录发过来就能定论。
fn note(notes: &mut Vec<String>, s: String) {
    crate::log::line(&s);
    notes.push(s);
}

pub fn steam_libraries(root: &Path) -> Vec<PathBuf> {
    let mut notes = Vec::new();
    steam_libraries_notes(root, &mut notes)
}

pub fn steam_libraries_notes(root: &Path, notes: &mut Vec<String>) -> Vec<PathBuf> {
    let mut libs = vec![root.to_path_buf()];
    let vdf = root.join("steamapps").join("libraryfolders.vdf");
    match std::fs::read_to_string(&vdf) {
        Ok(text) => {
            for (k, v) in vdf_pairs(&text) {
                // 新格式用 "path"，老格式是数字键直接给路径
                let is_path = k.eq_ignore_ascii_case("path")
                    || (!k.is_empty() && k.chars().all(|c| c.is_ascii_digit()));
                if !is_path {
                    continue;
                }
                let p = normalize_win_path(&v);
                if !p.is_dir() {
                    note(
                        notes,
                        format!("  库里记着的目录不存在，跳过：{}", p.display()),
                    );
                    continue;
                }
                let k2 = norm_key(&p);
                if !libs.iter().any(|l| norm_key(l) == k2) {
                    libs.push(p);
                }
            }
        }
        Err(e) => {
            // 这条最要命：读不到库清单 = 只知道默认库，其它盘的游戏全看不到。
            note(
                notes,
                format!(
                    "读不了库清单 {}：{e} —— 这次只扫默认库，其它盘上的游戏会看不到",
                    vdf.display()
                ),
            );
        }
    }
    libs
}

/// 这些不是游戏，只是 Steam 的运行库 / 再分发包。
///
/// 按「名字里包含 proton 就算运行库」的子串匹配会把真游戏一起误杀
/// （Proton Bus Simulator 就是）；所以按 Steam 对运行库的固定命名来判：
/// 精确名 + 明确的版本号前缀。
pub fn is_steam_junk(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    // 固定名字
    if matches!(
        n.as_str(),
        "steamworks common redistributables"
            | "steamvr"
            | "steamvr beta"
            | "proton experimental"
            | "proton hotfix"
            | "proton - experimental"
            | "steam linux runtime"
    ) {
        return true;
    }
    // 带版本号的：Steam Linux Runtime 3.0 (sniper) / Proton 9.0
    if n.starts_with("steam linux runtime ") {
        return true;
    }
    if let Some(rest) = n.strip_prefix("proton ") {
        // 只有「Proton + 数字版本」才是运行库；Proton Bus Simulator 这种真游戏放过
        if rest.chars().next().map(|c| c.is_ascii_digit()) == Some(true) {
            return true;
        }
    }
    false
}

pub fn scan_steam_notes(notes: &mut Vec<String>) -> Vec<GameEntry> {
    let mut out = Vec::new();
    let roots = steam_roots();
    note(notes, format!("Steam：找到 {} 个安装目录", roots.len()));
    let (mut manifests, mut junk, mut missing_dir, mut read_fail) = (0usize, 0usize, 0usize, 0usize);

    for root in &roots {
        let libs = steam_libraries_notes(root, notes);
        let shown: Vec<String> = libs.iter().map(|l| l.display().to_string()).collect();
        note(
            notes,
            format!(
                "  根目录 {}：{} 个库目录（{}）",
                root.display(),
                libs.len(),
                shown.join(" | ")
            ),
        );
        for lib in &libs {
            let sa = lib.join("steamapps");
            let Ok(read) = std::fs::read_dir(&sa) else {
                note(
                    notes,
                    format!("  读不了库目录 {}（被占用或权限不足），里面的游戏这次会漏掉", sa.display()),
                );
                continue;
            };
            for e in read.flatten() {
                let fname = e.file_name().to_string_lossy().to_string();
                if !fname.starts_with("appmanifest_") || !fname.ends_with(".acf") {
                    continue;
                }
                manifests += 1;
                let text = match std::fs::read_to_string(e.path()) {
                    Ok(t) => t,
                    Err(err) => {
                        read_fail += 1;
                        note(
                            notes,
                            format!("  读不了 {}：{err}（这个游戏这次会漏掉）", e.path().display()),
                        );
                        continue;
                    }
                };
                let pairs = vdf_pairs(&text);
                let get = |key: &str| {
                    pairs
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(key))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default()
                };
                let app_id = get("appid");
                let name = get("name");
                let installdir = get("installdir");
                if name.is_empty() || installdir.is_empty() {
                    note(
                        notes,
                        format!(
                            "  跳过 {}：manifest 里没读到名字或目录名",
                            e.path().display()
                        ),
                    );
                    continue;
                }
                if is_steam_junk(&name) {
                    junk += 1;
                    note(notes, format!("  按运行库排除：{name}"));
                    continue;
                }
                let dir = sa.join("common").join(&installdir);
                if !dir.is_dir() {
                    missing_dir += 1;
                    note(
                        notes,
                        format!("  目录不存在，跳过 {name}（{}）", dir.display()),
                    );
                    continue;
                }
                out.push(GameEntry {
                    source: Launcher::Steam,
                    app_id,
                    name,
                    install_dir: dir,
                });
            }
        }
    }

    note(
        notes,
        format!(
            "Steam 扫描完成：读到 {manifests} 个 manifest，收下 {} 个游戏，跳过 {}（运行库 {junk}、目录不在 {missing_dir}、读失败 {read_fail}）",
            out.len(),
            junk + missing_dir + read_fail
        ),
    );
    out
}

// ---------------------------------------------------------------- Epic

pub fn epic_manifest_dir() -> PathBuf {
    PathBuf::from(r"C:\ProgramData\Epic\EpicGamesLauncher\Data\Manifests")
}

pub fn scan_epic_notes(notes: &mut Vec<String>) -> Vec<GameEntry> {
    let mut out = Vec::new();
    let dir0 = epic_manifest_dir();
    let Ok(read) = std::fs::read_dir(&dir0) else {
        note(
            notes,
            format!(
                "Epic：读不了清单目录 {}（没装 Epic 启动器时就是这样，属正常）",
                dir0.display()
            ),
        );
        return out;
    };
    let (mut total, mut not_app, mut no_field, mut missing) = (0usize, 0usize, 0usize, 0usize);
    for e in read.flatten() {
        let p = e.path();
        if p.extension().map(|x| x != "item").unwrap_or(true) {
            continue;
        }
        total += 1;
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(err) => {
                note(notes, format!("  读不了 {}：{err}", p.display()));
                continue;
            }
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            note(notes, format!("  解析不了 {}（不是合法 JSON）", p.display()));
            continue;
        };

        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_owned());
        // 只要应用本体，跳过引擎/插件/DLC
        if v.get("bIsApplication").and_then(|x| x.as_bool()) == Some(false) {
            not_app += 1;
            continue;
        }
        let Some(name) = s("DisplayName") else {
            no_field += 1;
            continue;
        };
        let Some(loc) = s("InstallLocation") else {
            no_field += 1;
            continue;
        };
        let app_id = s("AppName").unwrap_or_default();
        let dir = PathBuf::from(&loc);
        if name.is_empty() || !dir.is_dir() {
            missing += 1;
            note(
                notes,
                format!("  目录不存在，跳过 {name}（{}）", dir.display()),
            );
            continue;
        }
        out.push(GameEntry {
            source: Launcher::Epic,
            app_id,
            name,
            install_dir: dir,
        });
    }
    note(
        notes,
        format!(
            "Epic 扫描完成：清单 {total} 个，收下 {} 个，跳过 {}（引擎/DLC {not_app}、缺字段 {no_field}、目录不在 {missing}）",
            out.len(),
            not_app + no_field + missing
        ),
    );
    out
}

// ---------------------------------------------------------------- 极简 PE 解析
//
// 两处用到它：find_render_exe 判断哪个 exe 是渲染器；advise_proxy 判断游戏会不会
// 加载某个代理 DLL 名。
//
// 「把文件前 N MB 读进来再解析」对 232 MB 的 TslGame.exe 和 457 MB 的
// HogwartsLegacy.exe 完全失效 —— 导入表根本不在前 16 MB 内。
// 所以先读头部拿节表，再按节表把 RVA 换算成文件偏移，seek 过去精确读取，任意大小都能解析。

use std::io::{BufReader, Read, Seek, SeekFrom};

fn rd_u16(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn rd_u32(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

// ---------------------------------------------------------------- WeGame

/// WeGame 把「每个游戏装在哪」记在注册表的这些根下面，一个游戏一个子键。
///
/// 这个布局**没有官方文档**，是从社区资料推断出来的 —— 网上流传的「重装系统后重新
/// 关联 WeGame 游戏」的土办法，就是手工把这些键重建出来，说明这些键正是 WeGame
/// 判断安装位置的依据。WeGame 是 32 位程序，所以主要看 WOW6432Node 那一侧。
const WEGAME_REG_ROOTS: [&str; 2] = ["SOFTWARE\\WOW6432Node\\Tencent", "SOFTWARE\\Tencent"];

/// WeGame 自己的安装目录候选（用来找它自带的 apps 目录）。
const WEGAME_INSTALL_SUBDIRS: [&str; 4] =
    ["Tencent\\WeGame", "Tencent\\wegame", "WeGame", "wegame"];

/// 值名像不像「安装路径」。纯粹按名字猜 —— 这就是没文档的代价。
pub fn wegame_value_is_path_like(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("path") || n.contains("dir") || n.contains("install") || n.contains("location")
}

/// 值名像不像「游戏显示名」。
pub fn wegame_value_is_name_like(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "name" || n == "gamename" || n == "displayname" || n == "title"
}

/// 注册表键名 -> 游戏名。WeGame 的键名有时带编号后缀，去掉它。
pub fn wegame_name_from_key(key: &str) -> String {
    let s = key.trim().trim_end_matches(')');
    let mut out = s;
    if let Some(i) = s.rfind(['(', '_', '-']) {
        let tail = s[i + 1..].trim();
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            out = s[..i].trim_end();
        }
    }
    if out.is_empty() {
        s.to_owned()
    } else {
        out.to_owned()
    }
}

/// 一个目录的归一化 key：小写、统一用反斜杠、去掉末尾分隔符。
///
/// 用途是**比较两个路径是不是同一个目录** —— 忽略清单、去重、手动条目查重都得用它。
/// 直接拿 display() 的字符串比会漏：D:\Games\X、d:/games/x、D:\Games\X\ 这三种写法
/// 在用户和各个启动器的记录里都出现过。
pub fn path_key(p: &std::path::Path) -> String {
    p.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// 这些 Tencent 子键明显不是 WeGame 游戏（客户端、聊天工具、播放器、办公工具）。
///
/// **只做精确匹配**，不做前缀匹配 —— 免得把「QQ飞车」「QQ炫舞」这种真游戏一起误杀。
/// 用户反馈「WeGame 列表里出现无关软件」时，先往这里加：加错名字只是不生效，
/// 不会误伤真游戏。
const WEGAME_NON_GAME_KEYS: [(&str, &str); 26] = [
    ("WeGame", "WeGame 客户端本体"),
    ("wegame", "WeGame 客户端本体"),
    ("WeGameX", "WeGame 国际版客户端"),
    ("QQ", "QQ 客户端"),
    ("QQNT", "QQ 新版客户端"),
    ("TencentQQ", "QQ 客户端"),
    ("QQProtect", "QQ 安全组件"),
    ("WeChat", "微信"),
    ("Weixin", "微信"),
    ("TIM", "TIM"),
    ("QQMusic", "QQ 音乐"),
    ("QQPlayer", "腾讯播放器"),
    ("QQLive", "腾讯视频"),
    ("TencentVideo", "腾讯视频"),
    ("TencentMeeting", "腾讯会议"),
    ("TXMeeting", "腾讯会议"),
    ("QQBrowser", "QQ 浏览器"),
    ("QQPinyin", "QQ 输入法"),
    ("QQInput", "QQ 输入法"),
    // 下面几条来自用户反馈的真实日志（QQPCMgr 那条曾经被当成游戏收下）
    ("QQPCMgr", "腾讯电脑管家"),
    ("QQPCMgrApps", "腾讯电脑管家"),
    ("PcMgrBrowserHp", "腾讯电脑管家组件"),
    ("QMUpdate", "QQ 音乐更新组件"),
    ("QQPhotoDrawEx", "QQ 空间组件"),
    ("QQ2009", "老版 QQ"),
    ("TencentDocs", "腾讯文档"),
];

/// 挑中的「渲染 EXE」如果是这些程序，说明那个目录根本不是游戏。
///
/// 按主名精确匹配（去掉 .exe、转小写）。这里只放**不可能是游戏本体**的名字 ——
/// 所以不放 update / setup 这类通用词。
const NON_GAME_EXE_STEMS: [(&str, &str); 18] = [
    ("qq", "QQ"),
    ("qqnt", "QQ 新版客户端"),
    ("qqprotect", "QQ 安全组件"),
    ("wechat", "微信"),
    ("weixin", "微信"),
    ("tim", "TIM"),
    ("wegame", "WeGame 客户端"),
    ("wegamex", "WeGame 客户端"),
    ("qqmusic", "QQ 音乐"),
    ("qqplayer", "腾讯播放器"),
    ("qqlive", "腾讯视频"),
    ("tencentvideo", "腾讯视频"),
    ("tencentmeeting", "腾讯会议"),
    ("txmeeting", "腾讯会议"),
    ("qqbrowser", "QQ 浏览器"),
    ("qqpinyin", "QQ 输入法"),
    // QQ 电脑管家里捆绑的微信 OCR 组件 —— 用户报的 QQPCMgr 就是被它顶进来的
    ("wechatocr", "微信 OCR（捆绑组件）"),
    ("tencentdocs", "腾讯文档"),
];

/// 安装目录里出现这些**目录名**，就说明这不是游戏（腾讯的客户端 / 工具）。
///
/// 为什么需要这一条：注册表键名可以叫任何名字。用户反馈的 QQPCMgr（腾讯电脑管家）
/// 键名没进名单、目录里又挑中了它捆绑的 WeChatOCR.exe，两道闸都没拦住。
/// **产品装在哪个目录里是藏不住的** —— 按路径分量判比按 exe 文件名判稳得多，
/// 而且以后腾讯再出新产品（键名我们没见过）也能挡住。
///
/// 刻意**不放** "Tencent" 和 "WeGame"：WeGame 自己的 apps 目录结构里就带这两个词，
/// 加了会把真游戏一起误杀。只放「只可能是这个产品」的目录名。
const NON_GAME_DIR_MARKERS: [(&str, &str); 15] = [
    ("QQPCMgr", "腾讯电脑管家"),
    ("QQMusic", "QQ 音乐"),
    ("QQLive", "腾讯视频"),
    ("TencentVideo", "腾讯视频"),
    ("TencentMeeting", "腾讯会议"),
    ("TXMeeting", "腾讯会议"),
    ("QQBrowser", "QQ 浏览器"),
    ("QQPinyin", "QQ 输入法"),
    ("TencentDocs", "腾讯文档"),
    ("WeChat", "微信"),
    ("Weixin", "微信"),
    ("TIM", "TIM"),
    ("QQProtect", "QQ 安全组件"),
    ("Qzone", "QQ 空间"),
    ("Foxmail", "Foxmail"),
];

/// 路径里有没有「已知非游戏产品」的目录名；有就返回那个产品名。
///
/// 只看**完整的路径分量**（大小写不敏感），不做子串匹配 —— 否则某个游戏目录名里
/// 恰好含有 "TIM" 这种短词就会被误杀。
pub fn non_game_dir_marker(path: &Path) -> Option<&'static str> {
    for c in path.components() {
        let name = c.as_os_str().to_string_lossy();
        if let Some((_, label)) = NON_GAME_DIR_MARKERS
            .iter()
            .find(|(n, _)| name.eq_ignore_ascii_case(n))
        {
            return Some(label);
        }
    }
    None
}

/// 一条 WeGame 候选的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WegameVerdict {
    /// 当成游戏收下
    Game,
    /// 键名是已知的非游戏产品
    NonGameKey,
    /// 值里没读出任何存在的目录
    NoDir,
    /// 目录里找不到像样的可执行文件
    NoRenderExe,
    /// 挑中的可执行文件是已知的非游戏程序
    NonGameExe,
    /// 目录（或可执行文件）落在已知非游戏产品的安装目录里
    NonGamePath,
}

impl WegameVerdict {
    pub fn label(self) -> &'static str {
        match self {
            WegameVerdict::Game => "收下",
            WegameVerdict::NonGameKey => "跳过：非游戏键",
            WegameVerdict::NoDir => "跳过：没有目录",
            WegameVerdict::NoRenderExe => "跳过：没有可执行文件",
            WegameVerdict::NonGameExe => "跳过：可执行文件是非游戏程序",
            WegameVerdict::NonGamePath => "跳过：装在非游戏产品的目录里",
        }
    }
}

/// 键名是不是已知的非游戏产品；是的话返回它的标签（给日志用）。
pub fn wegame_non_game_key(key: &str) -> Option<&'static str> {
    WEGAME_NON_GAME_KEYS
        .iter()
        .find(|(n, _)| key.eq_ignore_ascii_case(n))
        .map(|(_, label)| *label)
}

/// 可执行文件是不是已知的非游戏程序；是的话返回它的标签。
pub fn non_game_exe(exe: &Path) -> Option<&'static str> {
    let stem = exe.file_stem()?.to_str()?.to_ascii_lowercase();
    NON_GAME_EXE_STEMS
        .iter()
        .find(|(n, _)| stem == *n)
        .map(|(_, label)| *label)
}

/// WeGame 一条候选怎么判 —— **不碰注册表、不碰磁盘的纯函数**。
///
/// 抽出来是为了能在自测里用构造数据覆盖真实世界的例子（真游戏 / QQ / 微信 /
/// QQ音乐 / 腾讯会议…）：开发机上没装 WeGame，这些规则否则根本没法验证。
/// 返回 (判定, 给日志的一句话理由)。
pub fn wegame_verdict(
    key_name: &str,
    dir: Option<&Path>,
    render_exe: Option<&Path>,
) -> (WegameVerdict, String) {
    if let Some(label) = wegame_non_game_key(key_name) {
        return (
            WegameVerdict::NonGameKey,
            format!("跳过：键名「{key_name}」是已知的非游戏产品（{label}）"),
        );
    }
    let Some(dir) = dir else {
        return (
            WegameVerdict::NoDir,
            "跳过：这个键的所有值都没指向一个存在的目录".to_owned(),
        );
    };
    // 目录名就暴露了它是谁：用户反馈的 QQPCMgr 键名没进名单，但它装在
    // C:\Program Files (x86)\Tencent\QQPCMgr\ 下面 —— 这一条与键名无关，
    // 新出的腾讯产品也能挡住。
    if let Some(label) = non_game_dir_marker(dir) {
        return (
            WegameVerdict::NonGamePath,
            format!("跳过：安装目录属于「{label}」（{}）", dir.display()),
        );
    }
    let Some(exe) = render_exe else {
        return (
            WegameVerdict::NoRenderExe,
            format!("跳过：{} 里找不到像样的可执行文件", dir.display()),
        );
    };
    // 挑中的 exe 落在产品自己的子目录里（QQ 电脑管家捆绑的 WeChatOCR 就是这种）
    if let Some(label) = non_game_dir_marker(exe) {
        return (
            WegameVerdict::NonGamePath,
            format!("跳过：可执行文件属于「{label}」（{}）", exe.display()),
        );
    }
    if let Some(label) = non_game_exe(exe) {
        return (
            WegameVerdict::NonGameExe,
            format!(
                "跳过：挑中的可执行文件 {} 是已知的非游戏程序（{label}）",
                exe.display()
            ),
        );
    }
    (
        WegameVerdict::Game,
        format!("收下：目录 {}，可执行文件 {}", dir.display(), exe.display()),
    )
}

/// WeGame 自己装没装。
///
/// 这个前提挡掉的是最要命的一类误报：本机装了 QQ，而 QQ 的注册表键**也在 Tencent
/// 下面**，它的数据指向 QQ 自己的安装目录，里头当然找得到 exe —— 于是被当成一条
/// 「WeGame 游戏」列出来了。装都没装 WeGame，就不可能有 WeGame 游戏。
pub fn wegame_installed() -> bool {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    for root in ["SOFTWARE\\WOW6432Node\\Tencent\\WeGame", "SOFTWARE\\Tencent\\WeGame"] {
        if hklm.open_subkey(root).is_ok() {
            return true;
        }
    }
    !wegame_install_bases().is_empty()
}

/// 扫描 WeGame 已安装的游戏。
///
/// **判定故意非常保守**，两个原因：
///   1. 这个注册表布局没有官方文档，是从社区资料推断的；
///   2. 开发机上没装 WeGame，**没法在真实环境验证**（其他平台都是拿真实机器验过的）。
///
/// 所以只把「里面真能找到游戏可执行文件的目录」当成一条记录 —— 宁可漏报，也不列一堆
/// 垃圾让用户困惑。扫不到不影响使用：界面上还能用「选择目录」手动指到渲染 EXE 的文件夹。
pub fn scan_wegame() -> Vec<GameEntry> {
    let mut notes = Vec::new();
    scan_wegame_notes(&mut notes)
}

pub fn scan_wegame_notes(notes: &mut Vec<String>) -> Vec<GameEntry> {
    // 没装 WeGame 就直接返回空 —— 别去 Tencent 键下面瞎猜
    if !wegame_installed() {
        note(
            notes,
            // 措辞要老实：我们只是**没找到**安装痕迹，不等于用户没装。
            // 写成确定的「本机没装」会让真装了 WeGame 的用户以为功能不支持，
            // 于是根本不会来反馈「扫不到」。
            "WeGame：没找到 WeGame 的安装痕迹（Tencent 注册表和 Program Files 下都没有），已跳过 ——              如果确实装了 WeGame，可以用「选择目录」把游戏手动加进库"
                .to_owned(),
        );
        return Vec::new();
    }
    let mut out: Vec<GameEntry> = Vec::new();
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let (mut keys, mut non_game, mut no_dir, mut no_exe, mut non_game_exe, mut non_game_path) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    // 每个候选都记一行证据（哪个键、哪个值、哪个目录、挑中哪个 exe、为什么这样判），
    // 用户报「选出无关软件」时看这几行就够。给个上限：畸形注册表不该把日志刷爆。
    const DETAIL_CAP: usize = 80;
    let mut detailed = 0usize;

    for root in WEGAME_REG_ROOTS {
        let Ok(k) = hklm.open_subkey(root) else { continue };
        for key_name in k.enum_keys().flatten() {
            keys += 1;
            let Ok(gk) = k.open_subkey(&key_name) else { continue };

            let mut dir: Option<PathBuf> = None;
            let mut dir_value: Option<String> = None;
            let mut weak: Option<PathBuf> = None;
            let mut weak_value: Option<String> = None;
            let mut display: Option<String> = None;

            for (vname, _) in gk.enum_values().flatten() {
                let Ok(s) = gk.get_value::<String, _>(&vname) else { continue };
                let s = s.trim();
                if s.is_empty() {
                    continue;
                }
                if wegame_value_is_name_like(&vname) && display.is_none() {
                    display = Some(s.to_owned());
                    continue;
                }
                if let Some(d) = wegame_existing_dir(s) {
                    if wegame_value_is_path_like(&vname) && dir.is_none() {
                        dir = Some(d);
                        dir_value = Some(vname.clone());
                    } else if weak.is_none() {
                        weak = Some(d);
                        weak_value = Some(vname.clone());
                    }
                }
            }

            // 名字像路径的值优先；没有就用任何一个能落到真实目录的值兜底
            let (chosen, chosen_value) = match (dir, weak) {
                (Some(d), _) => (Some(d), dir_value),
                (None, Some(d)) => (Some(d), weak_value),
                (None, None) => (None, None),
            };
            let render = chosen.as_deref().and_then(find_render_exe);
            let (verdict, why) = wegame_verdict(&key_name, chosen.as_deref(), render.as_deref());

            if detailed < DETAIL_CAP {
                detailed += 1;
                note(
                    notes,
                    format!(
                        "WeGame 候选 [{root}\\{key_name}] 判定={} 目录={} 来自值={} 可执行文件={} —— {}",
                        verdict.label(),
                        chosen
                            .as_deref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "无".to_owned()),
                        chosen_value.as_deref().unwrap_or("无"),
                        render
                            .as_deref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "无".to_owned()),
                        why
                    ),
                );
            }

            match verdict {
                WegameVerdict::Game => {
                    if let Some(d) = chosen {
                        let name = display
                            .filter(|n| !n.trim().is_empty())
                            .unwrap_or_else(|| wegame_name_from_key(&key_name));
                        push_wegame(&mut out, name, d);
                    }
                }
                WegameVerdict::NonGameKey => non_game += 1,
                WegameVerdict::NoDir => no_dir += 1,
                WegameVerdict::NoRenderExe => no_exe += 1,
                WegameVerdict::NonGameExe => non_game_exe += 1,
                WegameVerdict::NonGamePath => non_game_path += 1,
            }
        }
    }

    // WeGame 自己的安装目录下可能有 apps\ 结构，兜底再扫一遍
    for base in wegame_install_bases() {
        let Ok(rd) = std::fs::read_dir(base.join("apps")) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            // 应用客户端目录（播放器/聊天/管家）：直接排除，并**记录原因**，
            // 这样用户复制诊断时能直接看到「为什么没扫它 / 为什么扫出来了」。
            let lower_name = name.to_lowercase();
            if let Some(kw) = APP_DIR_MARKERS.iter().find(|k| lower_name.contains(*k)) {
                notes.push(format!("已排除 | {} | 目录名含 {kw}", name));
                continue;
            }
            let render = find_render_exe(&p);
            let (verdict, why) = wegame_verdict(&name, Some(&p), render.as_deref());
            if detailed < DETAIL_CAP {
                detailed += 1;
                note(
                    notes,
                    format!(
                        "WeGame apps 目录 [{}] 判定={} 可执行文件={} —— {}",
                        p.display(),
                        verdict.label(),
                        render
                            .as_deref()
                            .map(|x| x.display().to_string())
                            .unwrap_or_else(|| "无".to_owned()),
                        why
                    ),
                );
            }
            match verdict {
                WegameVerdict::Game => {
                    if !name.trim().is_empty() {
                        push_wegame(&mut out, name, p);
                    }
                }
                WegameVerdict::NonGameExe => non_game_exe += 1,
                WegameVerdict::NoRenderExe => no_exe += 1,
                _ => {}
            }
        }
    }

    note(
        notes,
        format!(
            "WeGame 扫描完成：看了 {keys} 个注册表项，收下 {} 个游戏，跳过 {}（非游戏键 {non_game}、没读到目录 {no_dir}、目录里找不到可执行文件 {no_exe}、可执行文件是非游戏程序 {non_game_exe}、装在非游戏产品目录里 {non_game_path}{}）",
            out.len(),
            non_game + no_dir + no_exe + non_game_exe + non_game_path,
            if keys > detailed { "；候选明细另有上限，只记了前 80 条" } else { "" }
        ),
    );
    out
}

/// 一个值里的字符串若指向已存在的目录（或者是文件、就取它所在的目录），返回它。
fn wegame_existing_dir(s: &str) -> Option<PathBuf> {
    let p = PathBuf::from(s);
    if p.is_dir() {
        return Some(p);
    }
    if p.is_file() {
        if let Some(parent) = p.parent() {
            if parent.is_dir() {
                return Some(parent.to_path_buf());
            }
        }
    }
    None
}

fn push_wegame(out: &mut Vec<GameEntry>, name: String, dir: PathBuf) {
    // 用归一化 key 比：同一个目录在注册表里的写法可能大小写/斜杠不同
    let key = path_key(&dir);
    if out.iter().any(|g| path_key(&g.install_dir) == key) {
        return;
    }
    out.push(GameEntry {
        source: Launcher::WeGame,
        app_id: String::new(),
        name,
        install_dir: dir,
    });
}

fn wegame_install_bases() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        let Ok(pf) = std::env::var(var) else { continue };
        for sub in WEGAME_INSTALL_SUBDIRS {
            let p = PathBuf::from(&pf).join(sub);
            if p.is_dir() {
                out.push(p);
            }
        }
    }
    out
}

/// 解析 PE 导入表，返回被导入的 DLL 名（全小写）。任何异常一律返回空表，不 panic。
pub fn pe_imports(path: &Path) -> Vec<String> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut f = BufReader::new(file);

    // DOS 头 -> e_lfanew
    let mut dos = [0u8; 64];
    if f.read_exact(&mut dos).is_err() || &dos[0..2] != b"MZ" {
        return Vec::new();
    }
    let e_lfanew = rd_u32(&dos, 0x3C) as u64;

    // PE 签名 + COFF 头
    if f.seek(SeekFrom::Start(e_lfanew)).is_err() {
        return Vec::new();
    }
    let mut coff = [0u8; 24];
    if f.read_exact(&mut coff).is_err() || &coff[0..4] != b"PE\0\0" {
        return Vec::new();
    }
    let num_sections = rd_u16(&coff, 6) as usize;
    let size_opt = rd_u16(&coff, 20) as usize;
    if size_opt == 0 || num_sections == 0 || num_sections > 96 {
        return Vec::new();
    }

    // OptionalHeader + 节表一起读进来
    let hdr_len = size_opt + num_sections * 40;
    let mut hdr = vec![0u8; hdr_len];
    if f.seek(SeekFrom::Start(e_lfanew + 24)).is_err() || f.read_exact(&mut hdr).is_err() {
        return Vec::new();
    }

    // DataDirectory[1] = Import Table。PE32+ 在偏移 112，PE32 在偏移 96。
    let dd_off = match rd_u16(&hdr, 0) {
        0x20B => 112,
        0x10B => 96,
        _ => return Vec::new(),
    };
    if dd_off + 16 > hdr.len() {
        return Vec::new();
    }
    let import_rva = rd_u32(&hdr, dd_off + 8);
    if import_rva == 0 {
        return Vec::new();
    }

    let secs = &hdr[size_opt..];
    let rva_to_off = |rva: u32| -> Option<u64> {
        for i in 0..num_sections {
            let s = &secs[i * 40..i * 40 + 40];
            let vsize = rd_u32(s, 8);
            let vaddr = rd_u32(s, 12);
            let raw_size = rd_u32(s, 16);
            let raw_ptr = rd_u32(s, 20);
            let span = vsize.max(raw_size);
            if rva >= vaddr && (rva - vaddr) < span {
                return Some(u64::from(raw_ptr) + u64::from(rva - vaddr));
            }
        }
        None
    };

    let mut out = collect_import_names(&mut f, &rva_to_off, import_rva, 20, 12);

    // 延迟导入表 DataDirectory[13]。很多游戏把大部分依赖放在这里，只读普通导入表会漏掉。
    // PUBG 的 TslGame.exe 普通表里只剩 1 条（psapi.dll），其余全在延迟表里。
    let delay_dd = dd_off + 13 * 8;
    if delay_dd + 8 <= hdr.len() {
        let delay_rva = rd_u32(&hdr, delay_dd);
        if delay_rva != 0 {
            if let Some(doff) = rva_to_off(delay_rva) {
                let mut attrs = [0u8; 4];
                let ok = f.seek(SeekFrom::Start(doff)).is_ok() && f.read_exact(&mut attrs).is_ok();
                // Attributes 位 0 = 1 表示描述符里的字段是 RVA；否则是老式 VA，跳过
                if ok && (rd_u32(&attrs, 0) & 1) == 1 {
                    let mut d = collect_import_names(&mut f, &rva_to_off, delay_rva, 32, 4);
                    out.append(&mut d);
                }
            }
        }
    }

    out.sort();
    out.dedup();
    out
}

/// 读一张导入描述符表，返回其中的 DLL 名。
/// entry_size / name_off 用来同时兼容普通导入表(20/12)和延迟导入表(32/4)。
fn collect_import_names<F>(
    f: &mut BufReader<std::fs::File>,
    rva_to_off: &F,
    dir_rva: u32,
    entry_size: usize,
    name_off: usize,
) -> Vec<String>
where
    F: Fn(u32) -> Option<u64>,
{
    let mut out = Vec::new();
    let Some(mut off) = rva_to_off(dir_rva) else {
        return out;
    };

    for _ in 0..512 {
        let mut d = vec![0u8; entry_size];
        if f.seek(SeekFrom::Start(off)).is_err() || f.read_exact(&mut d).is_err() {
            break;
        }
        // 整条全 0 即结束标记（比只看某个字段可靠）
        if d.iter().all(|&b| b == 0) {
            break;
        }
        let name_rva = rd_u32(&d, name_off);
        if name_rva != 0 {
            if let Some(no) = rva_to_off(name_rva) {
                if f.seek(SeekFrom::Start(no)).is_ok() {
                    let mut name = Vec::new();
                    let mut byte = [0u8; 1];
                    while name.len() < 260 {
                        match f.read_exact(&mut byte) {
                            Ok(()) if byte[0] != 0 => name.push(byte[0]),
                            _ => break,
                        }
                    }
                    if !name.is_empty() {
                        if let Ok(s) = std::str::from_utf8(&name) {
                            out.push(s.to_ascii_lowercase());
                        }
                    }
                }
            }
        }
        off += entry_size as u64;
    }
    out
}

// ---------------------------------------------------------------- 文件身份判定
//
// 用来回答「用户是不是已经手动装过本项目」。判据是 Authenticode 证书里的签名者名称，
// 这比文件名或体积可靠得多。
//
// 两个签名者都是本项目的，取决于上游用的是哪一版：
//   "DLSSG Native Project" —— 0.2.4 native 包（现在是 archive/0.2.4/）
//   "DLSSG for SM86"       —— 上游代理包（仓库根目录，现在对外发布的那一套）
// 上游换证书时如果只认旧名字，所有下载都会被自己拒掉。
//
// 注意：这里是在证书数据里找已知字符串，不做完整签名链校验。
// 对本项目够用 —— 自签证书本来就不受 Windows 信任，验链没有意义。

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileIdentity {
    /// 本项目文件（DLSSG Native Project 自签证书）
    ThisProject,
    /// NVIDIA 官方签名
    Nvidia,
    /// 有签名，但不是上面两种
    OtherSigned,
    /// 没有签名
    Unsigned,
    /// 读不出来
    Unknown,
}

impl FileIdentity {
    pub fn label(&self) -> &'static str {
        match self {
            FileIdentity::ThisProject => "本项目文件（作者自签证书）",
            FileIdentity::Nvidia => "NVIDIA 官方签名",
            FileIdentity::OtherSigned => "其他签名者",
            FileIdentity::Unsigned => "无签名",
            FileIdentity::Unknown => "无法识别",
        }
    }

    /// 是不是「我们自己的」文件 —— 是的话可以安全覆盖
    pub fn is_ours(&self) -> bool {
        matches!(self, FileIdentity::ThisProject)
    }
}

/// 0.2.4 native 包的签名者
const SIGNER_THIS_PROJECT: &str = "DLSSG Native Project";
/// 上游代理包的签名者（CN 全名 "DLSSG for SM86 (self-signed)"）
const SIGNER_THIS_PROJECT_PROXY: &str = "DLSSG for SM86";
const SIGNER_NVIDIA: &str = "NVIDIA Corporation";

/// 在证书数据里找字符串。DER 里通常是 ASCII，个别字段是 UTF-16LE，两种都找。
fn cert_contains(blob: &[u8], needle: &str) -> bool {
    let ascii = needle.as_bytes();
    if !ascii.is_empty() && blob.windows(ascii.len()).any(|w| w == ascii) {
        return true;
    }
    let utf16: Vec<u8> = needle.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
    !utf16.is_empty() && blob.windows(utf16.len()).any(|w| w == utf16)
}

/// 读 PE 的证书表（DataDirectory[4]）。
/// 这一项和别的目录项不同：它的「RVA」字段直接就是文件偏移，不走节表映射。
fn pe_certificate_blob(path: &Path) -> Vec<u8> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut f = BufReader::new(file);

    let mut dos = [0u8; 64];
    if f.read_exact(&mut dos).is_err() || &dos[0..2] != b"MZ" {
        return Vec::new();
    }
    let e_lfanew = rd_u32(&dos, 0x3C) as u64;

    let mut coff = [0u8; 24];
    if f.seek(SeekFrom::Start(e_lfanew)).is_err() || f.read_exact(&mut coff).is_err() {
        return Vec::new();
    }
    let size_opt = rd_u16(&coff, 20) as usize;
    if size_opt < 144 {
        return Vec::new();
    }

    let mut opt = vec![0u8; size_opt];
    if f.seek(SeekFrom::Start(e_lfanew + 24)).is_err() || f.read_exact(&mut opt).is_err() {
        return Vec::new();
    }

    let dd_off = match rd_u16(&opt, 0) {
        0x20B => 112,
        0x10B => 96,
        _ => return Vec::new(),
    };
    let sec_off = dd_off + 4 * 8; // DataDirectory[4] = Security
    if sec_off + 8 > opt.len() {
        return Vec::new();
    }
    let file_off = rd_u32(&opt, sec_off) as u64;
    let size = rd_u32(&opt, sec_off + 4) as usize;
    if file_off == 0 || size == 0 || size > 16 * 1024 * 1024 {
        return Vec::new();
    }

    let mut blob = vec![0u8; size];
    if f.seek(SeekFrom::Start(file_off)).is_err() || f.read_exact(&mut blob).is_err() {
        return Vec::new();
    }
    blob
}

/// 判断一个 DLL 的来源身份。任何异常都返回 Unknown，不 panic。
pub fn identify_dll(path: &Path) -> FileIdentity {
    let blob = pe_certificate_blob(path);
    if blob.is_empty() {
        return match std::fs::metadata(path) {
            Ok(m) if m.len() > 1024 => FileIdentity::Unsigned,
            _ => FileIdentity::Unknown,
        };
    }
    if cert_contains(&blob, SIGNER_THIS_PROJECT) || cert_contains(&blob, SIGNER_THIS_PROJECT_PROXY)
    {
        return FileIdentity::ThisProject;
    }
    if cert_contains(&blob, SIGNER_NVIDIA) {
        return FileIdentity::Nvidia;
    }
    FileIdentity::OtherSigned
}

// ---------------------------------------------------------------- 代理入口推断

/// **所有代理入口名字 —— 全项目只有这一处硬编码。**
///
/// 顺序 = 上游推荐顺序：前 6 个是 alternatives/ 下现用的入口，
/// 最后一个是上游历史上用过、现在只剩归档包（archive/0.2.4/altnative/）里才有的名字。
/// 别处（deploy / importer / 界面）一律引用这里，免得上游改名时漏改一处。
/// version.dll 是上游默认；dbghelp / d3d12 是 0.3.0 新增的，winhttp 已被上游删掉。
/// 0.3.1 起 20 系和 30 系用同一套文件，所以只有这一份清单。
pub const PROXY_ALL: [&str; 7] = [
    "version.dll",
    "winmm.dll",
    "dbghelp.dll",
    "dinput8.dll",
    "dxgi.dll",
    "d3d12.dll",
    "winhttp.dll",
];

/// 当前版支持的代理入口，按上游推荐顺序 —— 就是 PROXY_ALL 的前 6 个。
/// （下标只能逐个写死：常量里不能做切片，`&ARR[..n]` 要用还没稳定的 Index trait。）
pub const PROXY_PRIORITY: [&str; 6] = [
    PROXY_ALL[0],
    PROXY_ALL[1],
    PROXY_ALL[2],
    PROXY_ALL[3],
    PROXY_ALL[4],
    PROXY_ALL[5],
];

/// 上游历史上用过的入口名（现在只有老归档包里才有）。
/// 判断「要不要拒绝覆盖」「要不要清理多余代理」时老名字也得认，
/// 否则用户从老版切过来时，目录里残留的 winhttp.dll 会被当成第三方文件。
pub const PROXY_HISTORIC: [&str; 1] = [PROXY_ALL[6]];

/// 某个文件名是不是代理入口（现用清单 + 历史上的名字）。
pub fn is_known_proxy(name: &str) -> bool {
    PROXY_PRIORITY.contains(&name) || PROXY_HISTORIC.contains(&name)
}

/// 判断入口时最多分析多少个模块，避免在大游戏目录上卡住
const PROXY_SCAN_MAX: usize = 60;

/// 目标目录里已经存在的一个入口文件
#[derive(Debug, Clone)]
pub struct ExistingEntry {
    pub name: String,
    pub bytes: u64,
    pub identity: FileIdentity,
}

#[derive(Debug, Clone)]
pub struct ProxyAdvice {
    pub recommended: String,
    /// 人类可读的判断依据
    pub reason: String,
    /// 已存在、但**不是**本项目文件的入口（会被跳过）
    pub occupied: Vec<ExistingEntry>,
    /// 已存在、**是**本项目文件的入口（可以安全覆盖，也就是用户之前手动装过的）
    pub own_existing: Vec<ExistingEntry>,
    /// true = 没能自动判定，用的是上游默认
    pub undetermined: bool,
    pub scanned: usize,
}

/// 判断该用哪个代理入口。
///
/// 原理：Windows 加载 DLL 时优先搜索 EXE 所在目录，所以只要游戏会加载的某个模块
/// 导入了 version.dll，把代理放进这个目录就会被加载。判据是**导入表**，不是游戏名字。
///
/// 三级：先排除被占用的 -> 再按导入表匹配 -> 都判不出来就用上游默认并标 undetermined。
pub fn advise_proxy(target_dir: &Path) -> ProxyAdvice {
    let cands: &[&str] = &PROXY_PRIORITY;
    // 先看目录里已经有哪些入口，并判断它们的来源身份。
    // 这一步同时回答了「用户是不是已经手动装过本项目」。
    let mut occupied: Vec<ExistingEntry> = Vec::new();
    let mut own_existing: Vec<ExistingEntry> = Vec::new();
    for &name in cands {
        let p = target_dir.join(name);
        if !p.is_file() {
            continue;
        }
        let entry = ExistingEntry {
            name: name.to_owned(),
            bytes: std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0),
            identity: identify_dll(&p),
        };
        if entry.identity.is_ours() {
            own_existing.push(entry);
        } else {
            occupied.push(entry);
        }
    }
    let is_occupied = |n: &str| occupied.iter().any(|o| o.name == n);

    // 已经装过本项目的话，直接复用已有入口 —— 比重新挑一个更合理，
    // 否则目录里会留下两个代理，上游 README 明确不建议这样。
    if let Some(first) = own_existing.first() {
        return ProxyAdvice {
            recommended: first.name.clone(),
            reason: format!(
                "目录里已经有本项目的 {}，直接复用它，避免同时存在两个代理",
                first.name
            ),
            occupied,
            own_existing,
            undetermined: false,
            scanned: 0,
        };
    }

    // 收集要分析的模块：目标目录 + 一层子目录里的 exe 与 dll
    let mut files: Vec<PathBuf> = Vec::new();
    let mut dirs = vec![target_dir.to_path_buf()];
    if let Ok(rd) = std::fs::read_dir(target_dir) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                dirs.push(e.path());
            }
        }
    }

    'outer: for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let is_module = p
                .extension()
                .map(|x| {
                    let s = x.to_string_lossy().to_ascii_lowercase();
                    s == "dll" || s == "exe"
                })
                .unwrap_or(false);
            if is_module {
                files.push(p);
                if files.len() >= PROXY_SCAN_MAX {
                    break 'outer;
                }
            }
        }
    }

    // 汇总：哪个候选名被哪些模块导入
    let mut hits: Vec<(String, String)> = Vec::new();
    for f in &files {
        let who = f
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let imports = pe_imports(f);
        for &cand in cands {
            if imports.iter().any(|i| i == cand) {
                hits.push((cand.to_owned(), who.clone()));
            }
        }
    }

    // 按优先级挑第一个「没被占用且被导入」的
    for &cand in cands {
        if is_occupied(cand) {
            continue;
        }
        let mut by: Vec<String> = hits
            .iter()
            .filter(|(c, _)| c == cand)
            .map(|(_, w)| w.clone())
            .collect();
        if !by.is_empty() {
            by.sort();
            by.dedup();
            return ProxyAdvice {
                recommended: cand.to_owned(),
                reason: format!("{} 会加载它", by.join("、")),
                occupied,
                own_existing: Vec::new(),
                undetermined: false,
                scanned: files.len(),
            };
        }
    }

    // 判不出来 -> 回退到第一个没被占用的入口，并标记 undetermined
    let fallback = cands
        .iter()
        .copied()
        .find(|n| !is_occupied(n))
        .unwrap_or(cands[0])
        .to_owned();
    let reason = if files.len() <= 1 {
        "这个目录里几乎没有可执行文件，可能不是渲染 EXE 所在目录".to_owned()
    } else {
        format!("扫了 {} 个模块，没有发现哪个候选入口被导入", files.len())
    };
    ProxyAdvice {
        recommended: fallback,
        reason,
        occupied,
        own_existing: Vec::new(),
        undetermined: true,
        scanned: files.len(),
    }
}

// ---------------------------------------------------------------- 图形 API 与游戏引擎

/// 渲染 EXE 实际用的图形 API。
///
/// 对用户来说这不是「冷知识」：**DLSS 帧生成只在 DX12 / Vulkan 下存在**。
/// DX11 及更早的游戏装了这个 Mod 也不会有任何效果，早点说清楚能省一次白忙。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GraphicsApi {
    #[default]
    Unknown,
    Dx12,
    Dx11,
    Dx10,
    Dx9,
    Vulkan,
    OpenGl,
}

impl GraphicsApi {
    pub fn label(self) -> &'static str {
        match self {
            GraphicsApi::Dx12 => "DX12",
            GraphicsApi::Dx11 => "DX11",
            GraphicsApi::Dx10 => "DX10",
            GraphicsApi::Dx9 => "DX9",
            GraphicsApi::Vulkan => "Vulkan",
            GraphicsApi::OpenGl => "OpenGL",
            // 只写「未知」：日志里是 "API=未知"、徽章里是 "图形 API：未知"，',
            // 自带前缀会变成「图形 API：图形 API 未知」（截图里出现过）。
            GraphicsApi::Unknown => "未知",
        }
    }

    /// 这个 API 下帧生成有没有意义。None = 认不出来（别说死）。
    pub fn frame_gen_possible(self) -> Option<bool> {
        match self {
            GraphicsApi::Dx12 | GraphicsApi::Vulkan => Some(true),
            GraphicsApi::Unknown => None,
            _ => Some(false),
        }
    }
}

/// 游戏引擎。只报**有明确证据**的；认不出来就是 Unknown —— 不猜。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GameEngine {
    #[default]
    Unknown,
    Unreal4,
    Unreal5,
    Unity,
    Source2,
    Source,
    Creation,
    ReEngine,
    CryEngine,
    /// GTA / 荒野大镖客：资源是 .rpf
    Rage,
    /// 刺客信条：资源是 .forge
    Anvil,
    /// 战地 / FIFA：Data 目录下的 .cas/.sb/.toc
    Frostbite,
    Godot,
    GameMaker,
    RpgMaker,
    MonoGame,
}

impl GameEngine {
    /// 徽章里用的短名字：徽章要带标签，太长会把整行撑开。
    pub fn short(self) -> &'static str {
        match self {
            GameEngine::Unreal4 => "UE4",
            GameEngine::Unreal5 => "UE5",
            GameEngine::Unity => "Unity",
            GameEngine::Source2 => "Source 2",
            GameEngine::Source => "Source",
            GameEngine::Creation => "Creation",
            GameEngine::ReEngine => "RE Engine",
            GameEngine::CryEngine => "CryEngine",
            GameEngine::Rage => "RAGE",
            GameEngine::Anvil => "Anvil",
            GameEngine::Frostbite => "Frostbite",
            GameEngine::Godot => "Godot",
            GameEngine::GameMaker => "GameMaker",
            GameEngine::RpgMaker => "RPG Maker",
            GameEngine::MonoGame => "MonoGame",
            GameEngine::Unknown => "未知",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GameEngine::Unreal4 => "Unreal Engine 4",
            GameEngine::Unreal5 => "Unreal Engine 5",
            GameEngine::Unity => "Unity",
            GameEngine::Source2 => "Source 2",
            GameEngine::Source => "Source",
            GameEngine::Creation => "Creation Engine",
            GameEngine::ReEngine => "RE Engine",
            GameEngine::CryEngine => "CryEngine",
            GameEngine::Rage => "RAGE",
            GameEngine::Anvil => "Anvil",
            GameEngine::Frostbite => "Frostbite",
            GameEngine::Godot => "Godot",
            GameEngine::GameMaker => "GameMaker",
            GameEngine::RpgMaker => "RPG Maker",
            GameEngine::MonoGame => "MonoGame / XNA",
            GameEngine::Unknown => "引擎未知",
        }
    }
}

/// 判定引擎时用到的**观测**。纯数据，所以能用构造数据测（不用真装游戏）。
#[derive(Debug, Clone, Default)]
pub struct LayoutFacts {
    /// 渲染 EXE 的文件名（小写）
    pub exe_name: String,
    /// 渲染 EXE 同目录的文件名（小写）
    pub siblings: Vec<String>,
    /// 往上找 `<根>\Content\Paks` 时看到的扩展名（小写，如 utoc / pak）
    pub pak_kinds: Vec<String>,
    /// 游戏根目录（以及它下面的 Data 目录）里出现过的扩展名（小写、去重）
    pub root_exts: Vec<String>,
}

/// 判定图形 API 的全部观测。纯数据，好用构造数据测。
///
/// 参考了 DLSS5-Swapper（MIT）的思路：**光看导入表不够**。有的游戏一个渲染器一个 exe
/// （`farcry3_d3d11.exe`），有的游戏旁边那个 `dxgi.dll` 其实是 DXVK（把 Direct3D 调用
/// 转成 Vulkan，真正呈现的是 Vulkan）。这两类用导入表都判不出来。
pub struct ApiFacts<'a> {
    /// 渲染 EXE 的文件名
    pub exe_name: &'a str,
    /// 它导入的 DLL 名（全小写）
    pub imports: &'a [String],
    /// 同目录的文件名（全小写）
    pub siblings: &'a [String],
    /// 同目录那个 d3d*/dxgi 代理 DLL 自己又导入了 vulkan-1.dll = DXVK/vkd3d 包装
    pub wrapper_is_vulkan: bool,
}

/// 从 exe 名里认渲染器：`*_d3d11.exe` / `*-vulkan.exe` 这类命名很常见。
fn api_from_exe_name(name: &str) -> Option<GraphicsApi> {
    let n = name.to_ascii_lowercase();
    // 按分隔符切成词，避免 `d3d11` 之类的子串在别的词里误命中
    let toks: Vec<String> = n
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect();
    let has = |names: &[&str]| toks.iter().any(|t| names.contains(&t.as_str()));
    if has(&["d3d12", "dx12"]) {
        return Some(GraphicsApi::Dx12);
    }
    if has(&["vulkan", "vk"]) {
        return Some(GraphicsApi::Vulkan);
    }
    if has(&["d3d11", "dx11"]) {
        return Some(GraphicsApi::Dx11);
    }
    if has(&["d3d10", "dx10"]) {
        return Some(GraphicsApi::Dx10);
    }
    if has(&["d3d9", "dx9"]) {
        return Some(GraphicsApi::Dx9);
    }
    if has(&["opengl", "gl"]) {
        return Some(GraphicsApi::OpenGl);
    }
    None
}

/// 综合全部证据判图形 API。证据强弱：包装器 > exe 名 > 导入表 > 同目录运行库。
pub fn api_from_facts(f: &ApiFacts<'_>) -> GraphicsApi {
    // 包装器最优先：它决定了**最终**是谁在呈现画面（DXVK → Vulkan）
    if f.wrapper_is_vulkan {
        return GraphicsApi::Vulkan;
    }
    if let Some(a) = api_from_exe_name(f.exe_name) {
        return a;
    }
    api_from_names(f.imports, f.siblings)
}

/// 从导入表 + 同目录文件判断图形 API。
///
/// 优先级说明：DX12 优先于 Vulkan —— 同时导入两者的游戏（很少见）用 DX12 跑是常态。
/// dxgi.dll 单独出现不算数（10/11/12 都会用它），必须有具体的 d3d*.dll。
pub fn api_from_names(imports: &[String], siblings: &[String]) -> GraphicsApi {
    let has = |list: &[String], n: &str| list.iter().any(|s| s.eq_ignore_ascii_case(n));
    if has(imports, "d3d12.dll") || has(imports, "d3d12core.dll") {
        return GraphicsApi::Dx12;
    }
    if has(imports, "vulkan-1.dll") {
        return GraphicsApi::Vulkan;
    }
    if has(imports, "d3d11.dll") {
        return GraphicsApi::Dx11;
    }
    if has(imports, "d3d10.dll") || has(imports, "d3d10_1.dll") {
        return GraphicsApi::Dx10;
    }
    if has(imports, "d3d9.dll") {
        return GraphicsApi::Dx9;
    }
    if has(imports, "opengl32.dll") {
        return GraphicsApi::OpenGl;
    }
    // 导入表为空多半是「运行时才 LoadLibrary」的游戏：退一步看它把哪个运行库
    // 放在自己旁边（Vulkan 游戏常自带 vulkan-1.dll）。仍然只是弱证据。
    if has(siblings, "vulkan-1.dll") {
        return GraphicsApi::Vulkan;
    }
    if has(siblings, "d3d12.dll") {
        return GraphicsApi::Dx12;
    }
    GraphicsApi::Unknown
}

/// 从「EXE 名 + 目录长相」判断引擎。每条规则都用**游戏自己带的文件名**做证据。
pub fn engine_from_facts(f: &LayoutFacts) -> GameEngine {
    // 比较一律不区分大小写：调用方已经统一小写了，但这个纯函数不该因为
    // 有人传了 "UnityPlayer.dll" 就判不出来（自测就是这么抓到的）。
    let sib = |n: &str| f.siblings.iter().any(|s| s.eq_ignore_ascii_case(n));
    let sib_has = |part: &str| f.siblings.iter().any(|s| s.to_ascii_lowercase().contains(part));
    let exe = f.exe_name.to_ascii_lowercase();
    // Unreal：<项目名>-Win64-Shipping.exe 是引擎自己的命名约定，基本不会误判。
    // 4 还是 5 只能靠 IoStore：UE5 默认把资源打成 .utoc/.ucas。
    if exe.ends_with("-win64-shipping.exe") || exe.ends_with("-win32-shipping.exe") {
        return if f.pak_kinds.iter().any(|k| k == "utoc") {
            GameEngine::Unreal5
        } else {
            GameEngine::Unreal4
        };
    }
    if sib("unityplayer.dll") || f.siblings.iter().any(|s| s.to_ascii_lowercase().ends_with("_data")) {
        return GameEngine::Unity;
    }
    if sib("engine2.dll") {
        return GameEngine::Source2;
    }
    if sib("engine.dll") && sib("vstdlib.dll") {
        return GameEngine::Source;
    }
    if sib("crysystem.dll") {
        return GameEngine::CryEngine;
    }
    if sib_has("re_chunk_") {
        return GameEngine::ReEngine;
    }
    if sib("data.win") {
        return GameEngine::GameMaker;
    }
    if sib("nw.dll") || sib("rgss301.dll") || sib("rgss300.dll") {
        return GameEngine::RpgMaker;
    }
    if sib("monogame.framework.dll") || f.siblings.iter().any(|s| s.to_ascii_lowercase().starts_with("xna")) {
        return GameEngine::MonoGame;
    }
    if f.siblings.iter().any(|s| s.to_ascii_lowercase().ends_with(".pck")) || sib_has("godot.windows") {
        return GameEngine::Godot;
    }
    // Creation Engine（Skyrim / Fallout）：资源是 Data 目录下的 .ba2 / .bsa，
    // 而 EXE 旁边通常能看到 Data 这个目录名。
    // 下面这几条靠「资源文件的扩展名」认 —— 那些扩展名基本只有一家在用，
    // 但仍然只用证据说话：拿不准的（比如 .pak 谁都用）一律不列。
    let ext = |e: &str| f.root_exts.iter().any(|x| x == e);
    if ext("rpf") {
        return GameEngine::Rage;
    }
    if ext("forge") {
        return GameEngine::Anvil;
    }
    // Frostbite：.cas / .sb / .toc 三选二才算（单个都可能在别家出现）
    let frost = ["cas", "sb", "toc"].iter().filter(|e| ext(e)).count();
    if frost >= 2 {
        return GameEngine::Frostbite;
    }
    // Creation Engine（Skyrim / Fallout / Starfield）：.ba2 / .bsa 是它独有的
    if ext("ba2") || ext("bsa") {
        return GameEngine::Creation;
    }
    GameEngine::Unknown
}

/// 在一些**固定的候选路径**里找 Streamline 的痕迹（有界，不做全盘搜索）。
///
/// 为什么需要：Streamline 不一定躺在渲染 EXE 旁边 —— UE 游戏常把它放在
/// `<根>\Engine\Binaries\ThirdParty\NVIDIA\DLSS\` 下面。只看 exe 同目录会把
/// 「其实自带帧生成」的游戏误判成「安装无效」，而那是个很重的结论。
fn find_streamline_marker(exe: &Path) -> Option<String> {
    let dir = exe.parent()?;
    let mut cur = Some(dir);
    // 向上 5 层：渲染 exe 通常在 <游戏根>\<项目名>\Binaries\Win64\，要走到 <游戏根>
    // 才看得见引擎目录 —— 以前只走 3 层，正好差一层（实测「霍格沃茨之遗」就卡在这里）。
    for _ in 0..5 {
        let Some(d) = cur else { break };
        let mut candidates = vec![
            d.to_path_buf(),
            d.join("Binaries").join("Win64"),
            d.join("Engine")
                .join("Binaries")
                .join("ThirdParty")
                .join("NVIDIA")
                .join("DLSS"),
            d.join("Engine")
                .join("Binaries")
                .join("ThirdParty")
                .join("NVIDIA"),
            // UE 插件布局（霍格沃茨之遗、以及不少 UE4/5 游戏就是这一套）：
            //   <根>\Engine\Plugins\Runtime\Nvidia\DLSS\Binaries\ThirdParty\Win64
            //   <根>\Engine\Plugins\Runtime\Nvidia\Streamline\Binaries\ThirdParty\Win64
            // Windows 文件系统不区分大小写，所以 Nvidia/NVIDIA 写哪个都行。
            d.join("Engine")
                .join("Plugins")
                .join("Runtime")
                .join("Nvidia")
                .join("DLSS")
                .join("Binaries")
                .join("ThirdParty")
                .join("Win64"),
            d.join("Engine")
                .join("Plugins")
                .join("Runtime")
                .join("Nvidia")
                .join("Streamline")
                .join("Binaries")
                .join("ThirdParty")
                .join("Win64"),
        ];
        // 插件目录名各家不同（Nvidia / NVIDIA / DLSS / Streamline / 自建名），
        // 所以再扫一层 Engine\Plugins\Runtime\*\Binaries\ThirdParty\Win64 兜底。
        // 只列一层目录，条目很少，代价可忽略。
        let runtime = d.join("Engine").join("Plugins").join("Runtime");
        if let Ok(rd) = std::fs::read_dir(&runtime) {
            for e in rd.flatten().take(40) {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    candidates.push(
                        e.path()
                            .join("Binaries")
                            .join("ThirdParty")
                            .join("Win64"),
                    );
                    candidates.push(e.path());
                }
            }
        }
        for probe in candidates {
            let Ok(rd) = std::fs::read_dir(&probe) else {
                continue;
            };
            for e in rd.flatten().take(400) {
                let n = e.file_name().to_string_lossy().to_lowercase();
                if n.contains("sl.interposer")
                    || n.contains("sl.dlss_g")
                    || n.contains("nvngx_dlssg")
                {
                    // 拼路径用 join，免得在格式化字符串里写反斜杠
                    return Some(probe.join(&n).display().to_string());
                }
            }
        }
        cur = d.parent();
    }
    None
}

/// 一次判定的完整结果，含**给人看的判定依据**（日志里要写清楚，便于后续开发）。
#[derive(Debug, Clone)]
pub struct TechReport {
    pub api: GraphicsApi,
    pub engine: GameEngine,
    pub streamline: bool,
    /// 判定依据：看过什么、命中了什么。写给日志，不是给界面。
    pub evidence: Vec<String>,
}

/// 一次把三件事算出来：图形 API、引擎、以及**是否自带 Streamline（DLSS 帧生成）**。
///
/// 最后一项对本工具最关键：上游 Mod 要求游戏自带 DLSS 帧生成（Streamline），
/// 没有它装了也不会生效 —— 这比「引擎是什么」更有用。
pub fn detect_tech(exe: &Path) -> (GraphicsApi, GameEngine, bool) {
    let r = detect_tech_report(exe);
    (r.api, r.engine, r.streamline)
}

/// 和 detect_tech 一样，但把**判定依据**一起带回来（写日志用）。
pub fn detect_tech_report(exe: &Path) -> TechReport {
    let imports = pe_imports(exe);
    let mut siblings: Vec<String> = Vec::new();
    if let Some(dir) = exe.parent() {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten().take(600) {
                siblings.push(e.file_name().to_string_lossy().to_lowercase());
            }
        }
    }
    let mut evidence: Vec<String> = Vec::new();
    evidence.push(format!("导入表 {} 项", imports.len()));
    let exe_name = exe
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // DXVK / vkd3d：同目录的 d3d*/dxgi 代理 DLL 自己导入了 vulkan-1.dll，
    // 说明 Direct3D 调用被转成了 Vulkan —— 实际呈现的是 Vulkan，报 DX12 是错的。
    let wrapper_is_vulkan = ["dxgi.dll", "d3d12.dll", "d3d11.dll", "d3d9.dll"]
        .iter()
        .filter(|n| siblings.iter().any(|s| s == *n))
        .any(|n| {
            exe.parent()
                .map(|d| pe_imports(&d.join(n)).iter().any(|i| i == "vulkan-1.dll"))
                .unwrap_or(false)
        });
    let mut api = api_from_facts(&ApiFacts {
        exe_name: &exe_name,
        imports: &imports,
        siblings: &siblings,
        wrapper_is_vulkan,
    });
    if wrapper_is_vulkan {
        evidence.push("同目录的 Direct3D DLL 是 DXVK/vkd3d 包装 -> 实际走 Vulkan".to_owned());
    } else if api != GraphicsApi::Unknown {
        evidence.push(format!("判定命中 -> {}", api.label()));
    }
    // 很多引擎（Unity 最典型）的游戏 exe 只是个壳：真正的渲染在引擎 DLL 里，
    // 所以主 exe 的导入表里根本没有 d3d*.dll。这时去看引擎 DLL 自己的导入表 ——
    // 实测一个 Unity 游戏：主 exe 0.6 MB（导入表没提 API），UnityPlayer.dll 里才有。
    if api == GraphicsApi::Unknown {
        for name in ["unityplayer.dll", "engine2.dll", "engine.dll", "crysystem.dll"] {
            if !siblings.iter().any(|x| x == name) {
                continue;
            }
            let Some(dir) = exe.parent() else { break };
            let dep = pe_imports(&dir.join(name));
            api = api_from_names(&dep, &siblings);
            if api != GraphicsApi::Unknown {
                evidence.push(format!("{name} 的导入表命中 -> {}", api.label()));
                break;
            }
        }
    }
    if api == GraphicsApi::Unknown {
        evidence.push("导入表里没有 d3d*.dll / vulkan-1.dll".to_owned());
    }
    // UE5 的 IoStore：<根>\Content\Paks 下的 .utoc。往上看最多三层找它。
    let mut pak_kinds: Vec<String> = Vec::new();
    if let Some(dir) = exe.parent() {
        let mut cur = Some(dir);
        for _ in 0..3 {
            let Some(d) = cur else { break };
            if let Ok(rd) = std::fs::read_dir(d.join("Content").join("Paks")) {
                for e in rd.flatten().take(200) {
                    let n = e.file_name().to_string_lossy().to_lowercase();
                    if let Some(ext) = n.rsplit('.').next() {
                        if ext != n {
                            pak_kinds.push(ext.to_owned());
                        }
                    }
                }
            }
            cur = d.parent();
        }
    }
    // 资源扩展名：游戏根目录 + 它的 Data 目录。用来认那些「靠资源格式说话」的引擎
    // （.rpf = RAGE、.forge = Anvil、.ba2/.bsa = Creation…）。只看扩展名，不读内容。
    let mut root_exts: Vec<String> = Vec::new();
    if let Some(dir) = exe.parent() {
        let mut cur = Some(dir);
        for _ in 0..2 {
            let Some(d) = cur else { break };
            for sub in [d.to_path_buf(), d.join("Data")] {
                if let Ok(rd) = std::fs::read_dir(&sub) {
                    for e in rd.flatten().take(500) {
                        let n = e.file_name().to_string_lossy().to_lowercase();
                        if let Some((_, ext)) = n.rsplit_once('.') {
                            if !ext.is_empty() && !root_exts.iter().any(|x| x == ext) {
                                root_exts.push(ext.to_owned());
                            }
                        }
                    }
                }
            }
            cur = d.parent();
        }
    }
    let facts = LayoutFacts {
        exe_name: exe
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default(),
        siblings: siblings.clone(),
        pak_kinds,
        root_exts: root_exts.clone(),
    };
    let engine = engine_from_facts(&facts);
    evidence.push(format!(
        "同目录 {} 个文件、根目录扩展名 {} 种",
        siblings.len(),
        root_exts.len()
    ));
    let mut relevant: Vec<&str> = Vec::new();
    for x in &siblings {
        if x.contains("unityplayer")
            || x.starts_with("engine")
            || x.contains("crysystem")
            || x.contains("re_chunk")
            || x.ends_with("_data")
        {
            relevant.push(x);
        }
    }
    if !relevant.is_empty() {
        evidence.push(format!("同目录可疑文件 {:?}", &relevant[..relevant.len().min(8)]));
    }
    let streamline = imports.iter().any(|i| {
        i.contains("nvngx_dlssg") || i.contains("sl.interposer") || i.contains("sl.dlss_g")
    });
    // Streamline 也可能由引擎 DLL 带进来（游戏 exe 不直接导入它），一起看。
    let mut streamline = streamline
        || siblings.iter().any(|x| {
            x.contains("sl.interposer") || x.contains("sl.dlss_g") || x.contains("nvngx_dlssg")
        });
    if !streamline {
        if let Some(hit) = find_streamline_marker(exe) {
            streamline = true;
            evidence.push(format!("在 {hit} 找到 Streamline 痕迹"));
        }
    }
    if streamline {
        evidence.push("发现 Streamline / nvngx_dlssg 的痕迹".to_owned());
    } else {
        evidence.push("没找到 Streamline 痕迹（导入表 + exe 同目录 + 几个固定候选路径）".to_owned());
    }
    let engine_ev = if engine == GameEngine::Unknown {
        "没找到任何引擎特征（按约定报未知，不猜）".to_owned()
    } else {
        format!("命中 -> {}", engine.label())
    };
    evidence.push(format!("引擎判定：{engine_ev}"));
    TechReport {
        api,
        engine,
        streamline,
        evidence,
    }
}

// ---------------------------------------------------------------- 显卡识别

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuRoute {
    /// RTX 30 系：保持上游默认 Router=SM86
    Sm86,
    /// RTX 20 系：必须改成 Router=SM75
    Sm75,
    /// GTX 16 系：和 RTX 20 系同是 Turing，但**没有 Tensor Core** —— DLSS 全系功能在
    /// 硬件上就跑不了，换哪个版本都没用。单独一条路，并且禁止部署。
    Gtx16,
    /// Pascal 及更早（GTX 10 系等）、MX、Quadro/Tesla：同样没有 Tensor Core，
    /// 部署了也不会生效。以前这些都落到 Unknown 而被静默放行。
    NoTensorCore,
    /// RTX 40 / 50 系：原生支持帧生成，不需要本 Mod
    NotNeeded,
    /// AMD / Intel / 核显：不适用
    Unsupported,
    /// 读不到或认不出来
    Unknown,
}

impl GpuRoute {
    pub fn label(self) -> &'static str {
        match self {
            GpuRoute::Sm86 => "RTX 30 系 (SM86)",
            GpuRoute::Sm75 => "RTX 20 系 (SM75)",
            GpuRoute::Gtx16 => "GTX 16 系（无 Tensor Core）",
            GpuRoute::NoTensorCore => "不支持帧生成的旧卡（无 Tensor Core）",
            GpuRoute::NotNeeded => "RTX 40 / 50 系",
            GpuRoute::Unsupported => "非 NVIDIA 显卡",
            GpuRoute::Unknown => "未能识别",
        }
    }

    /// 稳定标识：界面拿它做判断，**别用 label 的文案去比**（文案会改）。
    pub fn key(self) -> &'static str {
        match self {
            GpuRoute::Sm86 => "sm86",
            GpuRoute::Sm75 => "sm75",
            GpuRoute::Gtx16 => "gtx16",
            GpuRoute::NoTensorCore => "no_tensor_core",
            GpuRoute::NotNeeded => "not_needed",
            GpuRoute::Unsupported => "unsupported",
            GpuRoute::Unknown => "unknown",
        }
    }

    /// **部署前的显卡闸门**：返回 Some(理由) = 禁止部署，None = 放行。
    ///
    /// 为什么要拦「装了半天不生效」的那几张卡：GTX 16 系 / Pascal 及更早 / MX /
    /// Quadro 都没有 Tensor Core，DLSS 帧生成在硬件上就跑不了，换哪个版本都没用；
    /// RTX 40/50 系原生支持，装上是多余；非 NVIDIA 卡连驱动接口都不对。
    ///
    /// 这段规则原本只写在 egui 版的界面代码里（那里叫 gpu_gate）。搬到 core 之后
    /// 两个界面调的是同一份 —— 否则 Tauri 版要么再抄一遍、要么就完全不拦，
    /// 而「能不能装」恰恰是最不该两边不一致的一条。
    pub fn gate(self) -> Option<&'static str> {
        match self {
            GpuRoute::Unsupported => {
                Some("检测到非 NVIDIA 显卡，本 Mod 完全不适用（需要 NVIDIA 驱动接口）")
            }
            GpuRoute::NotNeeded => Some("RTX 40/50 系原生支持 DLSS 帧生成，不需要装本 Mod"),
            GpuRoute::Gtx16 => Some(
                "GTX 16 系没有 Tensor Core，DLSS 帧生成在硬件上就不支持（换哪个版本都没用）",
            ),
            GpuRoute::NoTensorCore => Some(
                "这块显卡没有 Tensor Core（Pascal 及更早 / MX / Quadro），DLSS 帧生成在硬件上就不支持",
            ),
            _ => None,
        }
    }
}

/// 从「在位的适配器 + nvidia-smi 结果」里挑出最终使用的型号名。
///
/// 抽成纯函数是为了能单独测：本机只有一块显卡，多卡、以及"类键里有旧显卡留下的
/// 幽灵条目"这两种真实情况，在开发机上复现不出来。
///
/// 优先级：
///   1. nvidia-smi 报的型号 —— 驱动自己在跑的那块卡上直接报的，最权威
///   2. 在位适配器里，能识别成 RTX / GTX 的优先（认不出来的排后面，比如 GT 1030）
///   3. 同样能识别的，按驱动版本从新到旧
pub fn pick_gpu_name(
    adapters: &[crate::gpu::GpuAdapter],
    smi_name: Option<String>,
) -> Option<String> {
    if let Some(n) = smi_name.filter(|s| !s.trim().is_empty()) {
        return Some(n);
    }
    let mut list: Vec<&crate::gpu::GpuAdapter> = adapters.iter().collect();
    list.sort_by(|a, b| {
        let ua = i32::from(classify_gpu(&a.driver_name) == GpuRoute::Unknown);
        let ub = i32::from(classify_gpu(&b.driver_name) == GpuRoute::Unknown);
        ua.cmp(&ub).then_with(|| b.driver_version.cmp(&a.driver_version))
    });
    list.first().map(|a| a.driver_name.clone())
}

/// 读显卡型号名。
///
/// **不自己遍历显示适配器类键。** 「取第一个名字带 NVIDIA 的条目」这种做法不可靠，
/// 因为那个键下面可能有：
///   * 旧显卡留下的**幽灵条目**（换过卡就会有）；
///   * 被别的工具改过的值（网上"解锁帧生成"的教程就会改 DriverDesc）。
///
/// 于是同一个用户会出现「这次识别成 1030、退出再进又变成 40 系」—— 因为每次谁先被
/// 枚举到不一定一样。而 40 系的名字会让路由判定变成「不需要本 Mod」，直接禁止部署，
/// 3050 的用户会莫名其妙被拦。
///
/// 现在两处读显卡的标准统一了：
///   * 先问 nvidia-smi（驱动自己在跑的卡）
///   * 回退时只用 gpu::enumerate() 的结果 —— 它反查了设备树，幽灵条目不参与
pub fn detect_gpu() -> Option<String> {
    pick_gpu_name(&crate::gpu::enumerate(), crate::gpu::nvidia_smi_gpu_name())
}

/// 按型号名判断该走哪条路由。名字判断是有依据的：这些字符串是驱动自己写进注册表的。
pub fn classify_gpu(name: &str) -> GpuRoute {
    let n = name.to_ascii_uppercase();
    if n.contains("RTX 50") || n.contains("RTX 40") {
        return GpuRoute::NotNeeded;
    }
    if n.contains("AMD") || n.contains("RADEON") || n.contains("INTEL") || n.contains("ARC A") {
        return GpuRoute::Unsupported;
    }
    if n.contains("RTX 30") {
        return GpuRoute::Sm86;
    }
    // GTX 16 系（1630 / 1650 / 1660）和 RTX 20 系同为 Turing，但**没有 Tensor Core**：
    // DLSS 帧生成在硬件上就跑不了，换哪个版本都没用。必须单独判出来，不能落到 Sm75 ——
    // 否则界面会给出一条根本无效的建议。
    if n.contains("GTX 16") {
        return GpuRoute::Gtx16;
    }
    // RTX 2050 是 GA107（Ampere），只是名字落在 "RTX 20" 这个子串里 ——
    // 先单独挑出来，否则界面会告诉用户「你是 Turing / SM75」，是错的。
    if n.contains("RTX 2050") {
        return GpuRoute::Sm86;
    }
    if n.contains("RTX 20") {
        return GpuRoute::Sm75;
    }
    // Pascal 及更早、MX、Quadro/Tesla：都没有 Tensor Core，和 GTX 16 系同一种处境。
    // 不分出来的话它们会落到 Unknown —— 而闸门对 Unknown 是放行的，用户会白忙一场。
    if n.contains("GTX 10")
        || n.contains("GTX 9")
        || n.contains("GTX 7")
        || n.contains("GT 10")
        || n.contains("GT 9")
        || n.contains("GT 7")
        || n.contains("GT 6")
        || n.contains("MX1")
        || n.contains("MX2")
        || n.contains("MX3")
        || n.contains("MX4")
        || n.contains("QUADRO")
        || n.contains("TESLA")
    {
        return GpuRoute::NoTensorCore;
    }
    GpuRoute::Unknown
}

/// 这些能被「父目录叫 Win64」命中，但绝不是渲染 EXE。
const EXE_EXCLUDE: &[&str] = &[
    "unins",
    "vcredist",
    "dxsetup",
    "dxwebsetup",
    "crashreport",
    "unitycrashhandler",
    "unrealcefsubprocess",
    "epicwebhelper",
    "eossdk",
    "easyanticheat",
    "eaclauncher",
    "beservice",
    "prereq",
    "installer",
    "subprocess",
    "helper",
    "launcher",
    "vconsole",
];

struct ExeCand {
    path: PathBuf,
    score: i32,
    size: u64,
}

/// 遍历目录、给每个候选 exe 打分，按分数从高到低返回。
///
/// **打分逻辑只此一份**：find_render_exe（自动认）与 candidate_exes（给界面列下拉）
/// 都从这里拿 —— 否则「自动选中的那个」和「下拉里排第一的那个」会是两套标准，
/// 用户看到的列表就和他实际部署的东西对不上。
fn collect_exe_candidates(install_dir: &Path) -> Vec<ExeCand> {
    let mut cands: Vec<ExeCand> = Vec::new();
    let mut visited = 0usize;

    for entry in WalkDir::new(install_dir)
        .max_depth(5)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        visited += 1;
        if visited > 30_000 {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let fname = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !fname.ends_with(".exe") || EXE_EXCLUDE.iter().any(|k| fname.contains(k)) {
            continue;
        }

        // 多信号加权。两条看似更聪明的路都不行：
        //   1) 只看目录名 -> PUBG 的 Engine\Binaries\Win64\UnrealCEFSubProcess、
        //      CS2 的 game\bin\win64\vconsole2 都会被当成正主；
        //   2) 读 PE 导入表找 d3d12.dll -> cs2.exe 只静态导入 user32+kernel32，
        //      TslGame.exe（232 MB）的导入表甚至不在前 16 MB 内。
        // 所以老老实实加权 + 维护排除表。
        let mut score: i32 = 0;
        let stem = fname.trim_end_matches(".exe");

        if fname.ends_with("-win64-shipping.exe") {
            score += 50;
        } else if fname.ends_with("-win32-shipping.exe") {
            score += 20;
        }

        let parent = path.parent();
        let parent_name = parent
            .and_then(|d| d.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or_default();

        // UE 约定：<Project>\Binaries\Win64\<Project>.exe
        if parent_name.eq_ignore_ascii_case("Win64") {
            score += 10;
            let mut anc = parent.and_then(|p| p.parent());
            let anc_is_binaries = anc
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .map(|n| n.eq_ignore_ascii_case("Binaries"))
                .unwrap_or(false);
            if anc_is_binaries {
                anc = anc.and_then(|p| p.parent());
            }
            if let Some(proj) = anc.and_then(|p| p.file_name()).and_then(|s| s.to_str()) {
                if proj.eq_ignore_ascii_case(stem) {
                    score += 30;
                }
            }
        }

        // exe 名和安装目录名一致（如 HogwartsLegacy\...\HogwartsLegacy.exe）
        if let Some(dirname) = install_dir.file_name().and_then(|s| s.to_str()) {
            if dirname.eq_ignore_ascii_case(stem) {
                score += 20;
            }
        }

        // 游戏本体二进制通常很大
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if size > 10 * 1024 * 1024 {
            score += 15;
        } else if size > 2 * 1024 * 1024 {
            score += 3;
        }

        cands.push(ExeCand {
            path: path.to_path_buf(),
            score,
            size,
        });
    }

    cands.sort_by(|a, b| b.score.cmp(&a.score).then(b.size.cmp(&a.size)));
    cands
}

/// 找游戏实际渲染用的 EXE。
///
/// 先按文件名/目录名打分排序（见 collect_exe_candidates），取分最高的那个。
/// 实在认不出来就返回 None（UI 会提示手动选择），不猜。
pub fn find_render_exe(install_dir: &Path) -> Option<PathBuf> {
    let cands = collect_exe_candidates(install_dir);
    let best = cands.first()?;
    // 一个正面信号都没有就老实返回 None，让用户手动选，不要瞎猜
    if best.score <= 0 {
        return None;
    }
    Some(best.path.clone())
}

/// 界面「部署目标」下拉里要列的 exe：这个目录里**可以当启动文件**的那些，按可信度排序。
///
/// 为什么要一个列表、而不只给「自动认出来的那一个」：UE / Unity 游戏的启动器
/// （*Launcher.exe、*Shipping.exe、Bootstrap）经常和真正的渲染 exe 挨着放，
/// 自动认得再准也只是"最像的那个"。让用户一眼看到候选、自己点一下，
/// 比让他去文件对话框里翻目录快得多，也少一层"认错了"的可能。
pub fn candidate_exes(install_dir: &Path) -> Vec<PathBuf> {
    /// 列表上限：一个游戏目录里像样的候选通常个位数，40 已经远超需要，
    /// 但也不能不设 —— 有的 UE 工程目录里几百个 exe。
    const MAX: usize = 40;
    let cands = collect_exe_candidates(install_dir);
    let mut out: Vec<PathBuf> = cands
        .iter()
        .filter(|c| c.score > 0)
        .take(MAX)
        .map(|c| c.path.clone())
        .collect();
    if out.is_empty() {
        // 一个正面信号都没有（测试目录、或不典型的布局）：至少把最大的几个 exe 列出来，
        // 否则下拉是空的，用户还得绕一圈去「手动选择」。
        out = cands.iter().take(12).map(|c| c.path.clone()).collect();
    }
    out
}

pub fn scan_all() -> Vec<GameEntry> {
    scan_all_notes().0
}

/// 扫描全部平台，并带回一份「扫描过程说明」（这些说明同时已经写进日志）。
///
/// 说明里既有统计，也有**每个被跳过的条目和原因**：用户报「扫不出来」时，
/// 让他把 logs 目录发过来就能直接定位，不用再靠猜。
/// 「像游戏库」的目录名（小写）：没有启动器的游戏常被放在这些目录下面。
const LOOSE_LIBRARY_NAMES: &[&str] = &[
    "games", "game", "my games", "steamlibrary", "gog games", "epic games",
    "xboxgames", "origin games", "repacks", "游戏", "单机游戏", "游戏库",
];

/// 目录名里**包含**这些词就肯定不是游戏：播放器/聊天/管家等国产客户端。
/// 用「包含」而不是「相等」：真实目录名五花八门（`QQMusic`、`QQ音乐`、`QQ音乐2024`…），
/// 相等匹配会漏（用户反馈：扫出了 QQ音乐）。
const APP_DIR_MARKERS: &[&str] = &[
    "qqmusic", "qq音乐", "cloudmusic", "netease", "kugou", "kuwo", "bilibili",
    "wechat", "weixin", "微信", "wegame", "tim", "tencent", "thunder", "xunlei",
    "qqlive", "qqplayer", "qqpcmgr", "dingtalk", "钉钉", "qq浏览器", "qqbrowser",
];

/// 这些子目录不是游戏本体：引擎资源、下载缓存、工作坊、存档……
/// 少了它们，`Content` / `steamapps` 也会被当成一个「游戏」。
const LOOSE_NOT_A_GAME: &[&str] = &[
    "steamapps", "workshop", "downloading", "shadercache", "temp", "tmp", "backup",
    "backups", "redist", "redistributable", "directx", "vcredist", "content", "engine",
    "__macosx", "save", "saves", "gamesave", "gamesaves", "config", "mods", "mod",
    "logs", "log", "cache", "common",
];

/// 各盘符下名字像游戏库的目录（D:\Game、E:\SteamLibrary、C:\My Games……）。
fn loose_roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for letter in b"A"[0]..=b"Z"[0] {
        let root = PathBuf::from(format!("{}:\\", letter as char));
        // 不存在 / 空光驱 / 网络盘：直接跳过，不重试
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in rd.flatten().take(500) {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_lowercase();
            if LOOSE_LIBRARY_NAMES.contains(&name.as_str()) {
                out.push(e.path());
            }
        }
    }
    out
}

/// 安装器 / 卸载器 / 更新器：**绝不是游戏主程序**。
/// 少了它，「有 exe 就算游戏」这条会被 `unins000.exe` 骗过去 —— 真实游戏目录里
/// 几乎都有一个卸载器（自测就是被这个抓出来的）。
const LOOSE_NOT_A_GAME_EXE: &[&str] = &[
    "unins", "uninstall", "setup", "install", "vcredist", "vc_redist", "dxsetup",
    "dxwebsetup", "oalinst", "crashpad", "crashreport", "crashhandler", "report",
    "launcher_installer", "update", "updater", "patcher", "helper",
];

/// 浅层检查：这个目录里（最多两层）有没有一个像游戏主程序的 exe。
/// 没有就不认 —— 否则 `Games` 这种空壳目录会变成一条假游戏。
fn has_game_exe(dir: &Path) -> bool {
    for entry in walkdir::WalkDir::new(dir)
        .max_depth(2)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .take(4000)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        if !p
            .extension()
            .map(|x| x.eq_ignore_ascii_case("exe"))
            .unwrap_or(false)
        {
            continue;
        }
        let stem = p
            .file_stem()
            .map(|x| x.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if NON_GAME_EXE_STEMS.iter().any(|s| stem.contains(s.0)) {
            continue;
        }
        if LOOSE_NOT_A_GAME_EXE.iter().any(|s| stem.contains(s)) {
            continue;
        }
        // 太小的多半是安装器 / 小工具，不算「像样的主程序」
        if std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) < 64 * 1024 {
            continue;
        }
        return true;
    }
    false
}

/// 扫「像游戏库的目录」：**没有启动器的游戏也能被扫到**。
///
/// 为什么不靠启动器就够：用户的绿色版 / 手工解压 / 学习版游戏放在 `D:\Game` 这种
/// 目录里，Steam/Epic/WeGame 的记录里根本没有它（真实反馈：别的工具能认出来，我们不能）。
/// 只枚举盘符根下的**库名目录**并看两层，不扫全盘。
pub fn scan_loose_notes(notes: &mut Vec<String>) -> Vec<GameEntry> {
    let roots = loose_roots();
    scan_loose_in(&roots, notes)
}

/// 同上的实际实现，根目录作为参数传入 —— 这样自测能用临时目录构造数据。
pub fn scan_loose_in(roots: &[PathBuf], notes: &mut Vec<String>) -> Vec<GameEntry> {
    let mut out: Vec<GameEntry> = Vec::new();
    if roots.is_empty() {
        notes.push(
            "本地目录：没找到名字像游戏库的目录（Games / Game / My Games / SteamLibrary…），已跳过"
                .to_owned(),
        );
        return out;
    }
    let mut seen: Vec<String> = Vec::new();
    let mut capped = false;
    for root in roots {
        let Ok(rd) = std::fs::read_dir(root) else {
            continue;
        };
        for e in rd.flatten().take(300) {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if LOOSE_NOT_A_GAME.contains(&name.to_lowercase().as_str()) {
                continue;
            }
            let dir = e.path();
            if !has_game_exe(&dir) {
                continue;
            }
            let key = path_key(&dir);
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            out.push(GameEntry {
                source: Launcher::Loose,
                app_id: String::new(),
                name,
                install_dir: dir,
            });
            if out.len() >= 80 {
                capped = true;
                break;
            }
        }
        if capped {
            break;
        }
    }
    notes.push(format!(
        "本地目录：在 {} 个库目录里找到 {} 个游戏{}",
        roots.len(),
        out.len(),
        if capped {
            "（已达 80 个上限，更多的请用「选择目录」手动添加）"
        } else {
            ""
        }
    ));
    out
}

pub fn scan_all_notes() -> (Vec<GameEntry>, Vec<String>) {
    let mut notes = Vec::new();
    let t0 = std::time::Instant::now();
    let mut all = scan_steam_notes(&mut notes);
    all.extend(scan_epic_notes(&mut notes));
    all.extend(scan_wegame_notes(&mut notes));
    // 没有启动器的游戏：枚举盘符找「像游戏库」的目录。用户把游戏解压在
    // D:\Game 这种地方时，只认启动器的扫描是看不到它的（真实反馈）。
    all.extend(scan_loose_notes(&mut notes));
    // 同一个目录可能被两个来源收录（Steam + Epic、或和一个手动条目重了）。
    // 按归一化路径去重，先出现的优先 —— 上面的调用顺序就是 Steam -> Epic -> WeGame。
    // 合并同类噪音：Steam 库里一堆「安装目录不存在」的卸载残留会刷几十行，
    // 把真正有用的信息（找到几个库、几个游戏）淹掉 —— 用户发来的诊断里就是 24 行这种。
    let missing = notes
        .iter()
        .filter(|n| n.contains("库里记着的目录不存在"))
        .count();
    if missing > 1 {
        notes.retain(|n| !n.contains("库里记着的目录不存在"));
        notes.push(format!(
            "  库里记着 {} 个条目的安装目录已不存在（多为卸载残留），已跳过",
            missing
        ));
    }
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    all.retain(|g| seen.insert(path_key(&g.install_dir)));
    all.sort_by_key(|g| g.name.to_lowercase());
    note(
        &mut notes,
        format!(
            "扫描结束：共 {} 个游戏，用时 {:.1} 秒",
            all.len(),
            t0.elapsed().as_secs_f64()
        ),
    );
    (all, notes)
}
