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
//! 以前是「一次运行一个带时间戳的文件、保留 10 个」，用户反馈说点开一次多一个、看着乱，
//! 所以改成固定名字滚动覆盖：上一次的改名成 prev，更早的删掉。

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
///   3. 老版本（0.9.5 及以前）每次启动建一个带时间戳的文件：把最新的那份留作
///      「上一次」，其余全删 —— 用户一升级上来，目录里十几个文件立刻变干净
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
    let _ = writeln!(f, "[{}] {}", clock(), msg);
    let _ = f.flush();
}

/// 一段操作的开始标记
pub fn section(title: &str) {
    line("");
    line(&format!("========== {} ==========", title));
}

/// 行首的时间（只取时分秒）
fn clock() -> String {
    util::now_utc()
        .split(' ')
        .nth(1)
        .unwrap_or_default()
        .to_owned()
}

