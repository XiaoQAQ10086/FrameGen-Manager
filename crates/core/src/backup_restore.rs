//! 部署 / 还原共用的「原件计划」（P1-7）。
//!
//! 三个业务各写过一遍同一套流程（读记录 → 备原件 → 写新文件 → 校验 → 失败回滚）：
//! `deploy.rs`（帧生成）、`sm.rs`（解锁组件）、`dlss5.rs`。这里只留下**一份**实现：
//!
//! | 步骤 | 函数 | 谁用 |
//! |---|---|---|
//! | 改动前把原件读进内存 | [`capture`] | 部署侧（deploy / sm） |
//! | 读一条原件并落成 `.bak` | [`snapshot_one`] | 部署侧（deploy / dlss5 / sm） |
//! | 还原前把备份读回来并校验 | [`load_originals`] | 还原 / 回滚侧（deploy / dlss5） |
//! | 写新文件（原子替换 + 校验 + 失败回滚） | [`apply`] | 部署侧（sm） |
//! | 按计划恢复（写回原件、删新建） | [`rollback`] | 回滚 / 还原 |
//!
//! 为什么不把 `deploy` / `dlss5` 的**写入**也换成 [`apply`]：它们的回滚必须按
//! **磁盘上的记录**走（重部署时会沿用上一次的原件备份，那份原件不在本次的快照里），
//! 所以这两处的回滚仍由各自的记录驱动，只是把「读备份」「写回原件」「删新建」
//! 换成了这里的实现。
//!
//! 这里所有函数都只做「一件事」，策略（谁该备份、哪些文件算我们的、文案怎么写）
//! 仍留在业务里 —— 回滚写错会让用户**丢原件**，判断不分散到公共层。

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::util;

/// 一条计划项：`(相对路径, 改动前是否存在, 改动前的字节)`。
///
/// `existed_before == false` 时 `bytes` 必须为空 —— 那个位置本来什么都没有，
/// 回滚时应当把文件删掉，而不是写回 0 字节的假原件。
pub type PlanEntry = (String, bool, Vec<u8>);

/// 一次部署 / 还原「会动到哪些文件、它们原来是什么样」。
///
/// 语义是**改动之前**的状态，不是要写进去的新内容：
/// 有了它，任何一步失败都能照着它把目录恢复成动手前的样子。
#[derive(Debug, Clone, Default)]
pub struct BackupPlan {
    pub entries: Vec<PlanEntry>,
}

impl BackupPlan {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 记一条。`existed_before == false` 时传空 `bytes`（回滚删文件即可）。
    pub fn push(&mut self, rel: impl Into<String>, existed_before: bool, bytes: Vec<u8>) {
        self.entries.push((rel.into(), existed_before, bytes));
    }

    /// 回滚时要**写回**的条目（原本就存在的那几份）。
    pub fn existing(&self) -> impl Iterator<Item = &PlanEntry> {
        self.entries.iter().filter(|e| e.1)
    }

    pub fn get(&self, rel: &str) -> Option<&PlanEntry> {
        self.entries.iter().find(|e| e.0 == rel)
    }

