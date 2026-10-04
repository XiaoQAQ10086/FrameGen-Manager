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

// 代理入口名单**只在 scan.rs 里硬编码一份**：`scan::PROXY_ALL` = 现用的 6 个
// （alternatives/ 那一套）+ 老归档包才有的 winhttp.dll。这里换成部署侧习惯的名字引用它，
// 免得两处各写一遍、以后上游改名时漏改一处。
//
// 为什么两个版本的名字都要认：用户从老版切到新版后，目录里残留的 winhttp.dll
// 会被当成第三方文件而拒绝处理（同一时刻只应存在一个代理入口）。
use crate::scan::PROXY_ALL as PROXY_ENTRIES;

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

/// 旧记录里的代理入口这次不用了，该怎么处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanAction {
    /// 不碰
    Keep,
    /// 我们新放的：直接从目录里删掉
    Remove,
    /// 那个位置**原本就有文件**（用户手动装的、或旧版本装的同项目文件）：
    /// 也要从目录里删掉（否则两个代理并存），但**备份记录必须留下来**，
    /// 否则「还原」再也放不回原件，就变成还原也管不到的孤儿。
    RemoveButKeepRecord,
}

/// 决定一条旧记录该怎么处置。
///
/// 抽成纯函数是为了能单独测：真实的代理 DLL 带本项目签名，自测里造不出来。
///
/// 判定故意保守 —— 只有「确实是我们当初写进去的那一份」才动：
///   * 不是代理入口 -> 不碰（ini 每次都在清单里，走不到这）
///   * 这次还要用 -> 不碰
///   * 文件已经不在了 -> 不碰
///   * 内容不是我们写的那份（用户换过）-> 不碰
pub fn plan_orphan(
    is_proxy: bool,
    in_payload: bool,
    file_present: bool,
    still_ours: bool,
    existed_before: bool,
) -> OrphanAction {
    if !is_proxy || in_payload || !file_present || !still_ours {
        return OrphanAction::Keep;
    }
    if existed_before {
        OrphanAction::RemoveButKeepRecord
    } else {
        OrphanAction::Remove
    }
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
                // 「非本工具部署」容易被读成「部署失败了」，这里说清是「没有本工具的记录」
                format!("已安装，本工具无记录（{}）", files.join("、"))
            }
            DeployState::Occupied { files } => format!("入口被占用: {}", files.join(", ")),
        }
    }

}

fn manifest_path(dir: &Path) -> Result<PathBuf> {
    Ok(util::backups_dir()?.join(util::dir_key(dir)).join("manifest.json"))
}

/// manifest 的状态。**「文件在但读不出来」必须和「压根没有这份文件」分开**：
/// 前者意味着我们可能把一个已部署目录当成从未部署过，那就会把我们自己放进去的
/// 文件记成「游戏原件」覆盖掉真正的原件 —— 后果是原件永久丢失。
pub enum ManifestState {
    Missing,
    Ok(Box<BackupManifest>),
    Broken(String),
}

/// 严格读一次 manifest：能区分「没有」和「坏了」。
pub fn manifest_state(dir: &Path) -> ManifestState {
    let Ok(p) = manifest_path(dir) else {
        return ManifestState::Missing;
    };
    if !p.is_file() {
        return ManifestState::Missing;
    }
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) => return ManifestState::Broken(e.to_string()),
    };
    match serde_json::from_str::<BackupManifest>(&text) {
        Ok(m) => ManifestState::Ok(Box::new(m)),
        Err(e) => ManifestState::Broken(e.to_string()),
    }
}

