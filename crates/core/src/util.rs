//! 通用工具：哈希、原子替换、应用数据目录、MOTW 清理。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    to_hex(&h.finalize())
}

/// 带**进程内缓存**的 sha256（key = 路径 + 大小 + mtime）。
///
/// 部署一次要对 8 个文件算哈希（资产 ~213MB），状态检查、清理判定还会重复算同一批文件。
/// 缓存只对"大小和 mtime 都没变"的文件生效，改过就重算，不存在脏读。
pub fn sha256_file_cached(p: &Path) -> anyhow::Result<String> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    // 缓存值：(大小, mtime 秒, 哈希)。键是规范化后的路径。
    type CacheVal = (u64, u64, String);
    static CACHE: OnceLock<Mutex<HashMap<String, CacheVal>>> = OnceLock::new();

    let meta = std::fs::metadata(p)?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = p.to_string_lossy().to_ascii_lowercase();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(g) = cache.lock() {
        if let Some((s, m, h)) = g.get(&key) {
            if *s == size && *m == mtime {
                return Ok(h.clone());
            }
        }
    }
    let h = sha256_file(p)?;
    if let Ok(mut g) = cache.lock() {
        g.insert(key, (size, mtime, h.clone()));
    }
    Ok(h)
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let data = std::fs::read(path).with_context(|| format!("读取文件失败: {}", path.display()))?;
    Ok(sha256_hex(&data))
}

/// 从文件算 git blob sha1（流式）。下载走的就是这条：几十 MB 的运行库
/// 不必为了算一个哈希再读进内存一遍。
pub fn git_blob_sha1_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let len = std::fs::metadata(path)
        .with_context(|| format!("读取文件失败: {}", path.display()))?
        .len();
    let mut h = Sha1::new();
    h.update(format!("blob {len}\0").as_bytes());
    let mut f =
        std::fs::File::open(path).with_context(|| format!("读取文件失败: {}", path.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(to_hex(&h.finalize()))
}

/// 把路径里的 Windows 用户名换成 `<user>`，好让用户放心把日志发给别人。
///
/// 只动这一处：目录名要留着 —— 「Tencent\\QQ」「steamapps\\common\\…」这些名字正是
/// 排查时真正要看的东西。算得上个人信息的只有用户名、以及它所在的那层用户目录。
/// 两条路都堵：先按 %USERPROFILE% 整段替换，再用 `\Users\<名字>\` 的形状兜底
/// （环境变量缺失、或者日志来自别人机器时也能生效）。
pub fn redact(s: &str) -> String {
    let mut out = s.to_owned();
    if let Some(p) = std::env::var_os("USERPROFILE") {
        let p = p.to_string_lossy().to_string();
        if p.len() > 3 {
            out = out.replace(&p, "%USERPROFILE%");
        }
    }
    redact_users_segment(&out)
}

/// `\Users\<名字>\` 里的那一段名字换成 `<user>`。
fn redact_users_segment(s: &str) -> String {
    const MARK: &str = "\\Users\\";
    // 只做 ASCII 小写，字节长度不变，所以下面按字节切是安全的
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        let Some(rel) = lower[i..].find(&MARK.to_ascii_lowercase()) else {
            break;
        };
        let name_start = i + rel + MARK.len();
        out.push_str(&s[i..name_start]);
        let rest = &s[name_start..];
        let name_end = rest.find('\\').map(|k| name_start + k).unwrap_or(s.len());
        out.push_str("<user>");
        i = name_end;
    }
    out.push_str(&s[i..]);
    out
}

pub fn app_data_dir() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().context("无法定位用户目录")?;
    let d = base.data_dir().join("FrameGen-Manager");
    std::fs::create_dir_all(&d).with_context(|| format!("创建目录失败: {}", d.display()))?;
    Ok(d)
}

/// 老版本的 %APPDATA% 目录名 —— 程序还叫 DLSSG-Manager 的那段时间用的是它。
/// 只用于一次性的兼容读取（升级上来时把老记录/老备份认出来），不往里写新东西。
pub fn legacy_app_data_dir() -> Option<PathBuf> {
    let base = directories::BaseDirs::new()?;
    Some(base.data_dir().join("DLSSG-Manager"))
}