    /// 这一条在计划里的样子；没记过就返回 `None`（调用方据此判断「不在计划里」）。
    pub fn existed_before(&self, rel: &str) -> Option<bool> {
        self.get(rel).map(|e| e.1)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 备份文件名的统一约定：**扁平**，路径分隔符换成 `_`。
///
/// 0.4.0 的清单里有 `dlssg_sm86\backend\x.dll` 这类带子目录的条目：直接用路径拼进备份
/// 目录会变成嵌套路径，还原读回时还容易踩分隔符的坑（`\` 与 `/` 两种写法都存在）。
pub fn backup_file_name(rel: &str) -> String {
    format!("{}.bak", rel.replace(['\\', '/'], "_"))
}

/// 临时文件名：同样扁平，避免「目标在子目录里」时 tmp 也跟着嵌套。
fn tmp_path(dir: &Path, rel: &str) -> PathBuf {
    let flat = rel.replace(['\\', '/'], "_");
    dir.join(format!(".{flat}.fgm.tmp"))
}

/// 部署之前拍快照：把 `dir` 里这些相对路径的现有内容读进内存。
///
/// **只读**：不建目录、不改文件，目标不存在就记 `existed_before = false`。
///
/// 路径一律走 [`util::safe_join`]（清单会被篡改 / 损坏，不能让一个 JSON 决定读哪）。
pub fn capture<S: AsRef<str>>(dir: &Path, rels: &[S]) -> Result<BackupPlan> {
    let mut plan = BackupPlan::new();
    for rel in rels {
        let rel = rel.as_ref();
        let p = util::safe_join(dir, rel)?;
        if p.is_file() {
            let bytes = std::fs::read(&p).with_context(|| format!("备份 {}", p.display()))?;
            plan.push(rel, true, bytes);
        } else {
            plan.push(rel, false, Vec::new());
        }
    }
    Ok(plan)
}

/// 读一条原件、落成备份文件；没有原件返回 `None`。
///
/// 给「一个文件一个文件地铺」的流程用（deploy / dlss5 / sm 的部署循环）：
/// 返回原件字节，调用方自己算哈希、自己决定记不记账。
/// 备份文件名固定是 `files_dir/` 下的 [`backup_file_name`]（需要路径就照这个拼）。
pub fn snapshot_one(dir: &Path, rel: &str, files_dir: &Path) -> Result<Option<Vec<u8>>> {
    let p = util::safe_join(dir, rel)?;
    if !p.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(&p).with_context(|| format!("备份 {}", p.display()))?;
    std::fs::create_dir_all(files_dir)
        .with_context(|| format!("创建备份目录失败: {}", files_dir.display()))?;
    let bp = files_dir.join(backup_file_name(rel));
    std::fs::write(&bp, &bytes).with_context(|| format!("写入备份 {}", bp.display()))?;
    Ok(Some(bytes))
}

/// 备份目录里的一条原件（三个业务的记录字段名不一样，这里只取共同的三项）。
pub struct BackupRef<'a> {
    /// 相对路径（还原到哪）。
    pub rel: &'a str,
    /// 备份目录里的文件名（[`backup_file_name`] 生成的扁平名）。
    pub backup_name: &'a str,
    /// 当初记下的原件哈希；老记录可能没有，那就只读不校验。
    pub original_sha256: Option<&'a str>,
}

/// 还原之前把原件读回来：**全部读完并校验通过才返回**。
///
/// 为什么必须先读完再动手：备份坏掉时如果已经删了一半部署文件，游戏目录就停在
/// 「我们的删了、原件也没回来」的半残状态。所以这里任何一份坏了都直接 `Err`，
/// 调用方此时**一个字都还没写**。
///
/// 返回的 `BackupPlan` 里每条都是 `existed_before = true`：
/// 记录里 `existed_before = false`（本来就没有原件）的条目不该传进来。
pub fn load_originals(files_dir: &Path, refs: &[BackupRef<'_>]) -> Result<BackupPlan> {
    let mut plan = BackupPlan::new();
    for r in refs {
        let src = files_dir.join(r.backup_name);
        let bytes = std::fs::read(&src).with_context(|| format!("读取备份 {}", src.display()))?;
        if let Some(orig) = r.original_sha256 {
            if util::sha256_hex(&bytes) != *orig {
                bail!(
                    "备份 {} 自身已损坏，中止（目录一个字都没动）",
                    r.backup_name
                );
            }
        }
        plan.push(r.rel, true, bytes);
    }
    Ok(plan)
}

/// 要写进目标目录的一个新文件。
pub struct WriteItem {
    pub rel: String,
    /// 新内容的来源文件（走文件复制，不整份读进内存）。
    pub src: PathBuf,
    /// 期望的 sha256，写完逐个校验。
    pub sha256: String,
}

/// 部署写入：逐个原子替换进 `dir`，写完逐个校验；任何一步失败都按 `plan` 回滚。
///
/// `plan` 是**改动之前**的快照（[`capture`] 或 [`snapshot_one`] 得来的）：
/// 失败时按它把原件写回去，目录不会停在半部署状态。
pub fn apply(dir: &Path, plan: &BackupPlan, items: &[WriteItem]) -> Result<()> {
    for it in items {
        let dst = util::safe_join(dir, &it.rel)?;
        if let Some(parent) = dst.parent() {
            if !parent.is_dir() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建目录 {}", parent.display()))?;
            }
        }
        let tmp = util::long_path(&tmp_path(dir, &it.rel));
        let step = std::fs::copy(util::long_path(&it.src), &tmp)
            .with_context(|| format!("复制到 {} 失败", dst.display()))
            .and_then(|_| util::atomic_replace(&tmp, &dst));
        if let Err(e) = step {
            let _ = std::fs::remove_file(&tmp);
            return Err(with_rollback(e, dir, plan));
        }
        util::clear_motw(&dst);
    }
    for it in items {
        let p = util::safe_join(dir, &it.rel)?;
        let got = util::sha256_file(&p)?;
        if !got.eq_ignore_ascii_case(&it.sha256) {
            let e = anyhow!("{} 复制后校验不一致", it.rel);
            return Err(with_rollback(e, dir, plan));
        }
    }
    Ok(())
}

/// 失败信息里带上回滚的**真实**结果：回滚失败时不能只写「已回滚」（那是撒谎）。
fn with_rollback(e: anyhow::Error, dir: &Path, plan: &BackupPlan) -> anyhow::Error {
    match rollback(dir, plan) {
        Ok(r) => {
            let mut msg = format!(
                "{e}，已回滚（恢复 {} 个原件、删除 {} 个新文件）",
                r.restored, r.removed
            );
            if !r.failed.is_empty() {
                msg.push_str(&format!(
                    "；另有 {} 个文件没删掉（{}）",
                    r.failed.len(),
                    explain_failed(&r.failed)
                ));
            }
            anyhow!(msg)
        }
        Err(re) => anyhow!("{e}；**回滚也失败了**：{re}（请点「还原」再试一次）"),
    }
}

/// 回滚结果。两个数字都是**真的做到了**的数量，不是计划里的条数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackReport {
    pub restored: usize,
    pub removed: usize,
    /// 没删掉的文件（相对路径 + 系统给的原因）。**非空时调用方不能删部署记录** ——
    /// 记录一删，目录里剩下的文件就成了「本工具无记录」，用户只能去点「清理」，
    /// 原件再也放不回来。实测：游戏还开着 / 杀毒软件正在扫，version.dll 删不掉。
    pub failed: Vec<(String, String)>,
}

