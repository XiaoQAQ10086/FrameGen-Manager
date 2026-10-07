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
/// 插件**运行时**在旁边建的目录（INI 里 [Logging] Directory=dlssg_sm86\logs；
/// 相对路径的 bundle 缓存也可能落在这一层）。它不是我们部署的文件，所以清单里没有 ——
/// 但不清掉的话，用户点完「还原」目录里还杵着它（用户明确要求一并删除）。
pub const RUNTIME_DIR: &str = "dlssg_sm86";

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
pub struct DirLock(PathBuf);

impl DirLock {
    /// 拿这把锁。DLSS5 那边（dlss5.rs）也用它：同一个游戏目录的备份记录
    /// 不能被两个实例同时写，理由见上面这段。
    pub fn acquire(base: &Path) -> Result<Self> {
        std::fs::create_dir_all(base)?;
        let p = base.join(".lock");
        if let Some(lock) = try_create(&p) {
            return Ok(lock);
        }
        // 已经有一把锁：先看它是不是**崩溃残留**。
        // 为什么必须判（实测踩到）：sidecar 被强杀 / 任务管理器结束进程 / 断电之后，
        // 这个文件会留下来 —— 那时这个游戏就永远部署不了，界面只会让他去删一个
        // 藏在 %APPDATA% 备份目录里的文件，基本等于卡死。
        if lock_is_stale(&p) {
            crate::log::line(&format!("清理陈旧的部署锁：{}", p.display()));
            let _ = std::fs::remove_file(&p);
            if let Some(lock) = try_create(&p) {
                return Ok(lock);
            }
        }
        bail!(
            "另一个窗口正在操作这个游戏目录（{}）。等它结束后再试；\
             如果确认没有别的窗口在跑，把这个文件删掉即可。",
            p.display()
        )
    }
}

/// 独占创建锁文件，并把「谁拿的、什么时候拿的」写进去。
fn try_create(p: &Path) -> Option<DirLock> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(p)
        .ok()?;
    let _ = writeln!(f, "{}", std::process::id());
    let _ = writeln!(f, "{}", util::now_epoch_secs());
    Some(DirLock(p.to_path_buf()))
}

/// 锁文件是不是崩溃残留：写进去的进程号已经不在，或者时间戳超过 10 分钟。
/// 读不出来（空文件 / 内容坏了）也算残留 —— 那种锁谁也解释不了。
fn lock_is_stale(p: &Path) -> bool {
    /// 部署本身几秒到一两分钟，10 分钟足够把「真在跑」和「死锁」分开。
    const STALE_SECS: u64 = 600;
    let Ok(text) = std::fs::read_to_string(p) else {
        return true;
    };
    let mut it = text.lines();
    let pid: u32 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let when: u64 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    // 没有时间戳 = 老版本写的、或者写了一半的锁：归属根本判断不了，按残留处理
    // （实测：旧版留下的空 .lock 会把一个游戏永久锁死）。
    if when == 0 {
        return true;
    }
    if util::now_epoch_secs().saturating_sub(when) > STALE_SECS {
        return true;
    }
    pid > 0 && !process_alive(pid)
}

/// 那个进程还在吗（OpenProcess + GetExitCodeProcess == STILL_ACTIVE）。
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        let _ = CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE
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
    // 3b) 插件运行时在旁边建的目录（日志 / bundle 缓存）。它不是我们部署的文件，
    //     所以 manifest 里没有；但不删的话，用户点完「还原」目录里还留着它。
    let swept = if sweep_runtime_dir(target_dir) {
        vec![format!("{RUNTIME_DIR}\\")]
    } else {
        Vec::new()
    };

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
    let mut msg = format!("已还原：恢复 {restored} 个原始文件");
    if !swept.is_empty() {
        msg.push_str(&format!("；另清理插件运行时留下的目录：{}", swept.join("、")));
    }
    Ok(msg)
}

/// 删掉插件运行时建的 dlssg_sm86 目录，返回是否真删掉了。
fn sweep_runtime_dir(dir: &Path) -> bool {
    let p = dir.join(RUNTIME_DIR);
    p.is_dir() && std::fs::remove_dir_all(&p).is_ok()
}

/// 目录里「本项目的帧生成文件」清单 —— 给「用户手动装过、没有本工具记录」的目录用。
///
/// 判据（保守，避免误删别人的东西）：
///   1. 代理入口：**按签名认**（本项目的自签证书），不看文件名 —— 所以就算用户手动
///      放了 version.dll/winmm.dll…，只要是我们签的那份就能认出来；
///   2. 两份 DLSS 运行库：它们由 NVIDIA 官方签名，游戏自带的也是同一份签名，
///      光看文件分不出是谁放的 —— 所以**只在目录里确实有本项目的代理入口时**才算进来
///      （没有代理入口就不存在帧生成 mod，也就没必要动运行库）；
///   3. dlssg_sm86.ini：同理，只在有代理入口时才认。
pub fn manual_files(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in PROXY_ENTRIES {
        let p = dir.join(n);
        if p.is_file() && crate::scan::identify_dll(&p).is_ours() {
            out.push((*n).to_owned());
        }
    }
    if out.is_empty() {
        return out;
    }
    for n in ["nvngx_dlssg.dll", "nvngx_dlss.dll"] {
        if dir.join(n).is_file() {
            out.push(n.to_owned());
        }
    }
    if dir.join(INI_NAME).is_file() {
        out.push(INI_NAME.to_owned());
    }
    // 插件运行时建的目录（日志 / bundle 缓存）。同上：只有确实有本项目的代理入口时
    // 才算 —— 免得把一个碰巧同名、其实是用户自己东西的目录删掉。
    if dir.join(RUNTIME_DIR).is_dir() {
        out.push(format!("{RUNTIME_DIR}\\（整个目录）"));
    }
    out
}