/// 备份根目录**只解析一次**。每次调用重新判断可写性的话，同一份程序在不同的
/// 运行方式下（提权 / 非提权、网络盘临时不可用）会解析到不同目录 —— 部署写在 A、
/// 还原去找 B，结果是「没有找到部署记录」，用户再也还原不回去。
static BACKUPS_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// 备份目录里的条目数与总字节数（设置页要显示“N 项 · X MB”）。
pub fn backups_report() -> (usize, u64) {
    let Ok(dir) = backups_dir() else {
        return (0, 0);
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return (0, 0);
    };
    let mut count = 0usize;
    let mut bytes = 0u64;
    for e in rd.flatten() {
        if !e.path().is_dir() {
            continue;
        }
        count += 1;
        bytes += dir_size(&e.path());
    }
    (count, bytes)
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => total += dir_size(&e.path()),
                Ok(_) => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                Err(_) => {}
            }
        }
    }
    total
}

/// 清理旧备份：删掉除「显卡名称备份」以外的所有记录，返回 (删除条目数, 释放字节)。
///
/// **删了之后已经部署到游戏里的 mod 就还原不回去了**（备份里是游戏原件）。
/// 所以界面必须先明确确认，并提示「想把游戏恢复原样请先逐个还原」。
/// 显卡名称那份留着：它很小，而且是「还原为改动前的名字」唯一的依据。
pub fn clean_backups() -> anyhow::Result<(usize, u64)> {
    let dir = backups_dir()?;
    let mut removed = 0usize;
    let mut freed = 0u64;
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Ok((0, 0));
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name == "gpu-name" || !e.path().is_dir() {
            continue;
        }
        freed += dir_size(&e.path());
        match std::fs::remove_dir_all(e.path()) {
            Ok(_) => removed += 1,
            Err(err) => crate::log::line(&format!("清理备份 {name} 失败：{err}")),
        }
    }
    crate::log::line(&format!("已清理 {removed} 份旧备份，释放 {freed} 字节"));
    Ok((removed, freed))
}

/// 备份明细：让界面在"清理"之前能列出**将要删掉哪些东西**。
/// 返回 (条目名, 种类, 字节数)；种类 = gpu-name / dlss5 / framegen。
pub fn backups_entries() -> Vec<(String, String, u64, String)> {
    let Ok(dir) = backups_dir() else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        if !e.path().is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        let kind = if name == "gpu-name" {
            "gpu-name"
        } else if name.ends_with("-dlss5") {
            "dlss5"
        } else {
            "framegen"
        };
        // 明细里尽量带上"这是哪个游戏目录"，清理前用户才看得懂要删什么。
        // 用宽松匹配读记录（不同种类记录里字段名不一样），读不到就留空。
        let mut target = String::new();
        if let Ok(t) = std::fs::read_to_string(e.path().join("manifest.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                for key in ["target_dir", "deploy_dir", "dir", "game_dir", "install_dir"] {
                    if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                        target = s.to_owned();
                        break;
                    }
                }
            }
        }
        out.push((name, kind.to_owned(), dir_size(&e.path()), target));
    }
    // 同种类的排一起，方便界面按种类分组显示
    out.sort_by_key(|e| (e.1.clone(), e.0.clone()));
    out
}

pub fn backups_dir() -> Result<PathBuf> {
    let d = BACKUPS_ROOT.get_or_init(default_backups_dir).clone();
    std::fs::create_dir_all(&d).with_context(|| format!("创建备份目录失败: {}", d.display()))?;
    Ok(d)
}

/// 备份目录：优先 exe 同级的 backups（跟解压出来的文件夹一起走，便携），
/// 同级不可写时回退 %APPDATA%。策略和 assets / 配置文件保持一致。
pub fn default_backups_dir() -> PathBuf {
    if let Some(dir) = data_root() {
        let b = dir.join("backups");
        if is_writable(&b) {
            return b;
        }
    }
    app_data_dir()
        .map(|d| d.join("backups"))
        .unwrap_or_else(|_| PathBuf::from("backups"))
}