/// 把失败清单压成一句给人看的话（最多列 5 个，剩下的写数量）。
fn explain_failed(failed: &[(String, String)]) -> String {
    let names: Vec<String> = failed
        .iter()
        .take(5)
        .map(|(rel, why)| format!("{rel}（{why}）"))
        .collect();
    if failed.len() > 5 {
        format!("{} 等 {} 个", names.join("、"), failed.len())
    } else {
        names.join("、")
    }
}

/// 把计划里的原件写回 `dir`（原子替换），返回恢复的条数。
///
/// 只做「写回」这一件事：删哪些文件由调用方决定（`deploy` / `dlss5` 会跳过被用户改过的
/// 文件，那个判断不能下沉到这里 —— 判错就是替用户丢文件）。
pub fn restore_originals(dir: &Path, plan: &BackupPlan) -> Result<usize> {
    let mut restored = 0usize;
    for (rel, existed, bytes) in &plan.entries {
        if !existed {
            continue;
        }
        let dst = util::safe_join(dir, rel)?;
        if let Some(parent) = dst.parent() {
            if !parent.is_dir() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建目录 {}", parent.display()))?;
            }
        }
        let tmp = util::long_path(&tmp_path(dir, rel));
        std::fs::write(&tmp, bytes).with_context(|| format!("写入 {}", dst.display()))?;
        if let Err(e) = util::atomic_replace(&tmp, &dst) {
            // 失败时别把半成品留在游戏目录里（有的游戏 / 反作弊会扫目录里的未知文件）
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        restored += 1;
    }
    Ok(restored)
}

