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
    let mut f = std::fs::File::open(path).with_context(|| format!("读取文件失败: {}", path.display()))?;
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

/// git 的 blob 对象哈希 = sha1("blob <字节数>\0" + 内容)。
/// 可以直接和 GitHub contents API 返回的 sha 字段比对，确认下载内容与仓库一致。
pub fn git_blob_sha1(data: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", data.len()).as_bytes());
    h.update(data);
    to_hex(&h.finalize())
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
        let Some(rel) = lower[i..].find(&MARK.to_ascii_lowercase()) else { break };
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
    app_data_dir().map(|d| same_path(&d, &root)).unwrap_or(false)
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
    Some(format!("已把旧位置的 {old_count} 项备份搬到程序目录的 backups\\"))
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
    /// 用户是否已同意改用备用下载源
    #[serde(default)]
    pub allow_backup_source: bool,
    /// 用户选定的下载源前缀。留空 = 自动（按测速值挑最快的那个）。
    #[serde(default)]
    pub backup_prefix: String,
    /// 部署时写进 INI 的「优化等级」（上游 0.3.2 是 0~3 档；出厂默认 1 = 加速且与官方逐位一致）。
    /// 只影响**部署到游戏目录的那一份**，资产目录里的原文件不动。
    #[serde(default = "default_fg_optimized")]
    pub fg_optimized: u8,
    /// 部署时写进 INI 的「倍率上限」（3 = 最高 4X 出厂默认；5 = 最高 6X）。
    #[serde(default = "default_fg_frames")]
    pub fg_frames: u8,
    /// 上次用的**窗口大小**（逻辑点）。
    ///
    /// 以前窗口大小是写死的：用户嫌右侧游戏名/路径被挤掉、手动拉宽，下次启动又回到
    /// 默认值，得反复调（真实反馈）。这里记住它，启动时按上次的大小打开。
    #[serde(default)]
    pub window_size: Option<[f32; 2]>,
}

// 这两个是**业务默认**（1 / 3），不是 u8 的类型默认（0），所以 AppConfig 的 Default
// 必须手写 —— 否则配置文件丢了或者读坏了，会静默变成「档位 0」（原厂内核不加速）。
fn default_fg_optimized() -> u8 {
    1
}
fn default_fg_frames() -> u8 {
    3
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            asset_dir: None,
            allow_backup_source: false,
            backup_prefix: String::new(),
            fg_optimized: default_fg_optimized(),
            fg_frames: default_fg_frames(),
            window_size: None,
        }
    }
}


/// 老配置里有没有「legacy_3101: true」（一次性迁移用）。
///
/// 这个开关已经取消：上游 0.3.1 起 20/30 系用同一套文件。但带这个标记的老配置对应的
/// 本地 version.dll 是旧的：探测落到镜像时指纹不可信，「已是最新」的判定会退化成
/// 「比字节数」，旧文件正好和旧记录对得上，于是静默跳过下载 —— 用户以为升级了，
/// 其实还是老版本。所以启动时看一次老配置。
pub fn config_had_legacy_3101() -> bool {
    let Ok(text) = std::fs::read_to_string(config_path()) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("legacy_3101").and_then(|x| x.as_bool()))
        .unwrap_or(false)
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
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_config(cfg: &AppConfig) -> Result<()> {
    // 配置可能改了资产目录位置：让缓存失效，否则界面还在用旧路径
    forget_assets_dir();
    let p = config_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&p, serde_json::to_string_pretty(cfg)?)
        .with_context(|| format!("写入配置失败: {}", p.display()))?;
    Ok(())
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
    std::fs::create_dir_all(&d)
        .with_context(|| format!("创建资产目录失败: {}", d.display()))?;
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
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
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
    match OpenOptions::new().read(true).write(true).share_mode(0).open(path) {
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