/// 老版本把备份放在 %APPDATA%\FrameGen-Manager\backups，只用于一次性搬迁。
fn legacy_backups_dir() -> Option<PathBuf> {
    app_data_dir().ok().map(|d| d.join("backups"))
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let src = e.path();
        let dst = to.join(e.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// 两个路径是不是同一个（大小写、末尾斜杠都不计较；Windows 下路径形式可能不同）。
fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| -> String {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_lowercase()
    };
    norm(a) == norm(b)
}

/// 当前数据根是不是「正常的两个位置」之一：
///   * 便携版 —— exe 同级（没有 FGM_DATA_DIR）
///   * 安装版 —— %APPDATA%\FrameGen-Manager（main.js 通过环境变量指的）
///
/// **别的目录一律不算**（打包脚本的冒烟测试、开发调试、用户手动指到临时目录）：
/// 那种情况下绝不能去搬 %APPDATA% 里的老备份 —— 实测会把用户唯一的备份
/// 搬进临时目录、随后被清理掉。
fn data_root_is_normal() -> bool {
    let Some(root) = data_root() else {
        return false;
    };
    if exe_dir().map(|d| same_path(&d, &root)).unwrap_or(false) {
        return true;
    }
    app_data_dir()
        .map(|d| same_path(&d, &root))
        .unwrap_or(false)
}

/// 搬迁结果只算一次：main() 一进来就调用它，界面再调用时拿到的是同一句话。
static MIGRATED: OnceLock<Option<String>> = OnceLock::new();

/// 把老位置的备份搬到跟 exe 同级的新位置。
/// 只在「新位置为空」且「老位置有东西」时搬；全部复制成功才删老目录，
/// 任何一步失败都保留老目录 —— 绝不因为搬家把备份弄丢。
pub fn migrate_backups() -> Option<String> {
    MIGRATED.get_or_init(do_migrate_backups).clone()
}

fn do_migrate_backups() -> Option<String> {
    // 数据根必须是「正常位置」才搬 —— 理由见 data_root_is_normal 的注释
    // （临时数据目录会让这函数把 %APPDATA% 里的备份搬走再删掉，实测踩到过）。
    if !data_root_is_normal() {
        return None;
    }
    let new = backups_dir().ok()?;
    let old = legacy_backups_dir()?;
    if new == old || !old.is_dir() {
        return None;
    }
    // 新位置已经有东西就不动，避免覆盖
    if std::fs::read_dir(&new).ok()?.next().is_some() {
        return None;
    }
    let old_count = std::fs::read_dir(&old).ok()?.count();
    if old_count == 0 {
        return None;
    }
    if copy_tree(&old, &new).is_err() {
        return None;
    }
    let _ = std::fs::remove_dir_all(&old);
    Some(format!(
        "已把旧位置的 {old_count} 项备份搬到程序目录的 backups\\"
    ))
}

/// 把**更老那一代**（程序还叫 DLSSG-Manager 时用的 %APPDATA%\DLSSG-Manager\backups）
/// 里的部署记录搬到当前备份目录。
///
/// 为什么需要它：老版本的记录放在那个旧目录里，而现在的记录在
/// %APPDATA%\FrameGen-Manager\backups（便携版在 exe 同级）。不搬的话，老用户升级上来
/// 会看到「已安装，本工具无记录」—— 文件靠签名仍认得出是本项目的，但**备份找不到，
/// 「还原」就永远是灰的**（用户问的正是这件事）。
///
/// 逐个子目录搬：目标已存在就跳过（新的优先，绝不覆盖）；全部处理成功才删源目录，
/// 任何一份复制失败都保留源目录 —— 绝不因为搬家把备份弄丢。
pub fn migrate_legacy_dlssg_backups() -> Option<String> {
    let old = legacy_app_data_dir()?.join("backups");
    if !old.is_dir() || !data_root_is_normal() {
        return None;
    }
    let new = backups_dir().ok()?;
    if same_path(&old, &new) {
        return None;
    }
    let mut moved = 0usize;
    let mut copied_all = true;
    for e in std::fs::read_dir(&old).ok()?.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let dst = new.join(e.file_name());
        if dst.exists() {
            continue; // 新位置已经有这一份（就是它，或者更新的），不覆盖
        }
        if copy_tree(&e.path(), &dst).is_err() {
            copied_all = false;
            let _ = std::fs::remove_dir_all(&dst); // 半份没用，删掉下次重来
            continue;
        }
        moved += 1;
    }
    if moved == 0 {
        return None;
    }
    if copied_all {
        let _ = std::fs::remove_dir_all(&old);
    }
    Some(format!(
        "已把老版本（DLSSG-Manager）的 {moved} 份部署记录搬进当前备份目录"
    ))
}

// ---------------------------------------------------------------- 配置与资产目录

const CONFIG_NAME: &str = "framegen-manager.json";