/// 删除重试：游戏 / 杀毒软件对新写入的 DLL 会短暂持有句柄，隔一会儿再删往往就成功了。
///
/// 为什么必须重试而不是删一次就算：删不掉时上层若仍按「还原成功」收尾，会把部署记录
/// 一起删掉 —— 目录里留下本工具的文件、记录却没了，界面显示「本工具无记录」，
/// 用户只能去点「清理」，游戏原件再也放不回来（1.6.3 用户实测就是这个现象）。
const DELETE_ATTEMPTS: usize = 5;
/// 第 2~5 次尝试前的等待（毫秒）。合计约 2 s，界面还在「还原中…」；够躲过杀毒软件
/// 对新写入 DLL 的那阵扫描，又不至于让真被游戏占着的文件把人晾太久。
const DELETE_BACKOFF_MS: [u64; DELETE_ATTEMPTS - 1] = [150, 300, 600, 1000];

/// 删一个文件，带重试与只读属性处理；文件已经不在（NotFound）视为删掉。
fn remove_file_with_retry(p: &Path) -> std::io::Result<()> {
    let mut waits = DELETE_BACKOFF_MS.iter().copied();
    loop {
        match std::fs::remove_file(p) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                // 退避表用完 = 这就是最后一次尝试：把系统的错误原样带回去
                let Some(wait_ms) = waits.next() else {
                    return Err(e);
                };
                // 只读属性也会让 remove_file 失败。这个文件本来就是我们部署进去的，
                // 去掉只读再删没有副作用。
                if let Ok(meta) = std::fs::metadata(p) {
                    let mut perms = meta.permissions();
                    if perms.readonly() {
                        #[allow(clippy::permissions_set_readonly_false)]
                        perms.set_readonly(false);
                        let _ = std::fs::set_permissions(p, perms);
                    }
                }
                std::thread::sleep(Duration::from_millis(wait_ms));
            }
        }
    }
}

/// 删掉计划里「原本不存在」的那些文件（这次新建的），返回真删掉的条数与失败清单。
///
/// 删不掉的**必须报出来**（以前只记不报）：文件可能正被游戏 / 杀毒占着，
/// 那不是致命错误，但调用方要知道「还没干净」—— 否则会把部署记录删掉，
/// 剩下的文件就再也对不上号了。
///
/// **只按路径删，不看内容** —— 所以调用方必须先确认这条确实是这次新建的。
pub fn remove_created(dir: &Path, plan: &BackupPlan) -> (usize, Vec<(String, String)>) {
    let mut removed = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();
    for (rel, existed, _) in &plan.entries {
        if *existed {
            continue; // 有原件的走 restore_originals，不能删
        }
        let p = match util::safe_join(dir, rel) {
            Ok(p) => p,
            Err(e) => {
                failed.push((rel.clone(), e.to_string()));
                continue;
            }
        };
        if !p.is_file() {
            continue; // 已经不在了（上一次还原删过 / 用户自己删了）
        }
        match remove_file_with_retry(&p) {
            Ok(()) => removed += 1,
            Err(e) => failed.push((rel.clone(), e.to_string())),
        }
    }
    (removed, failed)
}

