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

/// 配置文件的名字（和仓库里那份同名 —— 名字只在 update::INI_REPO_PATH 定义一次）
pub const INI_NAME: &str = update::INI_REPO_PATH;
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

/// 压缩包里的是「哪一版」—— 只看路径就能看出来，上游的布局是固定的：
///   * archive/…        —— 老的归档包（0.1.0 / 0.2.4），归档用
///   * 310.1/…          —— 上游自己的老版本目录（0.3.1 起 20/30 系都用根目录那份了）
///   * 其它（根目录）    —— 当前最新版
///
/// 返回 0 = 最新版（优先），1 = 老版本目录，2 = 归档老版。
/// 它只用来在**同名文件有多个副本**时挑哪个，不拿来拒绝任何文件。
pub fn build_rank(path: &str) -> u8 {
    let p = path.replace('\\', "/").to_lowercase();
    if p.contains("archive/") {
        // 归档那套一定是最差的（那张证书是老的，别让它顶掉新文件）
        return 2;
    }
    if p.contains("310.1/") {
        1
    } else {
        0
    }
}

/// 给人看的版本名
pub fn build_label(path: &str) -> &'static str {
    let p = path.replace('\\', "/").to_lowercase();
    if p.contains("archive/") {
        "归档的老包"
    } else if p.contains("310.1/") {
        "上游的 310.1 老版本目录"
    } else {
        "当前最新版"
    }
}

fn basename_lower(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim()
        .to_lowercase()
}

