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

/// git 的 blob 对象哈希 = sha1("blob <字节数>\0" + 内容)。
/// 可以直接和 GitHub contents API 返回的 sha 字段比对，确认下载内容与仓库一致。
pub fn git_blob_sha1(data: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", data.len()).as_bytes());
    h.update(data);
    to_hex(&h.finalize())
}

pub fn app_data_dir() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().context("无法定位用户目录")?;
    let d = base.data_dir().join("FrameGen-Manager");
    std::fs::create_dir_all(&d).with_context(|| format!("创建目录失败: {}", d.display()))?;
    Ok(d)
}

pub fn backups_dir() -> Result<PathBuf> {
    let d = default_backups_dir();
    std::fs::create_dir_all(&d).with_context(|| format!("创建备份目录失败: {}", d.display()))?;
    Ok(d)
}

/// 备份目录：优先 exe 同级的 backups（跟解压出来的文件夹一起走，便携），
/// 同级不可写时回退 %APPDATA%。策略和 assets / 配置文件保持一致。
pub fn default_backups_dir() -> PathBuf {
    if let Some(dir) = exe_dir() {
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

/// 搬迁结果只算一次：main() 一进来就调用它，界面再调用时拿到的是同一句话。
static MIGRATED: OnceLock<Option<String>> = OnceLock::new();

/// 把老位置的备份搬到跟 exe 同级的新位置。
/// 只在「新位置为空」且「老位置有东西」时搬；全部复制成功才删老目录，
/// 任何一步失败都保留老目录 —— 绝不因为搬家把备份弄丢。
pub fn migrate_backups() -> Option<String> {
    MIGRATED.get_or_init(do_migrate_backups).clone()
}

fn do_migrate_backups() -> Option<String> {
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

// ---------------------------------------------------------------- 配置与资产目录

const CONFIG_NAME: &str = "framegen-manager.json";

/// 应用配置。默认放在 exe 同级（便携，解压即用）；exe 同级不可写时回退 %APPDATA%。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    /// 用户自定义的资产目录。None 表示用默认位置。
    #[serde(default)]
    pub asset_dir: Option<PathBuf>,
    /// 用户是否已同意改用备用下载源
    #[serde(default)]
    pub allow_backup_source: bool,
    /// 备用下载源前缀，会拼在官方地址前面。留空表示用内置镜像列表。
    #[serde(default)]
    pub backup_prefix: String,
}

pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|p| p.to_path_buf())
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
            if let Some(dir) = exe_dir() {
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
    if let Some(dir) = exe_dir() {
        let a = dir.join("assets");
        if is_writable(&a) {
            return a;
        }
    }
    app_data_dir()
        .map(|d| d.join("assets"))
        .unwrap_or_else(|_| PathBuf::from("assets"))
}

/// 实际使用的资产目录：用户在界面上改过就用改过的，否则用默认位置。
pub fn assets_dir() -> Result<PathBuf> {
    let d = load_config().asset_dir.unwrap_or_else(default_asset_dir);
    std::fs::create_dir_all(&d)
        .with_context(|| format!("创建资产目录失败: {}", d.display()))?;
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

    let s = wide(src);
    let d = wide(dst);
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
pub fn is_locked(path: &Path) -> bool {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    // dwShareMode = 0 -> 独占
    match OpenOptions::new().read(true).write(true).share_mode(0).open(path) {
        Ok(_) => false,
        Err(e) => e.raw_os_error() == Some(32), // ERROR_SHARING_VIOLATION
    }
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