/// 按计划把目录恢复成动手前的样子：先写回原件，再删新建的文件。
///
/// 顺序不能反：先删后写，中途失败会留下「我们的删了、原件也没回来」的状态。
pub fn rollback(dir: &Path, plan: &BackupPlan) -> Result<RollbackReport> {
    let restored = restore_originals(dir, plan)?;
    let (removed, failed) = remove_created(dir, plan);
    Ok(RollbackReport {
        restored,
        removed,
        failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 每个用例一个独立临时目录（`cargo test` 并行跑，名字里带纳秒）。
    fn temp_dir(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "fgm-backup-restore-{tag}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 原件进计划、缺失的记成「本来没有」；且 capture 不动磁盘。
    #[test]
    fn capture_reads_existing_and_marks_missing() {
        let dir = temp_dir("capture");
        std::fs::write(dir.join("version.dll"), b"ORIGINAL").unwrap();

        let plan = capture(&dir, &["version.dll", "nvngx_dlssg.dll"]).unwrap();

        assert_eq!(plan.len(), 2);
        assert!(plan.get("version.dll").unwrap().1);
        assert_eq!(plan.get("version.dll").unwrap().2, b"ORIGINAL");
        assert!(!plan.get("nvngx_dlssg.dll").unwrap().1);
        assert!(plan.get("nvngx_dlssg.dll").unwrap().2.is_empty());
        // 只读：目标目录里没有多出任何文件，原件也没被改
        assert_eq!(std::fs::read(dir.join("version.dll")).unwrap(), b"ORIGINAL");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 备份读回并校验：哈希对上才进计划，对不上必须中止（此时磁盘一个字都没动）。
    #[test]
    fn load_originals_verifies_each_backup() {
        let files_dir = temp_dir("load");
        let name = backup_file_name("dlssg_sm86\\backend\\nvngx_dlssg.dll");
        assert_eq!(name, "dlssg_sm86_backend_nvngx_dlssg.dll.bak");
        std::fs::write(files_dir.join(&name), b"ORIGINAL").unwrap();
        let good = util::sha256_hex(b"ORIGINAL");

        let plan = load_originals(
            &files_dir,
            &[BackupRef {
                rel: "dlssg_sm86\\backend\\nvngx_dlssg.dll",
                backup_name: &name,
                original_sha256: Some(&good),
            }],
        )
        .unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan.get("dlssg_sm86\\backend\\nvngx_dlssg.dll").unwrap().2,
            b"ORIGINAL"
        );

        let bad = load_originals(
            &files_dir,
            &[BackupRef {
                rel: "x.dll",
                backup_name: &name,
                original_sha256: Some(&util::sha256_hex(b"TAMPERED")),
            }],
        );
        let msg = format!("{:#}", bad.unwrap_err());
        assert!(msg.contains("损坏"), "错误信息应说明备份损坏：{msg}");

        std::fs::remove_dir_all(&files_dir).ok();
    }

    /// 清单里的路径越界（被篡改 / 损坏）时必须拒绝读取，不能拼出目录外的路径。
    #[test]
    fn capture_rejects_escaping_paths() {
        let dir = temp_dir("escape");
        assert!(capture(&dir, &["..\\evil.dll"]).is_err());
        assert!(capture(&dir, &["C:\\Windows\\System32\\x.dll"]).is_err());
        assert!(capture(&dir, &[""]).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 原件恢复：部署过之后回滚，文件内容必须回到动手前那一份。
    #[test]
    fn rollback_restores_originals() {
        let dir = temp_dir("restore");
        std::fs::write(dir.join("version.dll"), b"GAME-ORIGINAL").unwrap();
        let plan = capture(&dir, &["version.dll"]).unwrap();

        // 模拟部署：把文件换成我们的
        std::fs::write(dir.join("version.dll"), b"OURS").unwrap();
        assert_eq!(std::fs::read(dir.join("version.dll")).unwrap(), b"OURS");

        let r = rollback(&dir, &plan).unwrap();
        assert_eq!(r.restored, 1);
        assert_eq!(r.removed, 0);
        assert_eq!(
            std::fs::read(dir.join("version.dll")).unwrap(),
            b"GAME-ORIGINAL"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 回滚：本来没有的位置要删掉，别留一个 0 字节的假文件。
    #[test]
    fn rollback_deletes_files_that_did_not_exist() {
        let dir = temp_dir("remove");
        let plan = capture(&dir, &["nvngx_dlssg.dll"]).unwrap();
        assert_eq!(plan.existed_before("nvngx_dlssg.dll"), Some(false));

        std::fs::write(dir.join("nvngx_dlssg.dll"), b"OURS").unwrap();
        let r = rollback(&dir, &plan).unwrap();
        assert_eq!(r.restored, 0);
        assert_eq!(r.removed, 1);
        assert!(!dir.join("nvngx_dlssg.dll").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 无记录时直接写：计划里没有原件也能照常部署（写进去、校验通过）。
    #[test]
    fn apply_writes_when_nothing_existed() {
        let dir = temp_dir("apply-new");
        let src_dir = temp_dir("apply-new-src");
        std::fs::write(src_dir.join("new.dll"), b"NEW").unwrap();
        let plan = capture(&dir, &["sub\\new.dll"]).unwrap(); // 目标不存在 + 还在子目录里

        apply(
            &dir,
            &plan,
            &[WriteItem {
                rel: "sub\\new.dll".to_owned(),
                src: src_dir.join("new.dll"),
                sha256: util::sha256_hex(b"NEW"),
            }],
        )
        .unwrap();
        assert_eq!(std::fs::read(dir.join("sub\\new.dll")).unwrap(), b"NEW");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&src_dir).ok();
    }

    /// 写入失败（校验不一致）时必须自动回滚，目录回到动手前的样子。
    #[test]
    fn apply_rolls_back_when_verify_fails() {
        let dir = temp_dir("apply-rollback");
        let src_dir = temp_dir("apply-rollback-src");
        std::fs::write(dir.join("version.dll"), b"GAME-ORIGINAL").unwrap();
        std::fs::write(src_dir.join("version.dll"), b"OURS").unwrap();
        let plan = capture(&dir, &["version.dll", "dbghelp.dll"]).unwrap();

        let err = apply(
            &dir,
            &plan,
            &[
                WriteItem {
                    rel: "version.dll".to_owned(),
                    src: src_dir.join("version.dll"),
                    // 故意写错的期望值 -> 校验必然失败
                    sha256: util::sha256_hex(b"WRONG"),
                },
                WriteItem {
                    rel: "dbghelp.dll".to_owned(),
                    src: src_dir.join("version.dll"),
                    sha256: util::sha256_hex(b"OURS"),
                },
            ],
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("已回滚"), "错误信息应说明已回滚：{msg}");
        // 原件回来、新建的删掉
        assert_eq!(
            std::fs::read(dir.join("version.dll")).unwrap(),
            b"GAME-ORIGINAL"
        );
        assert!(!dir.join("dbghelp.dll").exists());

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&src_dir).ok();
    }

    /// 文件被别的进程占用（游戏 / 杀毒软件）时：必须报「没删掉」，不能假装删干净了。
    ///
    /// 这是 1.6.3 用户实测踩到的坑：还原报成功、部署记录被删，目录里却还留着
    /// version.dll，界面随即显示「本工具无记录」，只能再去点「清理」。
    #[cfg(windows)]
    #[test]
    fn remove_created_reports_locked_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp_dir("locked");
        // 先拍快照（原本没有这个文件），再模拟部署写进去
        let plan = capture(&dir, &["version.dll"]).unwrap();
        std::fs::write(dir.join("version.dll"), b"OURS").unwrap();

        // 只共享读：别的进程还能读，但删除（需要 DELETE 权限）会被拒绝 ——
        // 和游戏 LoadLibrary 之后的状态一致。
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(dir.join("version.dll"))
            .unwrap();

        let (removed, failed) = remove_created(&dir, &plan);
        assert_eq!(removed, 0);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, "version.dll");
        assert!(dir.join("version.dll").exists());

        drop(held);
        let (removed, failed) = remove_created(&dir, &plan);
        assert_eq!(removed, 1);
        assert!(failed.is_empty(), "放开占用后不该再报失败：{failed:?}");
        assert!(!dir.join("version.dll").exists());

        std::fs::remove_dir_all(&dir).ok();
    }
}