/// 写 manifest：**必须原子**。非原子写中途崩溃会留下半截 JSON，
/// 下次就被当成「没部署过」（见 manifest_state 的说明）。
fn write_manifest(base: &Path, m: &BackupManifest) -> Result<()> {
    let tmp = base.join("manifest.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(m)?)?;
    util::atomic_replace(&tmp, &base.join("manifest.json"))
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

/// 目标目录里有没有「本项目的另一个代理入口」——也就是本次不打算用、又不是本工具
/// 部署过的那些。
///
/// 为什么需要它：上游 README 明确要求「每次只保留本项目的一个代理」。如果旧的那个是
/// 用户**手动**装进去的，本工具的 manifest 里没有记录，原来那套清理就没有依据可查，
/// 而那道「目录里还有别的东西」的提醒又只针对非本项目文件 —— 结果两个代理并存，
/// 用户还以为部署成功了。所以界面上要拿这个结果去问用户一句。
pub fn find_extra_own_proxies(target_dir: &Path, proxy: &str) -> Vec<String> {
    let recorded: Vec<String> = load_manifest(target_dir)
        .map(|m| m.files.into_iter().map(|e| e.rel_path).collect())
        .unwrap_or_default();
    PROXY_ENTRIES
        .iter()
        .copied()
        .filter(|e| *e != proxy)
        .filter(|e| {
            let p = target_dir.join(e);
            p.is_file()
                && crate::scan::identify_dll(&p).is_ours()
                && !recorded.iter().any(|r| r == e)
        })
        .map(str::to_owned)
        .collect()
}

/// 部署。步骤：冲突检查 -> 占用检查 -> 备份 -> 写 manifest -> 原子写入 -> 校验（失败即回滚）
///
/// proxy 是代理入口名（决定按哪个入口判冲突），files 是本次要写入的全部文件。
/// extra_proxies 是用户在弹窗里确认要移除的「本项目的另一个代理入口」。
pub fn deploy(
    target_dir: &Path,
    proxy: &str,
    files: &[DeployFile],
    extra_proxies: &[String],
) -> Result<String> {
    if !target_dir.is_dir() {
        bail!("游戏目录不存在: {}", target_dir.display());
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

    // 记录读不出来就**不要**接着部署：那会把我们自己上次放进去的文件当成游戏原件
    // 覆盖掉真正的备份。停下来让用户处理，比悄悄毁掉原件强。
    let existing_manifest = match manifest_state(target_dir) {
        ManifestState::Missing => None,
        ManifestState::Ok(m) => Some(*m),
        ManifestState::Broken(e) => bail!(
            "这个目录的部署记录读不出来（{e}）。为免把游戏原件当成本工具的文件覆盖掉，已停止部署：请把 {} 改名或删除后再试。",
            manifest_path(target_dir)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "记录文件".to_owned())
        ),
    };

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
        if p.exists() {
            match util::lock_state(&p) {
                util::LockState::InUse => {
                    bail!("{} 正被占用，请先完全退出游戏。", f.dest_name)
                }
                util::LockState::NoAccess => bail!(
                    "{} 现在写不进去：文件带了只读属性，或者权限不足。去掉只读 / 换个可写的游戏目录再试。",
                    f.dest_name
                ),
                util::LockState::Free => {}
            }
        }
    }

    // 3. 读源文件并算哈希
    // **不再把源文件整份读进内存**：代理 DLL 30 MB，两个运行库加起来 60 多 MB，
    // 全量入内存会让一次部署瞬时多占近 90 MB（和「轻量」的定位冲突）。
    // 只算哈希，真正的内容在第 6 步按文件复制。
    let mut payload: Vec<(String, PathBuf, String)> = Vec::new();
    for f in files {
        let sha = util::sha256_file(&f.src)
            .with_context(|| format!("读取 {}", f.src.display()))?;
        payload.push((f.dest_name.clone(), f.src.clone(), sha));
    }

    let key = util::dir_key(target_dir);
    let base = util::backups_dir()?.join(&key);
    let files_dir = base.join("files");
    std::fs::create_dir_all(&files_dir)?;
    // 从这里开始就在动这个目录的备份了：先拿锁，避免两个实例同时写同一份记录
    let _lock = DirLock::acquire(&base)?;

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

    // 4b. 用户确认要移除的「另一个本项目代理」也必须先备份。
    // 记成 existed_before=true 的条目，这样「还原」还能把原件放回去 ——
    // 上游的步骤是「备份到单独目录，再移出游戏目录」，两步都不能少。
    // 已经在旧 manifest 里记过账的交给第 8 步处理，这里不重复备份
    // （重复备份会用当前文件覆盖掉真正的原件）。
    let mut extra_removed: Vec<String> = Vec::new();
    for name in extra_proxies {
        if !PROXY_ENTRIES.contains(&name.as_str()) || name == proxy {
            continue;
        }
        if prev.contains_key(name) {
            continue;
        }
        if entries.iter().any(|e| &e.rel_path == name) {
            continue;
        }
        let dst = target_dir.join(name);
        if !dst.is_file() {
            continue;
        }
        let orig = std::fs::read(&dst).with_context(|| format!("备份 {}", dst.display()))?;
        let sha = util::sha256_hex(&orig);
        let bn = format!("{name}.bak");
        std::fs::write(files_dir.join(&bn), &orig)?;
        entries.push(BackupEntry {
            rel_path: name.clone(),
            existed_before: true,
            backup_name: Some(bn),
            original_sha256: Some(sha.clone()),
            deployed_sha256: sha,
        });
        extra_removed.push(name.clone());
    }

    // 5. 先写 manifest，保证后面任何失败都能回滚
    let mut manifest = BackupManifest {
        game_key: key,
        game_dir: target_dir.to_path_buf(),
        deployed_at: util::now_utc(),
        proxy_name: proxy.to_owned(),
        files: entries,
    };
    write_manifest(&base, &manifest)?;

    // 6. 原子写入。**中途失败也要回滚**：manifest 已经落盘了，只写一半会让界面
    //    显示「已部署」而文件其实是混的。回滚的成败如实写进错误信息。
    for (name, src, _sha) in &payload {
        let dst = target_dir.join(name);
        let tmp = target_dir.join(format!(".{}.tmp", name));
        let step = std::fs::copy(src, &tmp)
            .with_context(|| format!("写入临时文件 {}", tmp.display()))
            .and_then(|_| util::atomic_replace(&tmp, &dst));
        if let Err(e) = step {
            let _ = std::fs::remove_file(&tmp);
            // 直接走内部函数：锁已经在本函数手里，不能再抢一次
            let rb = restore_with(&manifest);
            return Err(anyhow::anyhow!("{e}；回滚{}", rollback_text(&rb)));
        }
        util::clear_motw(&dst);
    }

    // 7. 校验，失败回滚
    for (name, _bytes, sha) in &payload {
        let got = util::sha256_file(&target_dir.join(name))?;
        if got != *sha {
            let rb = restore_with(&manifest);
            bail!("{} 部署后校验失败；回滚{}", name, rollback_text(&rb));
        }
    }

    // 8. 清掉这次不用的代理入口
    //
    // 三个来源，规则见 plan_orphan：
    //   * 用户在弹窗里确认要移除的「另一个本项目代理」（extra_removed）—— 上面已备份
    //   * 旧 manifest 记过账、这次不用的：本来空着、是我们放的就直接删；那位置
    //     **原本就有文件**的也要删（不然两个代理并存），但备份记录必须搬进新
    //     manifest，否则「还原」再也放不回原件
    //   * 其它：不碰
    //
    // 另外：凡是有原件备份、这次又不部署的条目，记录都要一直传下去 —— 包括文件
    // 已经不在了的。因为「还原」收尾会把整个备份目录删掉，记录一丢，原件就永久
    // 拿不回来了。
    let mut removed: Vec<String> = Vec::new();
    let mut carried: Vec<BackupEntry> = Vec::new();
    let mut kept_record: Vec<String> = Vec::new();
    // 移不掉的（被占用 / 只读 / 权限）：必须说出来，而且记录要留着 ——
    // 否则界面显示「已部署」、目录里却还立着本项目的旧代理，「还原」也清不掉。
    let mut failed_remove: Vec<String> = Vec::new();

    for name in &extra_removed {
        let p = target_dir.join(name);
        if std::fs::remove_file(&p).is_ok() {
            kept_record.push(name.clone());
        } else {
            failed_remove.push(name.clone());
        }
    }

    if let Some(old) = &existing_manifest {
        for e in &old.files {
            if payload.iter().any(|(n, _, _)| *n == e.rel_path) {
                continue;
            }
            let p = target_dir.join(&e.rel_path);
            let present = p.is_file();
            let still_ours = present
                && util::sha256_file(&p)
                    .map(|h| h == e.deployed_sha256)
                    .unwrap_or(false);
            let has_backup = e.existed_before && e.backup_name.is_some();
            let action = plan_orphan(
                PROXY_ENTRIES.contains(&e.rel_path.as_str()),
                false,
                present,
                still_ours,
                e.existed_before,
            );
            let mut keep_record = false;
            match action {
                OrphanAction::Keep => {
                    // 只要有原件备份，这条记录**必须**传下去：还原收尾会把整个备份
                    // 目录删掉，记录一丢，那份原件就永久拿不回来了。
                    // （以前这里还要求「文件不在了或还是我们那份」，于是用户自己改过
                    //   那个文件时记录被丢掉、备份被连带删除 —— 和本文件的约定相反。）
                    keep_record = has_backup;
                }
                OrphanAction::Remove => {
                    if std::fs::remove_file(&p).is_ok() {
                        removed.push(e.rel_path.clone());
                    } else {
                        // 删不掉：记录留着，下次还原还能处理它
                        failed_remove.push(e.rel_path.clone());
                        keep_record = has_backup;
                    }
                }
                OrphanAction::RemoveButKeepRecord => {
                    if std::fs::remove_file(&p).is_ok() {
                        kept_record.push(e.rel_path.clone());
                    } else {
                        failed_remove.push(e.rel_path.clone());
                    }
                    keep_record = has_backup;
                }
            }
            if keep_record && !carried.iter().any(|c| c.rel_path == e.rel_path) {
                carried.push(e.clone());
            }
        }
    }

    if !removed.is_empty() {
        notes.push(format!(
            "已移除上次部署、这次不用的代理入口：{}（避免同时存在两个代理）",
            removed.join("、")
        ));
    }
    if !kept_record.is_empty() {
        notes.push(format!(
            "已按你的选择移出本项目的其它代理入口：{}（上游要求每次只留一个），原件在备份里，点「还原」可以恢复",
            kept_record.join("、")
        ));
    }
    if !failed_remove.is_empty() {
        notes.push(format!(
            "没能移出这些旧代理入口（可能正被占用或只读）：{} —— 本次记录已保留，下次点「还原」仍可处理",
            failed_remove.join("、")
        ));
    }

    if !carried.is_empty() {
        // 把记录补进刚落盘的新 manifest，否则「还原」找不到它们
        manifest
            .files
            .retain(|m| !carried.iter().any(|c| c.rel_path == m.rel_path));
        manifest.files.extend(carried);
        write_manifest(&base, &manifest)?;
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

/// 「这个游戏目录正在被操作」的锁文件。
///
/// 两个实例（或两个解压目录）同时部署/还原同一个游戏目录时，会往同一份 manifest
/// 和同一批 .bak 上写 —— 互相覆盖之后原件就找不回来了。UI 里的 busy 只挡得住同一个进程。
struct DirLock(PathBuf);

impl DirLock {
    fn acquire(base: &Path) -> Result<Self> {
        std::fs::create_dir_all(base)?;
        let p = base.join(".lock");
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(_) => Ok(DirLock(p)),
            Err(_) => bail!(
                "另一个窗口正在操作这个游戏目录（{}）。等它结束、或者删掉这个文件再试。",
                p.display()
            ),
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 把回滚的成败说清楚。以前无论成败都写「已自动回滚」—— 回滚失败时那是撒谎。
fn rollback_text(r: &Result<String>) -> String {
    match r {
        Ok(m) => format!("成功（{m}）"),
        Err(e) => {
            format!("**失败**：{e}（游戏目录可能停在半部署状态，请点「还原」再试一次）")
        }
    }
}

fn restore_with(manifest: &BackupManifest) -> Result<String> {
    let target_dir = manifest.game_dir.as_path();
    let base = util::backups_dir()?.join(&manifest.game_key);

    // 1) 先把所有备份读出来校验完。**任何一份坏了都要在动游戏目录之前中止** ——
    //    以前是先无条件删掉部署文件、再逐个读备份：备份坏掉时游戏目录已经被删了
    //    一半，原件又没放回去，留下一个半残的游戏目录。
    let mut plan: Vec<(&BackupEntry, Vec<u8>)> = Vec::new();
    for e in &manifest.files {
        if !e.existed_before {
            continue;
        }
        let Some(bn) = &e.backup_name else { continue };
        let src = base.join("files").join(bn);
        let bytes = std::fs::read(&src).with_context(|| format!("读取备份 {}", src.display()))?;
        if let Some(orig) = &e.original_sha256 {
            if util::sha256_hex(&bytes) != *orig {
                bail!("备份 {bn} 自身已损坏，中止还原（游戏目录一个字都没动）");
            }
        }
        plan.push((e, bytes));
    }

    // 2) 只删「确认还是我们部署的那一份」的文件。用户换成别的 mod、或游戏更新
    //    了自己那份 nvngx_dlssg.dll 之后，那个文件已经不是我们的了 —— 删掉它
    //    等于替用户丢文件，而且没有备份能救。
    let mut ours: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for e in &manifest.files {
        let p = target_dir.join(&e.rel_path);
        if !p.is_file() {
            continue;
        }
        let still_ours = !e.deployed_sha256.is_empty()
            && util::sha256_file(&p)
                .map(|h| h == e.deployed_sha256)
                .unwrap_or(false);
        if still_ours {
            ours.push(e.rel_path.clone());
        } else {
            skipped.push(e.rel_path.clone());
        }
    }

    // 3) 先把原件放回去，再删我们放进去的那些。顺序反过来的话，中途失败会留下
    //    「我们的删了、原件也没回来」的状态。
    let mut restored = 0usize;
    for (e, bytes) in &plan {
        let dst = target_dir.join(&e.rel_path);
        let tmp = target_dir.join(format!(".{}.tmp", e.rel_path));
        std::fs::write(&tmp, bytes)?;
        if let Err(e) = util::atomic_replace(&tmp, &dst) {
            // 失败时别把半成品留在游戏目录里（有的游戏/反作弊会扫目录里的未知文件）
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        restored += 1;
        ours.retain(|n| n != &e.rel_path);
    }
    for name in &ours {
        let _ = std::fs::remove_file(target_dir.join(name));
    }

    // 4) 有文件被改过就保留备份目录、如实说明：别把「原件还在」这一点也弄丢
    if !skipped.is_empty() {
        return Ok(format!(
            "部分还原：恢复 {restored} 个原始文件；{} 个文件的内容已不是本工具部署的那份，为避免误删已跳过（{}）。备份保留在 {}，确认不需要了可以手动删除。",
            skipped.len(),
            skipped.join("、"),
            base.display()
        ));
    }
    let _ = std::fs::remove_dir_all(&base);
    Ok(format!("已还原：恢复 {restored} 个原始文件"))
}

pub fn restore(target_dir: &Path) -> Result<String> {
    let Some(m) = load_manifest(target_dir) else {
        bail!("没有找到本工具对该目录的部署记录（{}）", target_dir.display());
    };
    // 和 deploy 用同一把锁：两个实例同时读写同一份备份会互相覆盖 ——
    // 那正是「原件找不回来」的典型路径。加在读到记录之后，
    // 这样「本来就没部署过」的目录不会平白多一个锁文件。
    let _lock = DirLock::acquire(&util::backups_dir()?.join(&m.game_key))?;
    restore_with(&m)
}
