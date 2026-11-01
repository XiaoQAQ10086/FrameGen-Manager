//! 运行日志。
//!
//! 目的很具体：用户反馈问题时把 logs 目录里的文件发过来，我们就能从里面看出
//! 「探测用了哪个源、拿到什么指纹、本地记录的是什么、每次尝试是成功还是失败、
//! 传了多少字节」这些东西 —— 没有这些，光看现象只能靠猜。
//!
//! **日志里会有本地路径、Windows 用户名、游戏安装目录。** 界面上要让用户知道，
//! 分享之前自己先看一眼。
//!
//! 实现刻意做得糙而稳：一个全局文件句柄加一把锁，每行写完立刻 flush（进程被强杀
//! 也留得下内容），任何一步失败都静默忽略 —— 日志绝不能把主流程搞崩。
//!
//! **目录里永远只有两个文件**：framegen.log（本次运行）+ framegen.prev.log（上一次运行）。
//! 用固定名字滚动覆盖：上一次的改名成 prev，更早的删掉 —— 否则一次运行多一个文件，
//! 点开一次多一个，看着乱。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::util;

/// 本次运行的日志文件名（固定名字：每次启动都复用同一个，不再一启动多一个文件）
pub const CUR_NAME: &str = "framegen.log";
/// 上一次运行的日志文件名 —— 只留这一份「上一次」。
/// 为什么不只留一份：用户常常是先关掉程序、过一会儿才来反馈，只留一份的话
/// 出问题那趟的记录已经被这次启动覆盖了；留两份刚好既能查、又不堆积。
pub const PREV_NAME: &str = "framegen.prev.log";
/// 单文件超过 4 MB 就滚到 prev，避免一次长时间运行写出一个几百 MB 的日志。
pub const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// 结构化事件日志（一行一个 JSON）：命令 / 参数 / 耗时 / 结果 / 错误码。
pub const EVENTS_NAME: &str = "events.jsonl";
pub const EVENTS_PREV_NAME: &str = "events.prev.jsonl";

static FILE: OnceLock<Mutex<File>> = OnceLock::new();
static PATH: OnceLock<PathBuf> = OnceLock::new();

/// 日志目录：跟数据根目录走（FGM_DATA_DIR 优先，否则程序同级 —— 便携版就这么用），
/// 目录不可写时回退 %APPDATA%。
pub fn dir() -> Option<PathBuf> {
    if let Some(d) = util::data_root() {
        let p = d.join("logs");
        if std::fs::create_dir_all(&p).is_ok() && util::is_writable(&p) {
            return Some(p);
        }
    }
    let p = util::app_data_dir().ok()?.join("logs");
    std::fs::create_dir_all(&p).ok()?;
    Some(p)
}

/// 本次会话的日志文件。init() 之前是 None。
pub fn path() -> Option<&'static PathBuf> {
    PATH.get()
}

/// 开一个本次会话的日志文件，返回它的路径。
pub fn init() -> Option<PathBuf> {
    let d = dir()?;
    let p = rotate(&d);
    let f = OpenOptions::new().create(true).append(true).open(&p).ok()?;
    let _ = FILE.set(Mutex::new(f));
    let _ = PATH.set(p.clone());
    Some(p)
}