/// 应用配置。默认放在 exe 同级（便携，解压即用）；exe 同级不可写时回退 %APPDATA%。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// 用户自定义的资产目录。None 表示用默认位置。
    #[serde(default)]
    pub asset_dir: Option<PathBuf>,

    /// 部署时写进 INI 的「优化等级」（上游 0.4.0 是 0~3 档；出厂默认 **2** = 全部加速
    /// 再加有损图像内核，并排看与档位 1 没有可见差别）。
    /// 只影响**部署到游戏目录的那一份**，资产目录里的原文件不动。
    #[serde(default = "default_fg_optimized")]
    pub fg_optimized: u8,
    /// 部署时写进 INI 的「倍率上限」（3 = 最高 4X 出厂默认；5 = 最高 6X）。
    #[serde(default = "default_fg_frames")]
    pub fg_frames: u8,
    /// 部署时写进 INI 的 DLSS-G 渲染预设（0.4.0 的 [DLSSG] Preset）。
    /// 出厂默认 C（FG Lite）；界面给 Auto / A / B / C。空串按出厂默认处理。
    #[serde(default = "default_fg_preset")]
    pub fg_preset: String,
    /// 优化等级从 0.3.x 时代的出厂值 1 迁到 0.4.0 的出厂值 2，只做一次。
    ///
    /// 为什么要迁移：老用户当年部署时，程序把当时的出厂值 1 记进了配置 —— 哪怕他从没主动选过。
    /// 于是升级到 0.4.0 之后，对话框还替他选着 1（与官方逐位一致），永远拿不到 0.4.0 的出厂默认 2。
    /// 这个标志保证只迁一次：用户之后自己改成 0/1/3 都不会再被动。
    #[serde(default)]
    pub optimized_migrated_to_2: bool,
    /// 上次"检测到旧版帧生成、要不要统一重做"这个提示是按哪个程序版本弹的。
    /// 记版本号而不是布尔值：升级到下一个版本时应该再问一次。
    #[serde(default)]
    pub legacy_prompt_version: String,
    /// 上次用的**窗口大小**（逻辑点）。
    ///
    /// 以前窗口大小是写死的：用户嫌右侧游戏名/路径被挤掉、手动拉宽，下次启动又回到
    /// 默认值，得反复调（真实反馈）。这里记住它，启动时按上次的大小打开。
    #[serde(default)]
    pub window_size: Option<[f32; 2]>,
    /// 帧生成超分（SR）预设：0.4.1 起是显式五选一
    /// Off / M-INT8 / L-INT8 / M-FP16 / L-FP16。空串按 Off 处理。
    #[serde(default)]
    pub fg_sr_mode: String,
    /// 「旧版帧生成已自动清除」这个动作按哪个程序版本做过一次
    #[serde(default)]
    pub fg_cleanup_version: String,
    /// 每次启动时自动刷新游戏库（默认**关**，用户要求）。
    ///
    /// 关掉后启动只显示上次的缓存（秒开），发现新游戏靠手动点「扫描游戏库」；
    /// 第一次使用（库是空的）无论开关如何都会自动扫一次。
    #[serde(default)]
    pub auto_scan_on_start: bool,
    /// 「Smooth Motion 组件有新版本」这个提示是按哪个程序版本弹的。
    /// 记版本号而不是布尔值：每次发新版应该再提醒一次。
    #[serde(default)]
    pub sm_legacy_prompt_version: String,
    /// 「DLSS5 已更换为 GITC 版，需先卸载再重装」这个提示按哪个程序版本弹过。
    #[serde(default)]
    pub dlss5_legacy_prompt_version: String,
    /// **统一**的「三类资产有更新」提示按哪个程序版本弹过（帧生成 / SM / DLSS5 合并成一个弹窗）。
    #[serde(default)]
    pub assets_legacy_prompt_version: String,
    /// 1.7.0 停更退场的「清空所有部署」是否已经成功跑完。
    ///
    /// 失败时保持 false：下次启动会重试（否则没清掉的 version.dll 就永远留在游戏目录里）。
    #[serde(default)]
    pub retired: bool,
    /// 已经尝试过退场清理但没成功：停更版不允许再往游戏里部署，直到清理通过。
    #[serde(default)]
    pub retire_pending: bool,
    /// 停更公告是否已经给用户看过（只弹一次）。
    #[serde(default)]
    pub retire_announced: bool,
}

