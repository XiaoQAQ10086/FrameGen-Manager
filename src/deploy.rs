//! 部署 / 备份 / 还原。
//!
//! 目录布局：
//!   %APPDATA%\DLSSG-Manager\backups\<game_key>\manifest.json
//!   %APPDATA%\DLSSG-Manager\backups\<game_key>\files\<name>.bak

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::util;

/// 上游 altnative/ 提供的替代代理入口，同一时刻只应存在一个。
pub const PROXY_ENTRIES: [&str; 5] = [
    "version.dll",
    "winmm.dll",
    "dinput8.dll",
    "winhttp.dll",
    "dxgi.dll",
];
pub const INI_NAME: &str = "dlssg_sm86.ini";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupEntry {
    pub rel_path: String,
    pub existed_before: bool,
    pub backup_name: Option<String>,
    pub original_sha256: Option<String>,
    pub deployed_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub game_key: String,
    pub game_dir: PathBuf,
    pub deployed_at: String,
    pub proxy_name: String,
    pub files: Vec<BackupEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployState {
    NotDeployed,
    /// 本工具部署的（有 manifest，可以一键还原）
    Deployed { proxy: String, deployed_at: String },
    /// 目录里有本项目的文件，但不是本工具部署的（用户之前手动装的）
    ManuallyInstalled { files: Vec<String> },
    /// 目录里有代理 DLL，但不是本项目的 —— 可能是别的 Mod
    Occupied { files: Vec<String> },
}

impl DeployState {
    pub fn label(&self) -> String {
        match self {
            DeployState::NotDeployed => "未部署".to_owned(),
            DeployState::Deployed { proxy, .. } => format!("已部署 ({})", proxy),
            DeployState::ManuallyInstalled { files } => {
                format!("已安装，非本工具部署（{}）", files.join("、"))
            }
            DeployState::Occupied { files } => format!("入口被占用: {}", files.join(", ")),
        }
    }

}

fn manifest_path(dir: &Path) -> Result<PathBuf> {
    Ok(util::backups_dir()?.join(util::dir_key(dir)).join("manifest.json"))
}

pub fn load_manifest(dir: &Path) -> Option<BackupManifest> {
    let p = manifest_path(dir).ok()?;
    let t = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&t).ok()
}

pub fn state_of(dir: &Path) -> DeployState {
    if let Some(m) = load_manifest(dir) {
        return DeployState::Deployed {
            proxy: m.proxy_name,
            deployed_at: m.deployed_at,
        };
    }
    // 没有 manifest，就靠签名判断目录里的文件是不是本项目的。
    // 这样用户自己手动安装的情况也能被识别出来，而不是一律显示「未部署」。
    let mut ours: Vec<String> = Vec::new();
    let mut others: Vec<String> = Vec::new();
    for e in PROXY_ENTRIES {
        let p = dir.join(e);
        if !p.is_file() {
            continue;
        }
        if crate::scan::identify_dll(&p).is_ours() {
            ours.push(e.to_owned());
        } else {
            others.push(e.to_owned());
        }
    }

    if !ours.is_empty() {
        // 顺便看看两个 DLSS 运行库在不在
        for n in ["nvngx_dlssg.dll", "nvngx_dlss.dll"] {
            if dir.join(n).is_file() {
                ours.push(n.to_owned());
            }
        }
        return DeployState::ManuallyInstalled { files: ours };
    }

    if others.is_empty() {
        DeployState::NotDeployed
    } else {
        DeployState::Occupied { files: others }
    }
}

/// 要部署的一个文件
#[derive(Debug, Clone)]
pub struct DeployFile {
    /// 部署到游戏目录里的文件名
    pub dest_name: String,
    /// 源文件路径
    pub src: PathBuf,
}

impl DeployFile {
    pub fn new(dest_name: &str, src: impl Into<PathBuf>) -> Self {
        Self {
            dest_name: dest_name.to_owned(),
            src: src.into(),
        }
    }
}