fn kind_of(base: &str) -> Option<Kind> {
    // 代理入口名单只在 scan.rs 里硬编码（scan::PROXY_ALL），这里问它
    if crate::scan::is_known_proxy(base) {
        Some(Kind::Proxy)
    } else if base == INI_NAME {
        Some(Kind::Ini)
    } else if RUNTIME_NAMES.contains(&base) {
        Some(Kind::Runtime)
    } else {
        None
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

/// 从用户选的一个或多个 zip 里找出所有认得的文件，解到 work_dir 并逐个校验。
///
/// 只支持 zip：以前也支持直接选一个解压好的文件夹，那个入口已经删掉。
/// 上游源码 zip 里同时有根目录、310.1/ 和 archive/ 三套同名文件，
/// 按路径排优先级（见 build_rank）：根目录那份最新，优先取它。
pub fn stage(
    paths: &[PathBuf],
    work_dir: &Path,
    cancel: &AtomicBool,
    // 进度回调：(给用户看的一句话, 0.0~1.0 的完成度)。以前没有完成度，
    // 大压缩包导入时进度条一动不动，看着像卡死 —— 现在按「已看几个文件」推进。
    mut progress: impl FnMut(String, f32),
    // 第二个返回值是「说明」清单：哪些文件被跳过、为什么，界面会写进导入结果
) -> Result<(Vec<Staged>, Vec<String>)> {
    std::fs::create_dir_all(work_dir)?;
    // 每个解出来的候选带着「它属于哪一版」的排名，等同名的都收齐了再挑赢家
    let mut cands: Vec<(u8, String, Staged)> = Vec::new();
    // 临时文件名必须**全局唯一**：上游源码 zip 里根目录和 310.1/ 都叫 version.dll，
    // 早先按「文件名.part」解压，第二个会把第一个覆盖掉，然后合并时又把文件删了 ——
    // 结果就是用户点了「继续导入」之后报「导入失败」。所以这里带来源序号 + 条目序号。
    let mut uniq = 0usize;

    for (si, p) in paths.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("{}", update::CANCELLED_MSG);
        }
        if !p.is_file() {
            progress(format!("跳过（不是 zip 文件）：{}", p.display()), 0.0);
            continue;
        }
        let pack = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        progress(format!("打开 {pack} ..."), 0.0);
        let list = match update::zip_list(p) {
            Ok(l) => l,
            Err(e) => {
                // 选错文件很正常（比如选了 rar），说清原因就好
                progress(format!("{pack} 不是有效的 zip，已跳过（{e}）"), 0.0);
                continue;
            }
        };
        let n = list.len();
        progress(format!("{pack} 里有 {n} 个文件，正在找需要的 ..."), 0.0);
        for (i, meta) in list.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                anyhow::bail!("{}", update::CANCELLED_MSG);
            }
            let seen = i + 1;
            let frac = seen as f32 / n.max(1) as f32;
            let base = basename_lower(&meta.name);
            let Some(kind) = kind_of(&base) else {
                progress(format!("{pack}：已看 {seen}/{n}，正在找需要的 ..."), frac);
                continue;
            };
            progress(format!("{pack}：已看 {seen}/{n}，正在校验 {base} ..."), frac);
            let tmp = work_dir.join(format!("s{si}-{uniq}-{base}"));
            uniq += 1;
            let n = update::zip_extract_to(p, meta, &tmp)?;
            if n == 0 {
                let _ = std::fs::remove_file(&tmp);
                continue;
            }
            match classify(kind, base, tmp.clone(), pack.clone()) {
                Ok(st) => cands.push((build_rank(&meta.name), meta.name.clone(), st)),
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    progress(format!("{} 处理失败，已跳过：{e}", meta.name), frac);
                }
            }
        }
    }

    // 同名挑赢家：当前在用的那一版优先（0 最好）。输掉的那些直接删掉临时文件。
    let mut winners: Vec<(u8, String, Staged)> = Vec::new();
    for (rank, path, st) in cands {
        match winners.iter_mut().find(|(_, _, w)| w.name == st.name) {
            None => winners.push((rank, path, st)),
            Some(slot) => {
                if rank < slot.0 {
                    // 新的更优先：丢掉旧的
                    let _ = std::fs::remove_file(&slot.2.tmp);
                    *slot = (rank, path, st);
                } else {
                    let _ = std::fs::remove_file(&st.tmp);
                }
            }
        }
    }

    let mut out: Vec<Staged> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for (rank, path, mut st) in winners {
        // 上游当前版本已经不用这个入口了（winhttp 只有 archive/0.2.4/altnative 里才有）。
        // 它放进来纯属备用：两个版本的可选入口里都没有它，部署时永远不会用到，
        // 所以**不要**因为它来自老版就弹窗（用户会当成误判）。照收，只在清单里说明一句。
        let unused_entry =
            st.kind == Kind::Proxy && !crate::scan::PROXY_PRIORITY.contains(&st.name.as_str());
        if unused_entry {
            notes.push(format!(
                "{}：上游当前版本不使用这个入口（只有老版包里才有），仍然放进资产目录备用",
                st.name
            ));
        } else if rank > 0 && st.trusted {
            // 签名没问题，但这是「另一版」的文件：静默装下去会悄悄换掉资产里的版本，
            // 所以降级成「让用户确认一句」，并把原因说清楚
            st.trusted = false;
            st.note = format!(
                "{}。注意：这个文件来自{}，而不是最新版 —— 继续导入会用它覆盖资产里的同名文件",
                st.note,
                build_label(&path)
            );
        }
        out.push(st);
    }
    Ok((out, notes))
}

/// 把校验过的文件搬进资产目录，并记进状态（界面就会显示「已就绪」）。
/// 返回成功的文件名列表。
pub fn install(items: &[Staged]) -> Result<Vec<String>> {
    install_to(&util::assets_dir()?, items)
}

/// 把文件写进指定目录（自测用：不碰真实资产目录，也不写状态）。
pub fn install_to(dir: &Path, items: &[Staged]) -> Result<Vec<String>> {
    std::fs::create_dir_all(dir)?;
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
    if dir == util::assets_dir().unwrap_or_default() {
        update::save_state(&state)?;
    }
    Ok(done)
}

/// 清掉这次的临时文件（导完、或者用户放弃时都要清）
pub fn cleanup(items: &[Staged]) {
    for it in items {
        let _ = std::fs::remove_file(&it.tmp);
    }
}
