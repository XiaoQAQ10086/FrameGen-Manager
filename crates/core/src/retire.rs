//! 1.7.0 停更退场：把本工具在**所有游戏**里的部署痕迹清干净。
//!
//! 为什么单独一个模块：这不是「某个游戏的还原」，而是升级到最后一个小版本时的一次性
//! 退场动作 —— 用户装完 1.7.0 后，程序要自动把帧生成 / Smooth Motion / DLSS5 的部署
//! 全部撤掉，再弹公告让用户去新项目。动作必须：
//!   * 覆盖「游戏库里有的」和「库里删了但备份记录还在的」两类目录；
//!   * 一类失败不影响其它类，最后如实报告哪些没清掉（绝不谎报「已清除」）；
//!   * 只有全部成功才删备份记录 —— 否则下次启动还要靠记录重试。
//!
//! 删除范围与各自的手动还原/卸载完全一致（复用 deploy / sm / dlss5 的实现），
//! 这里只负责「找目标 + 按顺序调用 + 汇总」。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::deploy;
use crate::dlss5;
use crate::scan;
use crate::sm;
use crate::util;

#[derive(Debug, Default, Serialize)]
pub struct RetireReport {
    /// 真正动过、并且复查通过的游戏（按名字去重）
    pub cleaned: Vec<String>,
    /// 有文件没删掉 / 还原失败的游戏（带原因）
    pub failed: Vec<String>,
    /// 不影响「文件已清干净」的提醒（例如没有 NVIDIA 驱动时无法写回 SM 配置）
    pub warnings: Vec<String>,
    /// 扫描到的目标目录数
    pub targets: usize,
    /// 最后清掉的备份记录条数 / 字节
    pub backups_removed: usize,
    pub backups_bytes: u64,
}

impl RetireReport {
    /// 游戏目录里**没有留下部署文件**才算成功。
    ///
    /// 驱动配置写回失败（例如这台机器现在没有 NVIDIA 驱动）只进 warnings：
    /// 那不会在游戏目录里留文件，也不该让停更公告永远弹不出来。此时备份记录会保留，
    /// 公告里如实附上提醒。
    pub fn ok(&self) -> bool {
        self.failed.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
struct Target {
    dir: PathBuf,
    name: String,
    exe: Option<PathBuf>,
    fg: bool,
    sm_record: bool,
    sm_asset: bool,
    dlss5: bool,
}

fn target_key(dir: &str) -> String {
    PathBuf::from(dir.trim())
        .display()
        .to_string()
        .trim_end_matches(['\\', '/'])
        .to_lowercase()
}

fn add_target(map: &mut BTreeMap<String, Target>, dir: &str, name: &str, exe: Option<&str>) {
    let dir = dir.trim();
    if dir.is_empty() {
        return;
    }
    let p = PathBuf::from(dir);
    let key = target_key(dir);
    let slot = map.entry(key).or_insert_with(|| Target {
        dir: p.clone(),
        name: name.trim().to_owned(),
        ..Target::default()
    });
    if slot.name.is_empty() && !name.trim().is_empty() {
        slot.name = name.trim().to_owned();
    }
    if slot.exe.is_none() {
        if let Some(e) = exe.map(str::trim).filter(|e| !e.is_empty()) {
            slot.exe = Some(PathBuf::from(e));
        }
    }
    if slot.name.is_empty() {
        slot.name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| p.display().to_string());
    }
}

fn json_str(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            if !s.trim().is_empty() {
                return Some(s.to_owned());
            }
        }
    }
    None
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 从备份目录里把「记录里出现过的游戏目录」全部找回来。
///
/// 这些记录各有各的格式（帧生成 manifest / SM profile / SM 组件 / DLSS5 manifest），
/// 所以不按固定结构反序列化，只认几个「目录字段」的并集。
fn collect_backup_targets(base: &Path, map: &mut BTreeMap<String, Target>) {
    let Ok(rd) = std::fs::read_dir(base) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if name == "gpu-name" {
            continue;
        }
        if name == "sm-profiles" {
            for f in json_files(&p) {
                if let Some(v) = read_json(&f) {
                    let game = json_str(&v, &["game"]).unwrap_or_default();
                    let exe = json_str(&v, &["exe"]);
                    if let Some(dir) = json_str(&v, &["dir", "game_dir", "install_dir"]) {
                        add_target(map, &dir, &game, exe.as_deref());
                        if let Some(t) = map.get_mut(&target_key(&dir)) {
                            t.sm_record = true;
                        }
                    }
                }
            }
            continue;
        }
        if name == "sm-assets" {
            for f in json_files(&p) {
                if let Some(v) = read_json(&f) {
                    if let Some(dir) = json_str(&v, &["dir", "game_dir", "install_dir"]) {
                        let game = json_str(&v, &["game"]).unwrap_or_default();
                        add_target(map, &dir, &game, None);
                        if let Some(t) = map.get_mut(&target_key(&dir)) {
                            t.sm_asset = true;
                        }
                    }
                }
            }
            continue;
        }
        let mf = p.join("manifest.json");
        let Some(v) = read_json(&mf) else { continue };
        let dir = json_str(
            &v,
            &["game_dir", "target_dir", "deploy_dir", "dir", "install_dir"],
        );
        let Some(dir) = dir else { continue };
        let game = json_str(&v, &["game", "game_name", "name"]).unwrap_or_default();
        let exe = json_str(&v, &["exe", "game_exe"]);
        add_target(map, &dir, &game, exe.as_deref());
        if let Some(t) = map.get_mut(&target_key(&dir)) {
            if name.ends_with("-dlss5") {
                t.dlss5 = true;
            } else {
                t.fg = true;
            }
        }
    }
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map(|x| x == "json").unwrap_or(false) {
            out.push(p);
        }
    }
    out
}

