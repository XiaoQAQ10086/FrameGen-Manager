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

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::util;

/// 最多保留几个日志文件，超了删最旧的。
const KEEP_FILES: usize = 10;

static FILE: OnceLock<Mutex<File>> = OnceLock::new();
static PATH: OnceLock<PathBuf> = OnceLock::new();

/// 日志目录：优先程序同级（和 assets / backups 一样的便携式布局），
/// 程序目录不可写时回退 %APPDATA%。
pub fn dir() -> Option<PathBuf> {
    if let Some(d) = util::exe_dir() {
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
    prune(&d);
    let p = d.join(format!("framegen-{}.log", stamp()));
    let f = OpenOptions::new().create(true).append(true).open(&p).ok()?;
    let _ = FILE.set(Mutex::new(f));
    let _ = PATH.set(p.clone());
    Some(p)
}

/// 写一行。没初始化过就什么都不做 —— 命令行自检和测试不该因此崩。
pub fn line(msg: &str) {
    let Some(m) = FILE.get() else { return };
    let Ok(mut f) = m.lock() else { return };
    let _ = writeln!(f, "[{}] {}", clock(), msg);
    let _ = f.flush();
}

/// 一段操作的开始标记
pub fn section(title: &str) {
    line("");
    line(&format!("========== {} ==========", title));
}

/// 文件名用的时间戳（纯数字，按名字排序就是按时间排序）
fn stamp() -> String {
    util::now_utc()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// 行首的时间（只取时分秒）
fn clock() -> String {
    util::now_utc()
        .split(' ')
        .nth(1)
        .unwrap_or_default()
        .to_owned()
}

/// 只保留最近 KEEP_FILES 个日志文件。文件名带时间戳，按名字排序即按时间排序。
fn prune(d: &Path) {
    let Ok(rd) = std::fs::read_dir(d) else { return };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("framegen-") && n.ends_with(".log"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    if files.len() <= KEEP_FILES {
        return;
    }
    for p in &files[..files.len() - KEEP_FILES] {
        let _ = std::fs::remove_file(p);
    }
}