// 这两个是**业务默认**（2 / 3），不是 u8 的类型默认（0），所以 AppConfig 的 Default
// 必须手写 —— 否则配置文件丢了或者读坏了，会静默变成「档位 0」（原厂内核不加速）。
fn default_fg_optimized() -> u8 {
    // 0.4.1 出厂 Optimized=1（与官方逐位一致）
    1
}
fn default_fg_frames() -> u8 {
    3
}
fn default_fg_preset() -> String {
    // 见 update::DEFAULT_PRESET：C = FG Lite（生成帧开销明显更低、画质略低）
    crate::update::DEFAULT_PRESET.to_owned()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            asset_dir: None,
            fg_optimized: default_fg_optimized(),
            fg_frames: default_fg_frames(),
            fg_preset: default_fg_preset(),
            optimized_migrated_to_2: false,
            legacy_prompt_version: String::new(),
            window_size: None,
            fg_sr_mode: String::new(),
            fg_cleanup_version: String::new(),
            auto_scan_on_start: false,
            sm_legacy_prompt_version: String::new(),
            dlss5_legacy_prompt_version: String::new(),
            assets_legacy_prompt_version: String::new(),
            retired: false,
            retire_pending: false,
            retire_announced: false,
        }
    }
}

pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|p| p.to_path_buf())
}

/// 数据根目录的环境变量覆盖点。
///
/// **为什么需要它**：安装版（Electron + NSIS）不能把数据放在程序目录里 ——
/// electron-builder 的卸载器最后一句是 `RMDir /r $INSTDIR`，而它的安装流程会
/// **先跑旧版卸载器**（installSection.nsh 里无条件调用 uninstallOldVersion）。
/// 实测：同一个安装包连装两次，配置 / 游戏库 / 95MB 资产 / 备份全被删光。
///
/// 所以：**安装版**由 Electron 主进程把 FGM_DATA_DIR 指到 %APPDATA%\FrameGen-Manager；
/// **便携版**（zip 解压即用）不设这个变量，行为与以前完全一致。
pub const DATA_DIR_ENV: &str = "FGM_DATA_DIR";

/// 数据根目录：FGM_DATA_DIR 优先，否则 exe 同级（便携版）。
pub fn data_root() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os(DATA_DIR_ENV) {
        if !d.is_empty() {
            return Some(PathBuf::from(d));
        }
    }
    exe_dir()
}

/// 真去写一个探针文件来判断可写性 —— 只看只读位判断不准，还要看 ACL。
pub fn is_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".dlssg-write-probe");
    match std::fs::write(&probe, b"x") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// 配置文件位置：exe 同级优先（便携）。已有配置文件或 exe 目录可写就用它，
/// 否则退到 %APPDATA%。
pub fn config_path() -> PathBuf {
    CONFIG_PATH
        .get_or_init(|| {
            if let Some(dir) = data_root() {
                let p = dir.join(CONFIG_NAME);
                if p.exists() || is_writable(&dir) {
                    return p;
                }
            }
            app_data_dir()
                .map(|d| d.join(CONFIG_NAME))
                .unwrap_or_else(|_| PathBuf::from(CONFIG_NAME))
        })
        .clone()
}

pub fn load_config() -> AppConfig {
    let raw = std::fs::read_to_string(config_path()).ok();
    let Some(text) = raw else {
        return AppConfig::default();
    };
    let Ok(mut cfg) = serde_json::from_str::<AppConfig>(&text) else {
        return AppConfig::default();
    };
    /* 老配置（1.1.2 之前）里没有 fg_preset 这一项：serde 会按默认值给 C，界面也用 C，
    但文件本身不体现 —— 用户打开配置文件看不到这一项，会以为没生效。
    这里补写回去一次，让文件和界面说的是同一件事。 */
    let mut dirty = false;
    if !text.contains("\"fg_preset\"") {
        cfg.fg_preset = default_fg_preset();
        dirty = true;
    }
    /* 出厂优化等级从 1 换成 2：把"当年那个旧出厂值"迁过来（只迁一次，见字段说明）。
    只动 1：用户自己选的 0（不加速）或 3（全部有损）是明确的偏好，不能改。 */
    if !cfg.optimized_migrated_to_2 {
        if cfg.fg_optimized == 1 {
            crate::log::line(
                "优化等级：配置里还是 0.3.x 时代的出厂值 1，按 0.4.0 的出厂默认改成 2（想改回去在部署对话框里选）",
            );
            cfg.fg_optimized = 2;
        }
        cfg.optimized_migrated_to_2 = true;
        dirty = true;
    }
    if dirty {
        let _ = save_config(&cfg);
    }
    cfg
}