fn all_targets() -> BTreeMap<String, Target> {
    let mut map: BTreeMap<String, Target> = BTreeMap::new();
    // 1) 游戏库（自动扫描 + 手动添加）—— 覆盖「没有记录、手动装过」的情况
    let lib = scan::load_library();
    for row in lib.scanned.iter().chain(lib.manual.iter()) {
        let exe = row.render_exe.as_ref().map(|p| p.display().to_string());
        add_target(
            &mut map,
            &row.entry.install_dir.display().to_string(),
            &row.entry.name,
            exe.as_deref(),
        );
    }
    // 2) 备份记录 —— 覆盖「已经从库里有删掉、但记录还在」的目录
    if let Ok(base) = util::backups_dir() {
        collect_backup_targets(&base, &mut map);
    }
    map
}

fn add_cleaned(report: &mut RetireReport, name: &str) {
    if !report.cleaned.iter().any(|x| x == name) {
        report.cleaned.push(name.to_owned());
    }
}

fn cleanup_one(t: &Target, report: &mut RetireReport) -> bool {
    let dir = &t.dir;
    let label = if t.name.is_empty() {
        dir.display().to_string()
    } else {
        t.name.clone()
    };
    if !dir.is_dir() {
        // 目录不在：多半是游戏已卸载（磁盘还在），备份可以跟着删；
        // 但整个盘符/网络位置都不可用时，可能是移动硬盘或 NAS 暂时没接，
        // 不能当成「已清干净」把备份删了 —— 留一条提醒，备份保留。
        let root = dir.ancestors().last().unwrap_or(dir);
        if !root.exists() {
            report.warnings.push(format!(
                "{label}：游戏目录所在位置当前不可用（{}），相关备份记录已保留",
                dir.display()
            ));
        }
        return false;
    }
    let mut did = false;
    let exe = t
        .exe
        .clone()
        .or_else(|| scan::find_render_exe(dir))
        .unwrap_or_default();

    // ---- 帧生成：有记录走还原（能放回游戏原件），没记录但有我们的文件只删 ----
    if t.fg || deploy::load_manifest(dir).is_some() {
        match deploy::restore_report(dir) {
            Ok(o) => {
                if o.complete() {
                    did = true;
                } else {
                    report.failed.push(format!(
                        "{label}：帧生成没有完全还原（{}）",
                        o.leftover.join("、")
                    ));
                }
            }
            Err(e) => {
                if deploy::manual_files(dir).is_empty() {
                    // 记录读不出来、目录里也没有我们的文件：按已清掉处理
                    crate::log::line(&format!("[退场] {label} 没有帧生成部署记录：{e}"));
                } else {
                    match deploy::cleanup_manual(dir) {
                        Ok(_) => did = true,
                        Err(x) => report
                            .failed
                            .push(format!("{label}：帧生成清理失败（{x}）")),
                    }
                }
            }
        }
    } else if !deploy::manual_files(dir).is_empty() {
        match deploy::cleanup_manual(dir) {
            Ok(_) => did = true,
            Err(e) => report
                .failed
                .push(format!("{label}：帧生成清理失败（{e}）")),
        }
    }

    // ---- 0.3.x 遗留文件：只按哈希删，游戏自带运行库受保护 ----
    let (deleted, failed) = deploy::clean_legacy_leftovers(dir);
    if !deleted.is_empty() {
        did = true;
        crate::log::line(&format!(
            "[退场] {label} 清掉旧版帧生成文件：{}",
            deleted.join("、")
        ));
    }
    for f in failed {
        report
            .failed
            .push(format!("{label}：旧版帧生成文件没删掉（{f}）"));
    }

    // ---- Smooth Motion 组件（version.dll）----
    if t.sm_asset || sm::has_our_asset(dir) {
        let before = sm::has_our_asset(dir);
        match sm::asset_restore_searching(dir, &exe) {
            Ok(msg) => {
                if msg.starts_with("已") {
                    did = true;
                    crate::log::line(&format!("[退场] {label} SM 组件：{msg}"));
                }
                let after = sm::has_our_asset(dir);
                if before && after {
                    report
                        .failed
                        .push(format!("{label}：Smooth Motion 组件仍留在目录里"));
                }
            }
            Err(e) => report
                .failed
                .push(format!("{label}：Smooth Motion 组件取回失败（{e}）")),
        }
    }

    // ---- Smooth Motion 驱动配置 ----
    if t.sm_record {
        match sm::restore_game_sm(&label, dir, &exe) {
            Ok(msg) => {
                if msg.contains("已") {
                    did = true;
                    crate::log::line(&format!("[退场] {label} SM 配置：{msg}"));
                }
            }
            Err(e) => report.warnings.push(format!(
                "{label}：Smooth Motion 驱动配置无法写回（{e}）；备份记录已保留"
            )),
        }
    }

    // ---- DLSS5 / ReShade ----
    let state = dlss5::state_of(dir);
    let recorded = !matches!(state, dlss5::State::NotDeployed);
    let found = dlss5::detect(dir);
    if found.anything() && (found.ours || recorded) {
        let exe_opt = if exe.is_file() {
            Some(exe.as_path())
        } else {
            None
        };
        match dlss5::uninstall(dir, exe_opt, &mut |_, _| {}) {
            Ok(msg) => {
                did = true;
                crate::log::line(&format!("[退场] {label} DLSS5：{msg}"));
                // uninstall 自己只把残留在说明里写出来，不返回错误。退场要求「不留残余」，
                // 所以必须再查一次：还在就直接算失败，下次启动继续清。
                let left = dlss5::detect(dir);
                if left.anything() {
                    report
                        .failed
                        .push(format!("{label}：DLSS5 还有残留（{}）", left.label()));
                }
            }
            Err(e) => {
                // 记录在、文件已经被用户手动删掉的情况不算失败
                let left = dlss5::detect(dir);
                if left.anything() {
                    report
                        .failed
                        .push(format!("{label}：DLSS5 卸载失败（{e}）"));
                }
            }
        }
    }

    // 组件/插件都删完后，空的 dlssg_sm86 目录也一并去掉（只删空目录）
    let runtime = dir.join("dlssg_sm86");
    if runtime.is_dir() {
        let empty = std::fs::read_dir(&runtime)
            .map(|mut rd| rd.next().is_none())
            .unwrap_or(false);
        if empty && std::fs::remove_dir(&runtime).is_ok() {
            did = true;
        }
    }

    if did {
        add_cleaned(report, &label);
    }
    did
}

