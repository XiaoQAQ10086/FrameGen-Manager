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
}

impl Launcher {
    pub fn label(self) -> &'static str {
        match self {
            Launcher::Steam => "Steam",
            Launcher::Epic => "Epic",
            Launcher::WeGame => "WeGame",
            Launcher::Manual => "手动",
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
/// 扫描「扫不出来」这类反馈以前完全没法查 —— 现在每个跳过都有理由落在日志里，
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
/// 早先用「名字里包含 proton 就算运行库」的子串匹配，会把真游戏一起误杀
/// （Proton Bus Simulator 就是）；所以现在按 Steam 对运行库的固定命名来判：
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
// 关键教训：早先的实现是「把文件前 N MB 读进来再解析」，对 232 MB 的 TslGame.exe
// 和 457 MB 的 HogwartsLegacy.exe 完全失效 —— 导入表根本不在前 16 MB 内。
// 现在先读头部拿节表，再按节表把 RVA 换算成文件偏移，seek 过去精确读取，任意大小都能解析。

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

/// 这些 Tencent 子键明显不是 WeGame 游戏（客户端、聊天工具之类）。
///
/// 只做**精确匹配**，不做前缀匹配 —— 免得把「QQ飞车」这种真游戏一起误杀。
const WEGAME_NON_GAME_KEYS: [&str; 9] = [
    "WeGame", "wegame", "QQ", "QQNT", "QQProtect", "WeChat", "Weixin", "TIM", "TencentDocs",
];

/// WeGame 自己装没装。
///
/// 这个前提挡掉的是最要命的一类误报：本机装了 QQ，而 QQ 的注册表键**也在 Tencent
/// 下面**，它的数据指向 QQ 自己的安装目录，里头当然找得到 exe —— 于是被当成一条
/// 「WeGame 游戏」列出来了（实测踩到过）。装都没装 WeGame，就不可能有 WeGame 游戏。
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
            "WeGame：本机没装（Tencent 下没有 WeGame 的安装目录），跳过".to_owned(),
        );
        return Vec::new();
    }
    let mut out: Vec<GameEntry> = Vec::new();
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let (mut keys, mut non_game, mut no_dir, mut no_exe) = (0usize, 0usize, 0usize, 0usize);

    for root in WEGAME_REG_ROOTS {
        let Ok(k) = hklm.open_subkey(root) else { continue };
        for key_name in k.enum_keys().flatten() {
            // 客户端 / 聊天工具之类的键不是游戏
            keys += 1;
            if WEGAME_NON_GAME_KEYS
                .iter()
                .any(|n| key_name.eq_ignore_ascii_case(n))
            {
                non_game += 1;
                continue;
            }
            let Ok(gk) = k.open_subkey(&key_name) else { continue };

            let mut dir: Option<PathBuf> = None;
            let mut weak: Option<PathBuf> = None;
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
                    } else if weak.is_none() {
                        weak = Some(d);
                    }
                }
            }

            let Some(dir) = dir.or(weak) else {
                no_dir += 1;
                continue;
            };
            // 必须有能找到的游戏程序 —— 这一条把绝大多数噪音挡在外面
            if find_render_exe(&dir).is_none() {
                no_exe += 1;
                continue;
            }
            let name = display
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| wegame_name_from_key(&key_name));
            push_wegame(&mut out, name, dir);
        }
    }

    // WeGame 自己的安装目录下可能有 apps\ 结构，兜底再扫一遍
    for base in wegame_install_bases() {
        let Ok(rd) = std::fs::read_dir(base.join("apps")) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() || find_render_exe(&p).is_none() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if !name.trim().is_empty() {
                push_wegame(&mut out, name, p);
            }
        }
    }

    note(
        notes,
        format!(
            "WeGame 扫描完成：看了 {keys} 个注册表项，收下 {} 个游戏，跳过 {}（非游戏键 {non_game}、没读到目录 {no_dir}、目录里找不到游戏程序 {no_exe}）",
            out.len(),
            non_game + no_dir + no_exe
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
    if out.iter().any(|g| g.install_dir == dir) {
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
            GpuRoute::NotNeeded => "RTX 40 / 50 系",
            GpuRoute::Unsupported => "非 NVIDIA 显卡",
            GpuRoute::Unknown => "未能识别",
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
/// **不再自己遍历显示适配器类键。** 以前这里是「取第一个名字带 NVIDIA 的条目」，
/// 而那个键下面可能有：
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
    if n.contains("RTX 20") {
        return GpuRoute::Sm75;
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

/// 找游戏实际渲染用的 EXE。
///
/// 两阶段：先按文件名/目录名打分排序，再对前 25 个候选读 PE 导入表，
/// 优先选真正导入 d3d12.dll 的。既准确，又不会把整个游戏目录的 exe 全读一遍。
/// 实在认不出来就返回 None（UI 会提示手动选择），不猜。
pub fn find_render_exe(install_dir: &Path) -> Option<PathBuf> {
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

        // 多信号加权。实测过两条看似更聪明的路，都不行：
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

    if cands.is_empty() {
        return None;
    }

    cands.sort_by(|a, b| b.score.cmp(&a.score).then(b.size.cmp(&a.size)));
    let best = cands.first()?;
    // 一个正面信号都没有就老实返回 None，让用户手动选，不要瞎猜
    if best.score <= 0 {
        return None;
    }
    Some(best.path.clone())
}

pub fn scan_all() -> Vec<GameEntry> {
    scan_all_notes().0
}

/// 扫描全部平台，并带回一份「扫描过程说明」（这些说明同时已经写进日志）。
///
/// 说明里既有统计，也有**每个被跳过的条目和原因**：用户报「扫不出来」时，
/// 让他把 logs 目录发过来就能直接定位，不用再靠猜。
pub fn scan_all_notes() -> (Vec<GameEntry>, Vec<String>) {
    let mut notes = Vec::new();
    let t0 = std::time::Instant::now();
    let mut all = scan_steam_notes(&mut notes);
    all.extend(scan_epic_notes(&mut notes));
    all.extend(scan_wegame_notes(&mut notes));
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
