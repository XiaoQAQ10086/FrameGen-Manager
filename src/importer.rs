//! 手动导入：把用户从网盘 / U 盘 / 别人那儿拿到的 zip 变成可部署的资产。
//!
//! 用户只要「选中 zip」，剩下的（递归找文件、逐个校验、放到位）都在这里做。
//!
//! 校验策略（按用户定的）：
//!   * **签名可信** —— 作者自签证书（比对证书指纹）或 NVIDIA 的公开签名（Windows 认可）
//!     -> 静默通过，不打扰用户；
//!   * **其它** —— 没签名、内容被改过、证书不认识 —— 不当场拒绝，交给界面弹窗，
//!     写明原因，让用户自己决定「跳过」还是「仍然导入」。
//!
//! ini 没有签名，只能做结构检查（能解析、有已知的段落、没有奇怪内容）—— 结构对就静默通过。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

use crate::update;
use crate::util;
use crate::verify;

/// 代理入口的文件名（和上游 alternatives/ 里的一致）
pub const PROXY_NAMES: [&str; 7] = [
    "version.dll",
    "winmm.dll",
    "dbghelp.dll",
    "dinput8.dll",
    "dxgi.dll",
    "d3d12.dll",
    "winhttp.dll",
];
/// 配置文件的名字
pub const INI_NAME: &str = "dlssg_sm86.ini";
/// 两个 DLSS 运行库
pub const RUNTIME_NAMES: [&str; 2] = ["nvngx_dlssg.dll", "nvngx_dlss.dll"];

/// 认出来的文件类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Proxy,
    Ini,
    Runtime,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Proxy => "代理入口",
            Kind::Ini => "配置文件",
            Kind::Runtime => "DLSS 运行库",
        }
    }
}

/// 一个校验过的待导入文件（已经解到临时目录）
#[derive(Debug, Clone)]
pub struct Staged {
    pub kind: Kind,
    /// 放进资产目录时用的文件名（标准名，小写）
    pub name: String,
    /// 临时文件（还没搬进资产目录）
    pub tmp: PathBuf,
    /// 来自哪个包
    pub from: String,
    pub bytes: u64,
    pub sha256: String,
    /// 校验说明（给用户看）
    pub note: String,
    /// true = 静默通过；false = 需要用户确认
    pub trusted: bool,
}

fn basename_lower(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim()
        .to_lowercase()
}

fn kind_of(base: &str) -> Option<Kind> {
    if PROXY_NAMES.contains(&base) {
        Some(Kind::Proxy)
    } else if base == INI_NAME {
        Some(Kind::Ini)
    } else if RUNTIME_NAMES.contains(&base) {
        Some(Kind::Runtime)
    } else {
        None
    }
}

/// 把一个条目收进结果里；同名的后来者覆盖先前的（用户按顺序选包，通常最后那个是他想用的）
fn upsert(out: &mut Vec<Staged>, st: Staged) {
    if let Some(old) = out.iter_mut().find(|o| o.name == st.name) {
        let _ = std::fs::remove_file(&old.tmp);
        *old = st;
    } else {
        out.push(st);
    }
}

/// ini 的结构检查。不检查具体键值（上游以后加键是正常的），只要求「像一份正常的 ini」。
pub fn ini_sanity(text: &str) -> std::result::Result<(), String> {
    if text.trim().is_empty() {
        return Err("空文件".to_owned());
    }
    if text.len() > 64 * 1024 {
        return Err(format!("太大（{} 字节）", text.len()));
    }
    if text.contains('\0') {
        return Err("里面有空字节，不像是文本配置".to_owned());
    }
    const KNOWN: [&str; 5] = [
        "general",
        "framegeneration",
        "compatibility",
        "logging",
        "runtime",
    ];
    let mut sections = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with(';') || t.starts_with('#') {
            continue;
        }
        if let Some(inner) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            if KNOWN.contains(&inner.trim().to_ascii_lowercase().as_str()) {
                sections += 1;
            }
            continue;
        }
        if !t.contains('=') {
            let shown: String = t.chars().take(40).collect();
            return Err(format!("有一行既不是段落也不是键值：{shown}"));
        }
    }
    if sections == 0 {
        return Err("没有找到任何已知的配置段落".to_owned());
    }
    Ok(())
}