/// 退场清理总入口。只返回结果，不在这里改配置 —— 调用方（sidecar）负责把
/// `retired` 标志落盘，这样「失败了下次还能重试」和「成功了不再重复」都可控。
pub fn cleanup_all() -> RetireReport {
    let mut report = RetireReport::default();
    let targets = all_targets();
    report.targets = targets.len();
    crate::log::line(&format!("[退场] 开始清理：目标目录 {} 个", report.targets));
    for t in targets.values() {
        cleanup_one(t, &mut report);
    }
    // 只有「文件全清干净 + 配置也写回成功」才删备份记录。
    // 有失败时下次启动还要靠记录找到目录、把原件放回去；只有配置提醒时，
    // 备份也是那份配置唯一的依据，不能为了「不留残余」把它删掉。
    if !report.ok() {
        report.warnings.push(
            "有部署没有完全清除，备份记录已保留（关闭游戏后重启本程序会自动重试）".to_owned(),
        );
    } else if !report.warnings.is_empty() {
        report.warnings.push(
            "有项目没能完全清理（配置写回失败或目录暂时不可用），相关备份记录已保留（游戏目录里的文件已尽量清干净）"
                .to_owned(),
        );
    } else {
        match util::clean_backups() {
            Ok((n, bytes)) => {
                report.backups_removed = n;
                report.backups_bytes = bytes;
            }
            Err(e) => report.warnings.push(format!("备份记录清理失败：{e}")),
        }
        // clean_backups 只写日志、不把「删不掉」当错误。退场要求不留残余：
        // 这里再数一遍，删不掉就把整次清理判为失败，下次启动继续重试。
        let left = util::backups_entries()
            .into_iter()
            .filter(|(_, kind, _, _)| kind != "gpu-name")
            .count();
        if left > 0 {
            report.failed.push(format!(
                "还有 {left} 份备份记录没有删掉（可能被游戏或安全软件占用）"
            ));
        }
    }
    crate::log::line(&format!(
        "[退场] 完成：清理 {} 个，失败 {} 个，提醒 {} 条，备份 {} 份",
        report.cleaned.len(),
        report.failed.len(),
        report.warnings.len(),
        report.backups_removed
    ));
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "fgm-retire-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn collects_targets_from_every_record_layout() {
        let base = temp_dir("collect");
        let prof = base.join("sm-profiles");
        let assets = base.join("sm-assets");
        let d5 = base.join("abc-dlss5");
        let fg = base.join("xyz");
        let gpu = base.join("gpu-name");
        for d in [&prof, &assets, &d5, &fg, &gpu] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            prof.join("a.json"),
            r#"{"game":"Game A","exe":"C:/A/a.exe","dir":"C:/A"}"#,
        )
        .unwrap();
        std::fs::write(assets.join("b.json"), r#"{"dir":"C:/B"}"#).unwrap();
        std::fs::write(
            d5.join("manifest.json"),
            r#"{"game_dir":"C:/C","exe":"C:/C/c.exe"}"#,
        )
        .unwrap();
        std::fs::write(
            fg.join("manifest.json"),
            r#"{"game_dir":"C:/D","target_dir":"ignored"}"#,
        )
        .unwrap();
        std::fs::write(gpu.join("manifest.json"), r#"{"dir":"C:/GPU"}"#).unwrap();

        let mut map = BTreeMap::new();
        collect_backup_targets(&base, &mut map);
        assert_eq!(map.len(), 4, "{map:?}");
        let a = map.get("c:/a").unwrap();
        assert!(a.sm_record && a.name == "Game A");
        assert!(map.get("c:/b").unwrap().sm_asset);
        assert!(map.get("c:/c").unwrap().dlss5);
        assert!(map.get("c:/d").unwrap().fg);
        assert!(!map.contains_key("c:/gpu"), "gpu-name 不是游戏目录");
        let _ = std::fs::remove_dir_all(&base);
    }
}