pub fn save_config(cfg: &AppConfig) -> Result<()> {
    // 配置可能改了资产目录位置：让缓存失效，否则界面还在用旧路径
    forget_assets_dir();
    let p = config_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_json_atomic(&p, cfg).with_context(|| format!("写入配置失败: {}", p.display()))?;
    Ok(())
}

// ---------------------------------------------------------------- 原子写 JSON / 文本

/// 目标文件的临时名：`manifest.json` -> `manifest.json.tmp`（同目录，`atomic_replace` 才好换）。
fn tmp_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// 原子写文本：先写同目录 `.tmp`，再 `atomic_replace` 换上去。
///
/// 为什么统一走这一条：非原子写中途崩溃会留下半截文件，而**「文件在但读不出来」
/// 和「没有这份文件」是两种完全不同的处境** —— 部署记录读不出来时当成"没部署过"，
/// 下一步就会把游戏原件覆盖掉（见 `deploy::manifest_state` 的说明）。
/// 所以凡是"下次启动还要读"的文件一律原子写。
pub fn write_text_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = tmp_sibling(path);
    std::fs::write(&tmp, text).with_context(|| format!("写入 {} 失败", tmp.display()))?;
    atomic_replace(&tmp, path)?;
    Ok(())
}

/// 原子写 JSON（pretty，便于用户自己打开看）。**部署记录、配置、游戏库、GPU 名称备份
/// 都走它**，别再各写一遍 `to_string_pretty` + `fs::write`。
pub fn write_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    write_text_atomic(path, &text)
}

/// 默认资产目录：exe 同级 assets（能写就用，便携），否则 %APPDATA%\DLSSG-Manager\assets
pub fn default_asset_dir() -> PathBuf {
    if let Some(dir) = data_root() {
        let a = dir.join("assets");
        if is_writable(&a) {
            return a;
        }
    }
    app_data_dir()
        .map(|d| d.join("assets"))
        .unwrap_or_else(|_| PathBuf::from("assets"))
}

/// 资产目录的缓存。界面每帧都要显示它、还要拿它拼路径 —— 不缓存的话就是每帧一次
/// 读配置文件 + 一次 create_dir_all（60 FPS 下每秒上百次系统调用，网络盘/机械盘上
/// 明显拖慢界面）。
static ASSETS_DIR: OnceLock<std::sync::Mutex<Option<PathBuf>>> = OnceLock::new();

fn assets_cache() -> &'static std::sync::Mutex<Option<PathBuf>> {
    ASSETS_DIR.get_or_init(|| std::sync::Mutex::new(None))
}

/// 用户在界面上改了资产目录位置之后调用，让下一次重新解析。
pub fn forget_assets_dir() {
    if let Ok(mut g) = assets_cache().lock() {
        *g = None;
    }
}

/// 实际使用的资产目录：用户在界面上改过就用改过的，否则用默认位置。
pub fn assets_dir() -> Result<PathBuf> {
    if let Ok(g) = assets_cache().lock() {
        if let Some(d) = g.as_ref() {
            return Ok(d.clone());
        }
    }
    let d = load_config().asset_dir.unwrap_or_else(default_asset_dir);
    std::fs::create_dir_all(&d).with_context(|| format!("创建资产目录失败: {}", d.display()))?;
    if let Ok(mut g) = assets_cache().lock() {
        *g = Some(d.clone());
    }
    Ok(d)
}

/// 用规范化后的路径做稳定 key（大小写、分隔符、末尾斜杠都归一化）。
pub fn dir_key(dir: &Path) -> String {
    let s = dir
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase();
    sha256_hex(s.as_bytes())[..16].to_string()
}