/// 滚动日志：**目录里永远只有两个文件**。
///
///   1. 上一次的上一次（framegen.prev.log）删掉
///   2. 上一次那份（framegen.log）改名成 framegen.prev.log
///   3. 带时间戳的旧式文件：把最新的那份留作「上一次」，其余全删 ——
///      升级上来时目录里十几个文件立刻变干净
///
/// 抽成独立函数是为了自测能直接验它（init 里的全局句柄只能设一次）。
/// 返回本次要写的文件路径。
pub fn rotate(d: &Path) -> PathBuf {
    let cur = d.join(CUR_NAME);
    let prev = d.join(PREV_NAME);
    let _ = std::fs::remove_file(&prev);
    if cur.is_file() && std::fs::rename(&cur, &prev).is_err() {
        // 改名失败（极少见：被别的程序占着）就删掉，保证本次是从干净的文件开始写
        let _ = std::fs::remove_file(&cur);
    }

    let mut legacy: Vec<PathBuf> = std::fs::read_dir(d)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("framegen-") && n.ends_with(".log"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    legacy.sort();
    if !prev.is_file() {
        // 刚从老版本升级上来：把最新那份带时间戳的留成「上一次」，别让用户白丢
        if let Some(newest) = legacy.pop() {
            let _ = std::fs::rename(&newest, &prev);
        }
    }
    for p in legacy {
        let _ = std::fs::remove_file(p);
    }
    cur
}

/// 写一行。没初始化过就什么都不做 —— 命令行自检和测试不该因此崩。
pub fn line(msg: &str) {
    let Some(m) = FILE.get() else { return };
    let Ok(mut f) = m.lock() else { return };
    // 单次运行太久时按大小滚一次；文件名仍是 cur/prev 两个，不堆文件。
    if let Some(p) = PATH.get() {
        let prev = p.with_file_name(PREV_NAME);
        if rotate_if_too_big(p, &prev, MAX_BYTES) {
            if let Ok(nf) = OpenOptions::new().create(true).append(true).open(p) {
                *f = nf;
            }
        }
    }
    // 日志是用户要发给别人的东西：写盘前把用户名换掉（见 util::redact）。
    let _ = writeln!(f, "[{}] {}", clock(), util::redact(msg));
    let _ = f.flush();
}

/// 结构化事件：一行一个 JSON。给「导出诊断包」和后续自动分析用。
///
/// 写失败静默（日志绝不能影响命令本身）；参数由调用方先做摘要，这里只补时间。
pub fn event(v: &serde_json::Value) {
    let Some(d) = dir() else { return };
    let p = d.join(EVENTS_NAME);
    let prev = d.join(EVENTS_PREV_NAME);
    let _ = rotate_if_too_big(&p, &prev, MAX_BYTES);
    let mut obj = match v {
        serde_json::Value::Object(o) => o.clone(),
        other => {
            let mut o = serde_json::Map::new();
            o.insert("event".to_owned(), other.clone());
            o
        }
    };
    obj.insert("ts".to_owned(), serde_json::json!(clock()));
    obj.insert(
        "epoch".to_owned(),
        serde_json::json!(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)),
    );
    let Ok(text) = serde_json::to_string(&serde_json::Value::Object(obj)) else {
        return;
    };
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{}", util::redact(&text));
        let _ = f.flush();
    }
}

/// 超过 max 就把 cur 改名成 prev（旧的 prev 删掉）；返回是否滚过。
/// 抽出来是为了能用临时目录测，不依赖 init 的全局句柄。
fn rotate_if_too_big(cur: &Path, prev: &Path, max: u64) -> bool {
    let too_big = std::fs::metadata(cur)
        .map(|m| m.len() > max)
        .unwrap_or(false);
    if !too_big {
        return false;
    }
    let _ = std::fs::remove_file(prev);
    if std::fs::rename(cur, prev).is_err() {
        let _ = std::fs::remove_file(cur);
    }
    true
}

/// 一段操作的开始标记
pub fn section(title: &str) {
    line("");
    line(&format!("========== {} ==========", title));
}

/// 行首的时间（只取时分秒）
fn clock() -> String {
    util::now_local_hms()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_rotation_moves_current_to_prev() {
        let d = std::env::temp_dir().join(format!(
            "fgm-log-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        let cur = d.join(CUR_NAME);
        let prev = d.join(PREV_NAME);
        std::fs::write(&cur, vec![b'x'; 64]).unwrap();
        assert!(rotate_if_too_big(&cur, &prev, 32));
        assert!(!cur.exists());
        assert_eq!(std::fs::metadata(&prev).unwrap().len(), 64);
        // 没超限不滚
        std::fs::write(&cur, b"short").unwrap();
        assert!(!rotate_if_too_big(&cur, &prev, 32));
        assert!(cur.exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