/// 部署。步骤：冲突检查 -> 占用检查 -> 备份 -> 写 manifest -> 原子写入 -> 校验（失败即回滚）
///
/// proxy 是代理入口名（决定按哪个入口判冲突），files 是本次要写入的全部文件。
pub fn deploy(target_dir: &Path, proxy: &str, files: &[DeployFile]) -> Result<String> {
    if !target_dir.is_dir() {
        bail!("目标目录不存在: {}", target_dir.display());
    }
    if !PROXY_ENTRIES.contains(&proxy) {
        bail!("不支持的代理入口: {}", proxy);
    }
    if files.is_empty() {
        bail!("没有要部署的文件");
    }
    for f in files {
        if !f.src.is_file() {
            bail!("缺少源文件: {}", f.src.display());
        }
    }

    let existing_manifest = load_manifest(target_dir);

    // 1. 冲突检查
    //
    // 关键点：目标位置已有同名文件时，先看它的签名者。
    //   - "DLSSG Native Project" 签名 -> 本项目之前装的，可以安全覆盖
    //   - 其它签名者 / 无签名        -> 第三方的，拒绝覆盖（除非它不是代理入口，
    //                                   比如游戏自带的 nvngx_dlssg.dll，那个本来就要升级）
    let mut overwrite_own: Vec<String> = Vec::new();
    let mut overwrite_other: Vec<String> = Vec::new();
    for f in files {
        let dst = target_dir.join(&f.dest_name);
        if !dst.is_file() {
            continue;
        }
        let is_proxy = crate::scan::PROXY_PRIORITY.contains(&f.dest_name.as_str());
        let is_dll = f.dest_name.to_ascii_lowercase().ends_with(".dll");

        if is_dll {
            let id = crate::scan::identify_dll(&dst);
            if id.is_ours() {
                overwrite_own.push(f.dest_name.clone());
                continue;
            }
            if is_proxy {
                bail!(
                    "{} 已存在，签名者是「{}」，不是本项目的文件，拒绝覆盖。请换个代理入口，或先手动备份/移除它。",
                    f.dest_name,
                    id.label()
                );
            }
            overwrite_other.push(format!("{}（{}）", f.dest_name, id.label()));
        } else if is_proxy {
            bail!("{} 已存在且不是本工具部署的，拒绝覆盖。", f.dest_name);
        }
    }

    let mut notes: Vec<String> = Vec::new();
    if !overwrite_own.is_empty() {
        notes.push(format!("已覆盖本项目的旧文件：{}", overwrite_own.join("、")));
    }
    if !overwrite_other.is_empty() {
        notes.push(format!(
            "已覆盖游戏原有的同名文件（已备份）：{}",
            overwrite_other.join("、")
        ));
    }
    if existing_manifest.is_none() {
        let others: Vec<&str> = PROXY_ENTRIES
            .iter()
            .copied()
            .filter(|e| {
                *e != proxy
                    && target_dir.join(e).is_file()
                    && !crate::scan::identify_dll(&target_dir.join(e)).is_ours()
            })
            .collect();
        if !others.is_empty() {
            notes.push(format!(
                "目录里还有 {}，不是本项目文件，建议确认是否冲突",
                others.join("、")
            ));
        }
    }

    // 2. 占用检查
    for f in files {
        let p = target_dir.join(&f.dest_name);
        if p.exists() && util::is_locked(&p) {
            bail!("{} 正被占用，请先完全退出游戏。", f.dest_name);
        }
    }

    // 3. 读源文件并算哈希
    let mut payload: Vec<(String, Vec<u8>, String)> = Vec::new();
    for f in files {
        let bytes = std::fs::read(&f.src).with_context(|| format!("读取 {}", f.src.display()))?;
        let sha = util::sha256_hex(&bytes);
        payload.push((f.dest_name.clone(), bytes, sha));
    }

    let key = util::dir_key(target_dir);
    let base = util::backups_dir()?.join(&key);
    let files_dir = base.join("files");
    std::fs::create_dir_all(&files_dir)?;

    // 4. 备份
    //
    // 重部署（上游更新后再点一次「部署」）时必须沿用**第一次**的备份。
    // 否则会把「我们上次部署进去的文件」当成游戏原文件重新备份一遍，
    // 把真正的原件覆盖掉 —— 之后「还原」只能还原出我们自己部署的版本。
    let prev: BTreeMap<String, BackupEntry> = existing_manifest
        .as_ref()
        .map(|m| m.files.iter().map(|e| (e.rel_path.clone(), e.clone())).collect())
        .unwrap_or_default();

    let mut entries = Vec::new();
    for (name, _bytes, sha) in &payload {
        // 之前部署过，而且它的原始备份还在 -> 直接沿用，不重新备份
        if let Some(p) = prev.get(name) {
            let backup_intact = match &p.backup_name {
                Some(bn) => files_dir.join(bn).is_file(),
                // 当时本来就没有原文件，那也不需要备份
                None => !p.existed_before,
            };
            if backup_intact {
                entries.push(BackupEntry {
                    rel_path: name.clone(),
                    existed_before: p.existed_before,
                    backup_name: p.backup_name.clone(),
                    original_sha256: p.original_sha256.clone(),
                    deployed_sha256: sha.clone(),
                });
                continue;
            }
        }

        let dst = target_dir.join(name);
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
        entries.push(BackupEntry {
            rel_path: name.clone(),
            existed_before: existed,
            backup_name,
            original_sha256,
            deployed_sha256: sha.clone(),
        });
    }

    // 5. 先写 manifest，保证后面任何失败都能回滚
    let manifest = BackupManifest {
        game_key: key,
        game_dir: target_dir.to_path_buf(),
        deployed_at: util::now_utc(),
        proxy_name: proxy.to_owned(),
        files: entries,
    };
    std::fs::write(
        base.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;

    // 6. 原子写入
    for (name, bytes, _sha) in &payload {
        let dst = target_dir.join(name);
        let tmp = target_dir.join(format!(".{}.tmp", name));
        std::fs::write(&tmp, bytes).with_context(|| format!("写入临时文件 {}", tmp.display()))?;
        util::atomic_replace(&tmp, &dst)?;
        util::clear_motw(&dst);
    }

    // 7. 校验，失败回滚
    for (name, _bytes, sha) in &payload {
        let got = util::sha256_file(&target_dir.join(name))?;
        if got != *sha {
            let _ = restore(target_dir);
            bail!("{} 部署后校验失败，已自动回滚。", name);
        }
    }

    // 8. 清掉上次部署、这次不再使用的代理入口
    //
    // 用户手动换代理入口再部署时，旧的那个会留在游戏目录里，而且新 manifest
    // 里没有它 —— 变成一个「还原也管不到」的孤儿，游戏加载哪个全看运气。
    // 判定条件很保守：必须是旧 manifest 记过账、当时没有原文件、而且现在这个
    // 文件的内容还等于我们当初写进去的那份（没被用户换过），才删。
    let mut removed: Vec<String> = Vec::new();
    if let Some(old) = &existing_manifest {
        for e in &old.files {
            if !PROXY_ENTRIES.contains(&e.rel_path.as_str()) || e.existed_before {
                continue;
            }
            if payload.iter().any(|(n, _, _)| *n == e.rel_path) {
                continue;
            }
            let p = target_dir.join(&e.rel_path);
            if !p.is_file() {
                continue;
            }
            let still_ours = util::sha256_file(&p)
                .map(|h| h == e.deployed_sha256)
                .unwrap_or(false);
            if still_ours && std::fs::remove_file(&p).is_ok() {
                removed.push(e.rel_path.clone());
            }
        }
    }
    if !removed.is_empty() {
        notes.push(format!(
            "已移除上次部署、这次不用的代理入口：{}（避免同时存在两个代理）",
            removed.join("、")
        ));
    }

    let names: Vec<&str> = payload.iter().map(|(n, _, _)| n.as_str()).collect();
    let mut msg = format!(
        "已部署 {} 个文件 -> {}：{}",
        names.len(),
        target_dir.display(),
        names.join("、")
    );
    if !notes.is_empty() {
        msg.push_str(" | ");
        msg.push_str(&notes.join(" | "));
    }
    Ok(msg)
}

fn restore_with(manifest: &BackupManifest) -> Result<String> {
    let target_dir = manifest.game_dir.as_path();
    let base = util::backups_dir()?.join(&manifest.game_key);

    // 移除本次部署的文件
    for e in &manifest.files {
        let p = target_dir.join(&e.rel_path);
        if p.is_file() {
            std::fs::remove_file(&p).with_context(|| format!("移除 {}", p.display()))?;
        }
    }

    // 还原备份
    let mut restored = 0usize;
    for e in &manifest.files {
        if !e.existed_before {
            continue;
        }
        let Some(bn) = &e.backup_name else { continue };
        let src = base.join("files").join(bn);
        let bytes = std::fs::read(&src).with_context(|| format!("读取备份 {}", src.display()))?;
        if let Some(orig) = &e.original_sha256 {
            if util::sha256_hex(&bytes) != *orig {
                bail!("备份 {} 自身已损坏，中止还原", bn);
            }
        }
        let dst = target_dir.join(&e.rel_path);
        let tmp = target_dir.join(format!(".{}.tmp", e.rel_path));
        std::fs::write(&tmp, &bytes)?;
        util::atomic_replace(&tmp, &dst)?;
        restored += 1;
    }

    let _ = std::fs::remove_dir_all(&base);
    Ok(format!(
        "已还原：移除 {} 个部署文件，恢复 {} 个原始文件",
        manifest.files.len(),
        restored
    ))
}

pub fn restore(target_dir: &Path) -> Result<String> {
    let Some(m) = load_manifest(target_dir) else {
        bail!("没有找到本工具对该目录的部署记录（{}）", target_dir.display());
    };
    restore_with(&m)
}