/// 「没有部署记录」时的清理：把本项目的帧生成文件删掉。
///
/// 与 restore 的区别：restore 有备份、能把游戏原件放回去；这里**没有备份**，
/// 只能删（用户手动装的，或者记录丢了）。所以只删能确认是本项目的文件，
/// 删完复查一遍并如实报告。
pub fn cleanup_manual(dir: &Path) -> Result<String> {
    let files = manual_files(dir);
    if files.is_empty() {
        bail!(
            "这个目录里没有找到本项目的帧生成文件（{}）",
            dir.display()
        );
    }
    // 和其它写操作共用同一把锁：避免和另一个实例的部署/还原撞在一起
    let _lock = DirLock::acquire(&util::backups_dir()?.join(util::dir_key(dir)))?;
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    for n in &files {
        // 清单里可能带「（整个目录）」这种后缀（运行时目录），按实际类型删
        let raw = n.split('（').next().unwrap_or(n).trim_end_matches('\\');
        let p = dir.join(raw);
        let r = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        match r {
            Ok(_) => deleted.push(n.clone()),
            Err(e) => failed.push(format!("{n}（{e}）")),
        }
    }
    let mut msg = format!("已删除 {} 项：{}", deleted.len(), deleted.join("、"));
    if !failed.is_empty() {
        msg.push_str(&format!("\n· 没删掉的：{}", failed.join("、")));
    }
    msg.push_str(
        "\n· 注意：这是「没有备份记录」的清理 —— 那些文件本来就是你手动放进去的，游戏原件恢复不了。",
    );
    let left = manual_files(dir);
    if left.is_empty() {
        msg.push_str("\n· 复查通过：目录里已经没有本项目的帧生成文件了。");
    } else {
        msg.push_str(&format!("\n· 复查还有残留：{}", left.join("、")));
    }
    Ok(msg)
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

/// 部署前的准备结果（纯数据，不含任何 UI 状态）。
pub struct PreparedDeploy {
    pub files: Vec<DeployFile>,
    pub notes: Vec<String>,
}

/// 组装本次要写入的文件（代理 DLL + 缺的运行库 + INI）。
///
/// 与 egui 版 do_deploy() 的步骤一一对应，语义完全相同：
///   1. 代理 DLL 本体；
///   2. 游戏目录已有的 DLSS 运行库不动，缺的才补；
///   3. INI：出厂默认用资产原文件，改过档位才生成 deploy.ini。
pub fn prepare_deploy(
    dir: &Path,
    proxy: &str,
    fg_optimized: u8,
    fg_frames: u8,
) -> Result<PreparedDeploy> {
    let mut files = Vec::new();
    let mut notes = Vec::new();

    let dll = crate::update::asset_path(proxy)?;
    if !dll.is_file() {
        anyhow::bail!("资产缺失（{proxy}），请先「下载 / 更新资产」");
    }
    files.push(DeployFile::new(proxy, dll));

    let (have, need) = crate::update::runtime_deploy_plan(dir);
    for name in &have {
        notes.push(format!("游戏目录已有 {name}，用游戏自带的那份（不覆盖）"));
    }
    for name in &need {
        let p = crate::update::asset_path(name).unwrap_or_default();
        if !p.is_file() {
            anyhow::bail!("缺少运行库 {name}，请先「下载 / 更新资产」");
        }
        files.push(DeployFile::new(name, p));
    }

    let asset_ini = crate::update::asset_path(crate::update::INI_REPO_PATH).unwrap_or_default();
    let (ini_path, ini_notes) =
        crate::update::prepare_deploy_ini(&asset_ini, fg_optimized, fg_frames)?;
    for n in ini_notes {
        notes.push(n);
    }
    files.push(DeployFile::new(INI_NAME, ini_path));

    Ok(PreparedDeploy { files, notes })
}

/// 一步到位：准备 + 部署（返回部署说明与备注）。
pub fn deploy_game(
    dir: &Path,
    proxy: &str,
    fg_optimized: u8,
    fg_frames: u8,
) -> Result<(String, Vec<String>)> {
    let prep = prepare_deploy(dir, proxy, fg_optimized, fg_frames)?;
    let msg = deploy(dir, proxy, &prep.files, &[])?;
    Ok((msg, prep.notes))
}