/// 需要时把路径转成 Win32 的「verbatim」形式（\\?\ 前缀），绕开 260 字符上限。
///
/// 什么时候需要：游戏装在很深的目录里（Steam 库 + 长中文目录名），
/// 部署目标加上临时文件名就可能超过 MAX_PATH —— 那时 WriteFile/CopyFile 会直接失败，
/// 用户看到的是「写不进去」但不知道为什么。短路径保持原样，免得改变别的语义。
pub fn long_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if !p.is_absolute() || s.len() <= 240 {
        return p.to_path_buf();
    }
    // 前缀用字符拼出来，源码里就不必到处写反斜杠转义
    let bs = '\\';
    let unc: String = [bs, bs].iter().collect();
    let verbatim: String = [bs, bs, '?', bs].iter().collect();
    if s.starts_with(&verbatim) {
        return p.to_path_buf();
    }
    // UNC：{verbatim}UNC{bs}server{bs}share；本地盘：{verbatim}C:{bs}...
    if let Some(rest) = s.strip_prefix(&unc) {
        return PathBuf::from(format!("{verbatim}UNC{bs}{rest}"));
    }
    PathBuf::from(format!("{verbatim}{s}"))
}

/// 原子替换：MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)。
/// std::fs::rename 在目标已存在时会失败，所以必须走 Win32。
pub fn atomic_replace(src: &Path, dst: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    // 长路径加 verbatim 前缀，否则深目录下的游戏根本写不进去
    let s = wide(&long_path(src));
    let d = wide(&long_path(dst));
    let ok = unsafe {
        MoveFileExW(
            s.as_ptr(),
            d.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        bail!(
            "原子替换失败: {} -> {} ({})",
            src.display(),
            dst.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// 删除 Zone.Identifier 备用流。下载来的文件带 MOTW，复制进游戏目录后应当清掉。
pub fn clear_motw(path: &Path) {
    let mut ads = path.as_os_str().to_owned();
    ads.push(":Zone.Identifier");
    let _ = std::fs::remove_file(PathBuf::from(ads));
}

/// 独占方式试打开，用来判断文件是否被占用（游戏在运行）。
/// 目标文件现在能不能写。**三态**：只说「被占用」会让用户一直去关游戏，
/// 而实际可能只是文件带了只读属性。
pub enum LockState {
    Free,
    InUse,
    NoAccess,
}

pub fn lock_state(path: &Path) -> LockState {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    // dwShareMode = 0 -> 独占
    match OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(path)
    {
        Ok(_) => LockState::Free,
        Err(e) => match e.raw_os_error() {
            Some(32) | Some(33) => LockState::InUse, // 共享冲突 / 区域锁
            Some(5) => LockState::NoAccess,          // 只读属性或权限不足
            _ => LockState::Free,
        },
    }
}

/// Unix 秒（锁文件里记「什么时候拿的」用它）。
pub fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 不引入 chrono，自己算一个 UTC 时间戳（Howard Hinnant 的 civil_from_days）。
pub fn now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 本地时间 `HH:MM:SS`（日志用）。
///
/// 之前日志写的是 UTC，用户看到的时间比自己晚 8 小时，报障时对不上。
pub fn now_local_hms() -> String {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::SystemInformation::GetLocalTime;
        let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut st);
        return format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond);
    }
    #[allow(unreachable_code)]
    now_utc().split(' ').nth(1).unwrap_or_default().to_owned()
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn format_bytes(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{} B", n)
    }
}

/// 用系统默认浏览器打开网址。失败时返回 Err，由调用方决定怎么提示。
pub fn open_url(url: &str) -> Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let verb = wide("open");
    let target = wide(url);
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 返回值 <= 32 即失败
    if r as isize <= 32 {
        bail!("打开浏览器失败（ShellExecuteW 返回 {}）", r as isize);
    }
    Ok(())
}