fn classify(kind: Kind, name: String, tmp: PathBuf, from: String) -> Result<Staged> {
    let bytes = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    let sha256 = util::sha256_file(&tmp).unwrap_or_default();
    let (trusted, note) = match kind {
        Kind::Ini => {
            let text = std::fs::read_to_string(&tmp).unwrap_or_default();
            match ini_sanity(&text) {
                Ok(()) => (
                    true,
                    format!(
                        "配置文件 {} 字节，结构检查通过（没有签名，只能检查结构）",
                        bytes
                    ),
                ),
                Err(e) => (false, format!("配置文件看起来不对：{e}")),
            }
        }
        Kind::Proxy | Kind::Runtime => {
            let rep = verify::verify_file(&tmp);
            let ok = rep.content_trusted();
            (ok, format!("{}（{} 字节）", rep.summary(), bytes))
        }
    };
    Ok(Staged {
        kind,
        name,
        tmp,
        from,
        bytes,
        sha256,
        note,
        trusted,
    })
}

/// 递归列出目录里的所有文件
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// 从一批路径里找出所有认得的文件，解到 work_dir 并逐个校验。
///
/// paths 里可以混着 .zip 和文件夹（有人习惯先解压）。
pub fn stage(
    paths: &[PathBuf],
    work_dir: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(String),
) -> Result<Vec<Staged>> {
    std::fs::create_dir_all(work_dir)?;
    let mut out: Vec<Staged> = Vec::new();
    for p in paths {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("{}", update::CANCELLED_MSG);
        }
        if p.is_dir() {
            // 先自己解压过的人：直接扫文件夹
            let files = walk_files(p);
            progress(format!("扫描文件夹 {}（{} 个文件）...", p.display(), files.len()));
            for f in files {
                if cancel.load(Ordering::Relaxed) {
                    anyhow::bail!("{}", update::CANCELLED_MSG);
                }
                let base = f
                    .file_name()
                    .map(|s| s.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                let Some(kind) = kind_of(&base) else { continue };
                let tmp = work_dir.join(&base);
                std::fs::copy(&f, &tmp)
                    .with_context(|| format!("复制 {} 失败", f.display()))?;
                let st = classify(kind, base, tmp, p.display().to_string())?;
                upsert(&mut out, st);
            }
            continue;
        }
        if !p.is_file() {
            progress(format!("跳过（不存在）：{}", p.display()));
            continue;
        }
        let pack = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        progress(format!("打开 {pack} ..."));
        let list = match update::zip_list(p) {
            Ok(l) => l,
            Err(e) => {
                // 选错文件很正常（比如选了 rar），说清原因就好
                progress(format!("{pack} 不是有效的 zip，已跳过（{e}）"));
                continue;
            }
        };
        progress(format!("{pack} 里有 {} 个文件，正在找需要的 ...", list.len()));
        for meta in &list {
            if cancel.load(Ordering::Relaxed) {
                anyhow::bail!("{}", update::CANCELLED_MSG);
            }
            let base = basename_lower(&meta.name);
            let Some(kind) = kind_of(&base) else { continue };
            let tmp = work_dir.join(format!("{base}.part"));
            let n = update::zip_extract_to(p, meta, &tmp)?;
            if n == 0 {
                let _ = std::fs::remove_file(&tmp);
                continue;
            }
            match classify(kind, base, tmp.clone(), pack.clone()) {
                Ok(st) => upsert(&mut out, st),
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    progress(format!("{} 处理失败，已跳过：{e}", meta.name));
                }
            }
        }
    }
    Ok(out)
}

/// 把校验过的文件搬进资产目录，并记进状态（界面就会显示「已就绪」）。
/// 返回成功的文件名列表。
pub fn install(items: &[Staged]) -> Result<Vec<String>> {
    let dir = util::assets_dir()?;
    let mut state = update::load_state();
    let mut done = Vec::new();
    for it in items {
        let dest = dir.join(&it.name);
        // 临时名放同一个目录：atomic_replace 换的是同一卷上的文件
        let tmp = dir.join(format!("{}.importing", it.name));
        std::fs::copy(&it.tmp, &tmp).with_context(|| format!("写入 {}", tmp.display()))?;
        util::atomic_replace(&tmp, &dest)?;
        util::clear_motw(&dest);
        state.files.insert(
            it.name.clone(),
            update::LocalFile {
                blob_sha: String::new(),
                etag: String::new(),
                sha256: it.sha256.clone(),
                bytes: it.bytes,
                downloaded_at: util::now_utc(),
                imported: true,
            },
        );
        done.push(it.name.clone());
    }
    update::save_state(&state)?;
    Ok(done)
}

/// 清掉这次的临时文件（导完、或者用户放弃时都要清）
pub fn cleanup(items: &[Staged]) {
    for it in items {
        let _ = std::fs::remove_file(&it.tmp);
    }
}