/// 把清单里的**相对路径**安全地拼到基准目录上（P0-11）。
///
/// 部署记录（manifest）是磁盘上的 JSON，可能被篡改或损坏。直接 `base.join(rel)` 会允许
/// `..\\..\\Windows\\System32\\x.dll` 或绝对路径，等于让一个 JSON 文件决定“往哪写、删哪”。
/// 这里拒绝：空、绝对路径、盘符/UNC、含 `..` 的组件、Windows 保留名与控制字符。
pub fn safe_join(base: &std::path::Path, rel: &str) -> anyhow::Result<std::path::PathBuf> {
    let rel = rel.trim();
    if rel.is_empty() {
        anyhow::bail!("清单里的路径为空");
    }
    if rel.len() > 512 {
        anyhow::bail!("清单里的路径过长");
    }
    if rel.chars().any(|c| (c as u32) < 0x20) {
        anyhow::bail!("清单里的路径含控制字符");
    }
    let p = std::path::Path::new(rel);
    if p.is_absolute() {
        anyhow::bail!("清单里的路径是绝对路径：{rel}");
    }
    if rel.contains(":") {
        anyhow::bail!("清单里的路径含盘符或 UNC：{rel}");
    }
    if rel.starts_with('\\') {
        anyhow::bail!("清单里的路径是 UNC：{rel}");
    }
    let mut out = base.to_path_buf();
    for comp in p.components() {
        match comp {
            std::path::Component::Normal(seg) => {
                let s = seg.to_string_lossy();
                let up = s.to_ascii_uppercase();
                let stem = up.split('.').next().unwrap_or("");
                const RESERVED: [&str; 22] = [
                    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6",
                    "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7",
                    "LPT8", "LPT9",
                ];
                if RESERVED.contains(&stem) {
                    anyhow::bail!("清单里的路径含保留名：{rel}");
                }
                out.push(seg);
            }
            _ => anyhow::bail!("清单里的路径非法（含 . / .. / 前缀）：{rel}"),
        }
    }
    if !out.starts_with(base) {
        anyhow::bail!("清单里的路径越界：{rel}");
    }
    Ok(out)
}

/// 读 PE 的版本资源块，返回一块**宽松的 ASCII 文本**（仅用于特征判断，不保证完整）。
///
/// 用途：识别“这个 dxgi.dll 是不是 ReShade 装进来的”——ReShade 的版本资源里带
/// `ProductName=ReShade`，而游戏/其它 mod 的代理没有。比写死某个版本的哈希更耐升级。
#[cfg(windows)]
pub fn file_version_text(p: &std::path::Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW};
    let wide: Vec<u16> = p
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let mut handle = 0u32;
        let size = GetFileVersionInfoSizeW(wide.as_ptr(), &mut handle);
        if size == 0 {
            return None;
        }
        if size > 4 * 1024 * 1024 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        if GetFileVersionInfoW(wide.as_ptr(), 0, size, buf.as_mut_ptr() as *mut _) == 0 {
            return None;
        }
        // 版本块里字符串是 UTF-16LE；丢掉 0 字节后按 ASCII 粗读，命中特征即可
        let mut out = String::with_capacity(buf.len() / 2);
        let mut i = 0usize;
        while i + 1 < buf.len() {
            let ch = buf[i];
            if ch != 0 {
                out.push(if ch.is_ascii() { ch as char } else { ' ' });
            }
            i += 2;
        }
        if out.trim().is_empty() {
            None
        } else {
            Some(out)
        }
    }
}

#[cfg(test)]
mod config_compat_tests {
    use super::AppConfig;

    /// 1.7.0 之前的配置里没有 retired / retire_pending / retire_announced。
    /// 少一个 `serde(default)` 就会让整个配置解析失败、所有设置被重置 —— 这里钉住它。
    #[test]
    fn old_config_without_retire_fields_still_parses() {
        let cfg: AppConfig = serde_json::from_str(r#"{"fg_optimized":0}"#).unwrap();
        assert_eq!(cfg.fg_optimized, 0);
        assert!(!cfg.retired);
        assert!(!cfg.retire_pending);
        assert!(!cfg.retire_announced);
    }

    #[test]
    fn retire_fields_round_trip() {
        let cfg = AppConfig {
            retired: true,
            retire_pending: false,
            retire_announced: true,
            ..AppConfig::default()
        };
        let text = serde_json::to_string(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert!(back.retired);
        assert!(back.retire_announced);
    }
}

#[cfg(test)]
mod safe_join_tests {
    use super::safe_join;
    use std::path::Path;

    #[test]
    fn rejects_escapes() {
        let base = Path::new("C:\\games\\demo");
        assert!(safe_join(base, "version.dll").is_ok());
        assert!(safe_join(base, "dlssg_sm86\\kernels\\fg_kernels.dll").is_ok());
        assert!(safe_join(base, "..\\evil.dll").is_err());
        assert!(safe_join(base, "a\\..\\..\\evil.dll").is_err());
        assert!(safe_join(base, "C:\\Windows\\x.dll").is_err());
        assert!(safe_join(base, "\\\\server\\share\\x.dll").is_err());
        assert!(safe_join(base, "con.txt").is_err());
        assert!(safe_join(base, "").is_err());
    }
}
