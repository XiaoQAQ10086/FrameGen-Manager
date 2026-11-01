//! 更新模块。
//!
//! 重要：上游 sdli1995/dlssg_for_sm86 **没有 GitHub Releases，也没有 Tags**。
//! 文件是直接提交在 main 分支根目录的（version.dll / dlssg_sm86.ini）。
//!
//! **正常流程一次 GitHub API 都不调。**
//!
//! 原因：api.github.com 未登录时按 IP 每小时只有 60 次配额，而不少用户走加速器 /
//! 代理，出口 IP 是共享的，配额会被别人吃光，于是「检查更新」「下载资产」直接失败。
//!
//! 替代方案：
//!   * 变更指纹：对 raw.githubusercontent.com 发 **HEAD**，响应里的 `ETag` 就是内容的
//!     SHA-256（64 位十六进制）。官方源和 ghproxy 镜像**都会**返回它。
//!   * 文件下载：raw.githubusercontent.com 官方源，或镜像前缀。
//!   * 运行库：直接拼 release 直链 github.com/{repo}/releases/download/{tag}/{asset}，
//!     不必先问 releases API 要资产列表。
//!
//! 只有 release 直链失败（作者删包 / 改名）时，才回退到 releases API。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::util;
use std::sync::atomic::{AtomicBool, Ordering};

pub const REPO: &str = "sdli1995/dlssg_for_sm86";
pub const BRANCH: &str = "main";
pub const INI_REPO_PATH: &str = "dlssg_sm86.ini";

const USER_AGENT: &str = "FrameGen-Manager/0.1 (+https://github.com/sdli1995/dlssg_for_sm86)";

pub fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        // 对下载来说这个值是「单次读取」的上限，不是整段下载的总上限：
        // reqwest 的阻塞读每调用一次就重新计时，所以只要服务器还在往外吐字节，
        // 多慢都能慢慢下完 —— 用户线路慢不该被掐断。
        // （按「平均速度低于 300 KB/s 就换源」会把慢线路掐断，只下到四分之一。）
        // 它现在只兜底两件事：源彻底不动了 —— 连续 20 秒一个字节都没有才判它死；
        // 以及「点取消之后最长还要等多久」：读卡住时取消要等这次读返回。
        // 60s 太长（用户以为程序死了）；健康的慢线路每读一次都重新计时，不受影响。
        .timeout(Duration::from_secs(20))
        // 连接超时别设太长：源被墙时每个候选都要空等这么久。
        // 能用的源 1 秒内就连上了，8 秒足够宽容。
        .connect_timeout(Duration::from_secs(8))
        .build()
        .context("创建 HTTP 客户端失败")
}

/// 取消下载时的固定文案。上层靠它区分「用户点了取消」和「真的下载失败」——
/// 否则取消会被一路当成失败，最后弹出一句「官方源下载失败」误导用户。
pub const CANCELLED_MSG: &str = "已取消下载";

fn cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false)
}

// 这里记的是每个候选的**测速值**（见下面的 SourceSpeeds），而不是「上次哪个成功了」。
// 区别很重要：镜像的快慢按用户线路和时间变，「上次能连上」不代表「这次够快」，
// 而慢源不换掉就是用户抱怨的那个问题。

/// 按候选顺序依次尝试，第一个成功的胜出。
///
/// **探测和下载的优先级是反的，这是故意的**：
/// * 探测（HEAD，拿 ETag 指纹）走**官方优先** —— 指纹要从 GitHub 自己那里拿才可信。
///   HEAD 很小，官方 raw 即使是慢速链路也能秒回。
/// * 下载走**镜像优先**，镜像之间再按**测速值**从快到慢排（见 rank_mirrors）。
///   内容仍然用官方拿到的指纹校验，所以既快又不牺牲可信度。
///
/// attempt 拿到的是 (完整 URL, 源前缀)；前缀空串表示官方源。
fn try_sources<T>(
    official: &str,
    mirrors: &[String],
    download: bool,
    cancel: Option<&AtomicBool>,
    mut attempt: impl FnMut(&str, &str) -> Result<T>,
) -> Result<T> {
    let mut urls: Vec<(String, String, &'static str)> = Vec::with_capacity(mirrors.len() + 1);
    if download {
        for m in mirrors {
            urls.push((format!("{m}{official}"), m.clone(), "备用源"));
        }
        urls.push((official.to_owned(), String::new(), "官方源"));
    } else {
        urls.push((official.to_owned(), String::new(), "官方源"));
        for m in mirrors {
            urls.push((format!("{m}{official}"), m.clone(), "备用源"));
        }
    }

    let mut errs: Vec<String> = Vec::new();
    for (url, prefix, label) in &urls {
        // 用户点了取消就立刻停，不要再去试下一个源 ——
        // 否则会一路试完所有镜像才报错，看起来像「所有源都坏了」。
        if cancelled(cancel) {
            bail!("{}", CANCELLED_MSG);
        }
        match attempt(url, prefix) {
            Ok(v) => return Ok(v),
            Err(e) => {
                if cancelled(cancel) {
                    bail!("{}", CANCELLED_MSG);
                }
                errs.push(format!("{label}：{e}"));
            }
        }
    }
    bail!("{}", errs.join("；"))
}

// ---------------------------------------------------------------- 软件自身更新

/// 我们自己的仓库。用来检查「FrameGen Manager 本身」有没有新版本 ——
/// 注意这和上游 Mod 的更新检查是两回事。
pub const SELF_REPO: &str = "XiaoQAQ10086/FrameGen-Manager";

/// 当前版本，编译时从 Cargo.toml 取。
pub const SELF_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 发版页面。有新版本时点按钮跳这里。
pub const RELEASES_URL: &str = "https://github.com/XiaoQAQ10086/FrameGen-Manager/releases";

/// 读我们自己仓库 main 分支上的 Cargo.toml，取 version 字段。
///
/// **为什么不查 Releases 接口**：
///   * api.github.com 未登录按 IP 限 60 次/小时 —— 正是这个项目一直在躲的东西；
///   * gh-proxy 这类镜像**只代理资源文件、拒绝代理网页**（直接回
///     "Web page content is not allowed"），所以 releases 页面和 releases.atom 都抓不到；
///   * 而 raw 上的 Cargo.toml 只有 2KB，官方源和镜像都拿得到，且不占配额。
///
/// 前提：发布流程是「改版本号 -> 提交 -> 打标签 -> 发 Release」一条龙，
/// 所以 main 上的版本号等于最新已发布版本。改流程的话这里要跟着改。
///
/// **为什么不只信第一个成功的源**：raw.githubusercontent.com 前面有 CDN 缓存，
/// 仓库里刚改完 Cargo.toml 的那几分钟，缓存还在吐旧内容。发 0.4.0 时官方 raw
/// 有约 3 分钟仍然说 0.3.0 —— 而官方恰好是优先源，一旦它「成功」返回就直接采信了，
/// 于是还停在 0.3.0 的用户被告知「已是最新」，根本看不到更新提示。各家的缓存时机
/// 不一样，所以这里改成**所有源都问、取最大的版本号**。
pub fn fetch_latest_self_version(client: &reqwest::blocking::Client) -> Option<String> {
    let official = format!("https://raw.githubusercontent.com/{SELF_REPO}/main/Cargo.toml");
    let mut sources = vec![(official.clone(), true)];
    for m in mirrors("") {
        sources.push((format!("{m}{official}"), false));
    }

    // 所有源**并发**问（总耗时约等于最慢那个，而不是相加）。
    let (tx, rx) = std::sync::mpsc::channel();
    for (url, is_official) in sources {
        let c = client.clone();
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((is_official, fetch_self_version_one(&c, &url)));
        });
    }
    // 把主线程手里这个发送端丢掉，rx.iter() 才会在那些线程都结束后收完
    drop(tx);

    let mut mirror_heard: Vec<String> = Vec::new();
    for (is_official, v) in rx.iter() {
        let Some(v) = v else { continue };
        if parse_version(&v).is_none() {
            continue;
        }
        if is_official {
            // **官方源说了算。** 镜像只是加速通道，不该有资格宣布版本号 ——
            // 以前这里是「所有源取最大的那个」，等于任何一个镜像都能让程序去下载
            // 并静默执行它指定的安装包。代价是官方 raw 缓存偶尔落后几分钟：
            // 那只是晚一会儿看到更新提示，比上面那条路好得多。
            return Some(v);
        }
        mirror_heard.push(v);
    }

    // 官方 raw 拿不到时（国内常见）才退到镜像，而且要求**至少两个镜像报同一个版本**：
    // 单个镜像被控或返回错内容时，不足以让程序去下载并执行它指定的东西。
    let mut best: Option<String> = None;
    for v in &mirror_heard {
        let agree = mirror_heard.iter().filter(|x| same_version(x, v)).count();
        if agree < 2 {
            continue;
        }
        if best.as_ref().map(|b| is_newer(v, b)).unwrap_or(true) {
            best = Some(v.clone());
        }
    }
    best
}

/// 这个版本的安装包**真的存在**吗（release 资产）。
///
/// 为什么需要：main 上的版本号是"发布一条龙"里的第一步，但它可能先于 release 存在。
/// 真实踩到过：版本号提到 1.0.6、release 还没建，于是所有旧版用户都被提示"发现新版本"，
/// 点下载却 404。界面上拒绝显示一个点不动的更新，比事后解释便宜得多。
///
/// 判据是对 **release 资产直链**发一个 HEAD：资产存在时 GitHub 会 302 到签名地址、
/// 跟随之后是 200；不存在时直接 404（官方源与 ghproxy 镜像实测都是 404）。
/// 这条路不碰 api.github.com，所以不吃那 60 次/小时的配额，镜像也能验。
pub fn self_release_exists(client: &reqwest::blocking::Client, version: &str) -> bool {
    let tag = if version.starts_with('v') {
        version.to_owned()
    } else {
        format!("v{version}")
    };
    let asset = format!("FrameGen-Manager-{tag}-setup.exe");
    let official = self_release_url(&tag, &asset);
    let mut urls = vec![official.clone()];
    // 只试第一个镜像：官方被墙时它常常是通的，全试一遍会把界面拖住
    if let Some(m) = mirrors("").into_iter().next() {
        urls.push(format!("{m}{official}"));
    }
    for url in urls {
        let Ok(resp) = client.head(&url).timeout(Duration::from_secs(8)).send() else {
            continue;
        };
        if resp.status().is_success() {
            return true;
        }
    }
    false
}

/// 给自测用的入口（见上的说明）。
pub fn same_version_pub(a: &str, b: &str) -> bool {
    same_version(a, b)
}

/// 两个版本号是不是同一个版本（解析不出来的都算不同）。
fn same_version(a: &str, b: &str) -> bool {
    match (parse_version(a), parse_version(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// 问一个源，拿它报的版本号。失败返回 None。
fn fetch_self_version_one(client: &reqwest::blocking::Client, url: &str) -> Option<String> {
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(8))
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    parse_cargo_version(&resp.text().ok()?)
}

/// 从 Cargo.toml 文本里取 version 字段。
///
/// 只要单独的 version 键。依赖那行的键名不是单独的 version
/// （形如 `eframe = { version = ... }`），所以按「键名等于 version」匹配够准。
pub fn parse_cargo_version(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        let Some((k, v)) = t.split_once('=') else {
            continue;
        };
        if k.trim() != "version" {
            continue;
        }
        let v = v.trim().trim_matches('"').trim().to_owned();
        if !v.is_empty() {
            return Some(v);
        }
    }
    None
}

/// 把 "0.2.0" / "v0.2.0" 拆成三段数字。解析不了返回 None（宁可不提示，也别误报）。
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let t = s.trim().trim_start_matches('v');
    let mut it = t.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next().unwrap_or("0").parse().ok()?;
    let c = it.next().unwrap_or("0").parse().ok()?;
    Some((a, b, c))
}

/// remote 是不是比 local 新
pub fn is_newer(remote: &str, local: &str) -> bool {
    match (parse_version(remote), parse_version(local)) {
        (Some(r), Some(l)) => r > l,
        _ => false,
    }
}

/// 取响应头里的 ETag，并确认它看起来就是内容的 SHA-256。
///
/// raw.githubusercontent.com（以及会透传这个头的 ghproxy 镜像）返回的 ETag 就是
/// 64 位十六进制的 SHA-256；不满足这个形状就当作没有，免得拿别的哈希去比对。
fn etag_of(resp: &reqwest::blocking::Response) -> Option<String> {
    let raw = resp.headers().get("etag")?.to_str().ok()?;
    let v = raw.trim().trim_matches('"').trim().to_ascii_lowercase();
    if v.len() == 64 && v.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(v)
    } else {
        None
    }
}

/// 从仓库文件里抠出上游版本号。
///
/// 0.2.4（native）两处都写成 "Native 0.2.4"：
///   README 首行  "# DLSSG Native 0.2.4"
///   INI 首行注释 "; Native 0.2.4. Restart the game after changing this file."
/// 0.3.0（代理）改成了 "DLSSG for SM86（Proxy）- 0.3.0 版本"，锚点没了，
/// 所以再兜一层：取首行里第一个「带小数点的数字」。
pub fn extract_version(text: &str) -> Option<String> {
    if let Some(idx) = text.find("Native ") {
        let rest = &text[idx + "Native ".len()..];
        let v: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let v = v.trim_end_matches('.').to_owned();
        if !v.is_empty() {
            return Some(v);
        }
    }
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    version_in_line(first)
}

/// 取一行里第一个「数字.数字」形状的版本号。
/// 手写而不是用正则：只为这一处不值得引入 regex 依赖。
fn version_in_line(line: &str) -> Option<String> {
    let cs: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        if !cs[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut dotted = false;
        while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
            if cs[i] == '.' {
                dotted = true;
            }
            i += 1;
        }
        if dotted {
            let v: String = cs[start..i].iter().collect();
            let v = v.trim_end_matches('.').to_owned();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// 官方下载地址
pub fn official_url(repo_path: &str) -> String {
    format!("https://raw.githubusercontent.com/{REPO}/{BRANCH}/{repo_path}")
}

/// 把 reqwest 的错误翻译成人能看懂的话
pub fn friendly_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "连接超时，网络太慢或连接被拦截".to_owned()
    } else if e.is_connect() {
        "连不上下载服务器（网络被墙、DNS 或代理问题）".to_owned()
    } else if e.is_status() {
        format!(
            "服务器返回错误状态 {}",
            e.status().map(|s| s.as_u16()).unwrap_or(0)
        )
    } else if e.is_decode() {
        "响应内容无法解码".to_owned()
    } else if e.is_request() || e.is_body() {
        "请求发送失败，通常是网络被拦截或 DNS 解析不了。可以改用备用源重试。".to_owned()
    } else {
        format!("网络请求失败（可以改用备用源重试）：{e}")
    }
}

/// 临时文件名：version.dll -> version.dll.part
fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".part");
    dest.with_file_name(name)
}

/// 清理资产目录里遗留的 .part 文件（进程被强杀时会留下）
pub fn clean_stale_partials() -> usize {
    let Ok(dir) = util::assets_dir() else {
        return 0;
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let cutoff =
        std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(7 * 24 * 3600));
    let mut n = 0;
    for e in rd.flatten() {
        let p = e.path();
        if !p.extension().map(|x| x == "part").unwrap_or(false) {
            continue;
        }
        // 断点续传要靠 .part 接着下，所以只清「很久没动过」的（7 天），
        // 别把刚下到一半的删掉 —— 跨会话继续下载就靠它。
        let stale = match (cutoff, std::fs::metadata(&p).and_then(|m| m.modified())) {
            (Some(c), Ok(t)) => t < c,
            _ => true,
        };
        if stale && std::fs::remove_file(&p).is_ok() {
            n += 1;
        }
    }
    n
}

/// 校验通过后，把下载好的 .part 落到正式位置。
///
/// download() 只负责写到 .part —— 内容校验（签名/大小）必须在替换之前完成，
/// 否则「所有源都校验失败」时坏内容已经把资产目录里那份好文件顶掉了。
pub fn commit_download(dest: &Path) -> Result<()> {
    util::atomic_replace(&part_path(dest), dest)?;
    util::clear_motw(dest);
    Ok(())
}

/// 一次下载的「写到哪、谁能叫停、进度报给谁」。
///
/// 捆成一个结构体的唯一原因是**别再让参数表无节制地长下去** ——
/// download / download_auto 本来要传 8~9 个参数，多一个少一个都容易传错位置。
pub struct Sink<'a> {
    /// 最终落盘位置（下载过程中写的是 .part，成功后原子替换）
    pub dest: &'a Path,
    /// 用户随时可以点取消
    pub cancel: &'a AtomicBool,
    /// 进度回调：(已下字节, 总字节[0 = 未知], 正在用哪个源)
    pub progress: &'a mut dyn FnMut(u64, u64, &str),
}

/// 这个文件允许从哪些源拿。和 Sink 一样，捆起来是为了别让参数表无节制地长。
#[derive(Clone, Copy)]
pub struct SourcePolicy<'a> {
    /// 用户指定的镜像前缀（空 = 自动挑）
    pub prefix: &'a str,
    /// 下载时先试镜像。DLL 走这条：镜像快几十倍，内容真实性由签名兜底。
    pub prefer_mirror: bool,
    /// **只允许官方源。** 没有签名可验的文件（dlssg_sm86.ini、SHA256SUMS.txt）必须走这条：
    /// 镜像可以不返回 ETag，那时指纹比对整段被跳过，等于把一份没人校验过的内容装进去。
    pub official_only: bool,
}

/// 自动选源下载一个仓库文件。这就是界面上「下载 / 更新资产」走的路径，
/// 不用用户再手点「改用备用源」。
///
/// **prefer_mirror 是有讲究的：**
/// * 大文件（15 MB 的代理 DLL）传 true —— 镜像快几十倍。代价是
///   gh-proxy.com **不转发 ETag**，那边 ETag 比对会落空，必须靠 `verify` 里的
///   签名校验兜住（这 5 个代理 DLL 都有本项目签名，镜像伪造不出来）。
/// * 小文件（581 B 的 ini）传 false —— 官方源再慢也是瞬间，而且官方**会**给
///   ETag，比对能真正生效。ini 没有签名，只能靠这个。
///
/// `verify` 失败时返回 Err 就会自动换下一个源重试。
///
/// 原来是「download_auto -> download_pass -> download」三层，中间那层只被这里
/// 用过一次，已经并进来；换源顺序仍然只看「这个源能不能把文件给全」，不看速度。
pub fn download_auto(
    client: &reqwest::blocking::Client,
    repo_path: &str,
    sink: Sink<'_>,
    expect_etag: Option<&str>,
    policy: SourcePolicy<'_>,
    verify: &dyn Fn(&Path) -> Result<()>,
) -> Result<Downloaded> {
    let official = official_url(repo_path);
    let ms = if policy.official_only {
        Vec::new()
    } else {
        mirrors(policy.prefix)
    };
    let prefer_mirror = policy.prefer_mirror;
    let (dest, cancel) = (sink.dest, sink.cancel);
    try_sources(&official, &ms, prefer_mirror, Some(cancel), |url, src| {
        let t0 = Instant::now();
        let r = download(
            client,
            repo_path,
            Sink {
                dest,
                cancel,
                progress: &mut *sink.progress,
            },
            url,
            expect_etag,
            src,
        );
        let secs = t0.elapsed().as_secs_f64();
        match r {
            Ok(dl) => {
                let tmp = part_path(dest);
                // 先校验临时文件，再替换目标：任何源失败都不会破坏已有的好文件
                if let Err(e) = verify(&tmp) {
                    crate::log::line(&format!(
                        "校验失败 {repo_path} <- {}：{e}",
                        source_label(src)
                    ));
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                crate::log::line(&format!(
                    "下载成功 {repo_path} <- {}  {} 字节  {secs:.1}s",
                    source_label(src),
                    dl.bytes
                ));
                let _ = &tmp;
                commit_download(dest)?;
                Ok(dl)
            }
            Err(e) => {
                crate::log::line(&format!(
                    "下载失败 {repo_path} <- {}  {secs:.1}s：{e}",
                    source_label(src)
                ));
                Err(e)
            }
        }
    })
}

/// 下载结果。带回去给调用方存档。
#[derive(Debug, Clone)]
pub struct Downloaded {
    pub bytes: u64,
    /// 本地算出来的内容 SHA-256（只是记录，不是校验依据）
    pub sha256: String,
    /// 内容的 git blob sha1（显示用，和 GitHub 的 blob sha 一致）
    pub blob_sha: String,
    /// 响应头里的 ETag，也就是 GitHub 给这份内容的内容指纹
    pub etag: Option<String>,
}

/// 下载并校验。**官方源、镜像、运行库 zip 都走这一条**（download_raw 已并进来）。
///
/// - url 由调用方决定（官方源或备用源）
/// - sink.cancel 置位时立刻中断，且磁盘上不留任何残留
///
/// 校验方式（不再依赖 GitHub API）：
///   1. expect_etag 有值时，把**响应里的 ETag** 和它比对；
///   2. 再比对 Content-Length，防止被截断（两种情况下都生效）。
///
/// expect_etag 传 None 表示「这一份没有可信的官方指纹可比」—— 运行库 zip 走的是
/// Release 直链，没有 GitHub 的 ETag 可对，解压后靠 NVIDIA 签名兜底。
///
/// 说明：raw.githubusercontent.com 的 ETag 是 GitHub 自己的内容哈希（不是 SHA-256，
/// 本地算不出来），所以这里比的是「两次请求说的是不是同一份内容」。
/// 官方源直连时 HTTPS 本身已经保证了内容真实性，这一步主要是防镜像返回错东西。
pub fn download(
    client: &reqwest::blocking::Client,
    repo_path: &str,
    sink: Sink<'_>,
    url: &str,
    expect_etag: Option<&str>,
    // 正在用的是哪个源（空串 = 官方源），跟着进度一起报给界面
    source: &str,
) -> Result<Downloaded> {
    let Sink {
        dest,
        cancel,
        progress,
    } = sink;
    let tmp = part_path(dest);
    // 断点续传：上次没下完的 .part 接着下（只有服务器回 206 才算数）。
    // 换源重试、同一个源再试一次，都能从这里续上，慢速网络不用从头再来。
    let mut resume = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    let mut req = client.get(url);
    if resume > 0 {
        req = req.header("Range", format!("bytes={resume}-"));
        // If-Range：远端内容已经变了就别续传，否则会拼出「旧头 + 新尾」的混合文件。
        // 服务端不按它回 206 时下面的分支会把 resume 归零、从头下。
        if let Some(exp) = expect_etag {
            req = req.header("If-Range", exp);
        }
    }
    let mut resp = req
        .send()
        .map_err(|e| anyhow::anyhow!(friendly_error(&e)))?;
    let status = resp.status();
    if status == reqwest::StatusCode::PARTIAL_CONTENT && resume > 0 {
        crate::log::line(&format!("断点续传 {repo_path}：从 {resume} 字节接着下"));
    } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE && resume > 0 {
        // 上次那条 .part 已经比远端内容还长（远端变小了）：它没用了，丢掉重下，
        // 否则每个源都会在这里失败、用户点多少次都下不动。
        let _ = std::fs::remove_file(&tmp);
        bail!("上次没下完的临时文件已过期，已丢弃：请再点一次下载");
    } else if status.is_success() {
        // 服务器不支持 Range（回 200），只能从头下
        resume = 0;
    } else {
        bail!("下载 {repo_path} 返回 HTTP {}", status.as_u16());
    }
    let resp_etag = etag_of(&resp);
    let total = resp
        .content_length()
        .filter(|n| *n > 0)
        .map(|n| n + resume)
        .unwrap_or(0);

    // **流式写盘**：以前是把整份内容收进 Vec（nvngx_dlss.dll 有 56 MB，
    // 增长期间还可能翻倍）。现在边下边写 .part，内存占用与文件大小无关；
    // 校验和在下完之后从文件算。
    if resume > 0 {
        // .part 大小和 Range 起点对不上（被别人动过）就重下
        let real = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        if real != resume {
            resume = 0;
        }
    }
    let mut out = if resume > 0 {
        std::fs::OpenOptions::new().append(true).open(&tmp)?
    } else {
        let _ = std::fs::remove_file(&tmp);
        std::fs::File::create(&tmp)?
    };
    let mut chunk = vec![0u8; 64 * 1024];
    let mut got: u64 = resume;
    // 只记总耗时，供下完后记录测速值用 —— 不再按速度拦任何东西。
    let t0 = Instant::now();
    // 进度回调里那第三段文字在这里算一次 —— 别每 64 KB 都新分配一个 String
    let tag = format!("经 {}", source_label(source));
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&tmp);
            bail!("{}", CANCELLED_MSG);
        }
        let n = resp
            .read(&mut chunk)
            .map_err(|e| anyhow::anyhow!("下载中断：{}", e))?;
        if n == 0 {
            break;
        }
        use std::io::Write;
        out.write_all(&chunk[..n])
            .map_err(|e| anyhow::anyhow!("写临时文件失败：{e}"))?;
        got += n as u64;
        progress(got, total, &tag);
    }
    use std::io::Write;
    out.flush()?;
    drop(out);

    // 指纹比对：下载响应说的必须是同一份内容
    if let (Some(exp), Some(got)) = (expect_etag, &resp_etag) {
        if !exp.eq_ignore_ascii_case(got) {
            let _ = std::fs::remove_file(&tmp);
            bail!(
                "完整性校验失败：{repo_path} 的内容指纹和仓库对不上，已丢弃（镜像可能返回了错误内容）"
            );
        }
    }
    // 长度比对：防截断
    if total > 0 && got != total {
        let _ = std::fs::remove_file(&tmp);
        bail!("下载不完整：{repo_path} 期望 {total} 字节，实际只收到 {got} 字节，已丢弃");
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 只写临时文件，**不**替换目标：内容校验交给调用方（download_auto / download_with_mirror），
    // 校验通过才 atomic_replace。否则「所有源都校验失败」时，坏内容已经把资产目录里
    // 那份好文件顶掉了 —— 镜像返回 HTML 错误页就是这么把可用资产弄坏的。
    record_download_speed(source, got, t0.elapsed().as_secs_f64());
    Ok(Downloaded {
        bytes: got,
        sha256: util::sha256_file(&tmp).unwrap_or_default(),
        blob_sha: util::git_blob_sha1_file(&tmp).unwrap_or_default(),
        etag: resp_etag,
    })
}

/// 下载成功后把测速值记下来，下次排序就有依据了。
/// 太小的样本不记 —— 581 字节的 ini 算出来的数没有意义。
fn record_download_speed(source: &str, bytes: u64, secs: f64) {
    if bytes < SPEED_RECORD_MIN_BYTES || secs < 0.3 {
        return;
    }
    record_speed(source, (bytes as f64 / secs / 1024.0) as u64);
}

pub fn asset_path(name: &str) -> Result<PathBuf> {
    Ok(util::assets_dir()?.join(name))
}

/// 内置资产源目录：安装目录（便携版就是 exe 同级）下的 bundled-assets。
///
/// 由 electron-builder 的 extraFiles 原样铺进来（便携版解压后也在同一层）。
/// 它是「资产目录」的**最后一份拷贝**：这里也没有，就真的没地方补了。
pub fn bundled_assets_dir() -> Option<PathBuf> {
    Some(util::exe_dir()?.join("bundled-assets"))
}

/// 一次「从安装目录补内置资产」的结果。
#[derive(Debug, Default)]
pub struct AssetRepair {
    /// 实际使用的资产目录
    pub dir: PathBuf,
    /// 安装目录（exe 同级；bundled-assets / bundled-sm / bundled-dlss5 都在它下面）
    pub bundle: Option<PathBuf>,
    /// 这次补回来的文件（相对资产目录的路径，反斜杠分隔）
    pub repaired: Vec<String>,
    /// 本来就存在、大小也对（没动）
    pub present: usize,
    /// 资产目录和安装目录**都没有**的核心文件 —— 多半被安全软件删了
    pub missing: Vec<String>,
    /// 复制 / 校验失败的文件与原因
    pub failed: Vec<String>,
}

/// 把安装目录里的三套内置资产（bundled-assets / bundled-sm / bundled-dlss5）中
/// 缺失（或大小不对）的文件补进资产目录。
///
/// **只补不覆盖**：大小一致的文件一律跳过，绝不碰用户自己换过的资产。
///
/// 为什么需要它：version.dll 这类代理 DLL 从 0.4.1 起没有数字签名，Windows Defender /
/// 360 / 火绒经常把它当注入工具删掉。安装目录和资产目录各有一份，两份都没了，界面就只剩
/// 一句「资产目录里还没有 version.dll」—— 用户完全不知道是被杀软拦了，只会反复卸载重装。
/// 这里先把能补的补回来；源里也没有的那些由 missing 如实报出来（界面据此提示加白名单）。
pub fn repair_assets_from_bundle() -> Result<AssetRepair> {
    let dir = util::assets_dir()?;
    repair_assets_at(util::exe_dir().as_deref(), &dir)
}

/// repair_assets_from_bundle 的实现。安装目录与资产目录显式传入，单测才能不碰真实数据目录。
fn repair_assets_at(app: Option<&Path>, dir: &Path) -> Result<AssetRepair> {
    std::fs::create_dir_all(dir).with_context(|| format!("创建资产目录失败: {}", dir.display()))?;
    let mut out = AssetRepair {
        dir: dir.to_path_buf(),
        bundle: app.map(Path::to_path_buf),
        ..Default::default()
    };
    let Some(app) = app else {
        out.missing = expected_asset_files();
        return Ok(out);
    };
    // 三套资产各自铺到不同子目录，和 main.js 的 ensureBundledAssets 完全一致
    let trees: [(&str, PathBuf, &str); 3] = [
        ("bundled-assets", dir.to_path_buf(), ""),
        ("bundled-sm", dir.join("bundled-sm"), "bundled-sm"),
        ("bundled-dlss5", dir.join("DLSS5"), "DLSS5"),
    ];
    for (src_rel, dst, prefix) in trees {
        let src = app.join(src_rel);
        if src.is_dir() {
            copy_missing_tree(&src, &dst, prefix, &mut out);
        }
    }
    for rel in expected_asset_files() {
        if dir.join(&rel).is_file() {
            continue;
        }
        if app.join("bundled-assets").join(&rel).is_file() {
            // 源里有、刚才却没补上：复制失败已经记进 failed，不重复报 missing
            continue;
        }
        out.missing.push(rel);
    }
    // Smooth Motion 组件也叫 version.dll，而且按显卡系列分了两份 —— 用户日志里
    // 正是 assets\bundled-sm\rtx30\version.dll 缺失（AST-004）。源里带了这套资产时才检查，
    // 免得以「便携版没带 SM 组件」的历史布局误报。
    if app.join("bundled-sm").is_dir() {
        for rel in [
            Path::new("bundled-sm").join("rtx30").join("version.dll"),
            Path::new("bundled-sm").join("rtx20").join("version.dll"),
        ] {
            if !dir.join(&rel).is_file() && !app.join(&rel).is_file() {
                out.missing.push(rel.to_string_lossy().to_string());
            }
        }
    }
    Ok(out)
}

/// 把 src 里缺失/大小不对的文件补到 dst；reported 前缀只影响结果里的展示路径。
fn copy_missing_tree(src: &Path, dst: &Path, prefix: &str, out: &mut AssetRepair) {
    for entry in walkdir::WalkDir::new(src)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(src) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('/', "\\");
        if rel.is_empty() {
            continue;
        }
        let shown = if prefix.is_empty() {
            rel.clone()
        } else {
            format!("{prefix}\\{rel}")
        };
        let want = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let target = dst.join(&rel);
        if std::fs::metadata(&target)
            .map(|m| m.len() == want)
            .unwrap_or(false)
        {
            out.present += 1;
            continue;
        }
        if let Some(parent) = target.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                out.failed.push(format!("{shown}（建目录失败：{e}）"));
                continue;
            }
        }
        match std::fs::copy(entry.path(), &target) {
            Ok(_) => {
                let got = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                if got == want {
                    out.repaired.push(shown);
                } else {
                    out.failed
                        .push(format!("{shown}（复制后大小 {got} ≠ {want}）"));
                }
            }
            Err(e) => out.failed.push(format!("{shown}（{e}）")),
        }
    }
}

/// 部署 / 界面真正依赖的核心资产：六个代理 + INI + 运行库/内核分发表。
fn expected_asset_files() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |rel: &str| {
        if !out.iter().any(|x: &String| x.eq_ignore_ascii_case(rel)) {
            out.push(rel.to_owned());
        }
    };
    for n in crate::scan::PROXY_PRIORITY {
        add(n);
    }
    add(INI_REPO_PATH);
    for (rel, _, _) in crate::deploy::ASSET_INVENTORY
        .iter()
        .chain(crate::deploy::SR_INVENTORY.iter())
    {
        add(rel);
    }
    out
}

/// 出厂默认的「优化等级」（上游 0.4.0 的出厂 INI：Optimized=2；0.3.x 时期是 1）
pub const DEFAULT_OPTIMIZED: u8 = 1;

/// 出厂默认的 DLSS-G 渲染预设。
///
/// **C = FG Lite**，是本项目的默认选择：每个生成帧的 GPU 时间约少 32-54%，代价是画质略低
/// （主要在 HUD 与遮挡边缘）。上游出厂值是 Auto（交给游戏/驱动决定），界面把它列成
/// 「跟随游戏/驱动」供人改回去。
///
/// 为什么默认值必须写在这里、而不是改内置 INI：资产目录里的 dlssg_sm86.ini 是**用户可改**
/// 的一份拷贝，老用户升级上来时它可能还是 Autou 那份 stock 文件 —— 默认值放在代码里，
/// 每次部署都按它写进游戏目录，结果才是确定的。
pub const DEFAULT_PRESET: &str = "A";

/// 预设的合法取值（空串按出厂默认处理）。A/B 是上游 0.4.0 里就有的正式档位。
pub const PRESETS: [&str; 3] = ["A", "B", "C"];
/// 出厂默认的「倍率上限」（3 = 最高 4X；5 = 最高 6X）
pub const DEFAULT_MAX_FRAMES: u8 = 3;

/// 把「键=值」这一行改掉，其它内容（包括注释和顺序）原样保留。
/// 找不到这个键就返回 None —— 调用方保持原样并说明一句，而不是报错。
/// 改 INI 里某一项的值。section = Some("[DLSSG]") 时只改那一段里的；None 时不限段（老行为）。
///
/// 为什么必须分段：上游 0.4.0 把 Optimized / Preset 从别的段挪进了 [DLSSG]，
/// 而 Preset 这种键名很容易在别的段里也有，不分段就会改错地方。
fn ini_set_key_in(text: &str, section: Option<&str>, key: &str, value: &str) -> Option<String> {
    let mut cur_section = String::new();
    let mut found = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            cur_section = t.to_owned();
        } else if !t.starts_with(';') && !t.starts_with('#') {
            let in_wanted = match section {
                Some(want) => cur_section == want,
                None => true,
            };
            if in_wanted {
                if let Some(rest) = t.strip_prefix(key) {
                    if rest.starts_with('=') {
                        out.push_str(&format!("{key}={value}"));
                        if line.ends_with('\n') {
                            out.push('\n');
                        }
                        found = true;
                        continue;
                    }
                }
            }
        }
        out.push_str(line);
    }
    if found {
        Some(out)
    } else {
        None
    }
}

/// 在指定段里插入一行 key=value（段里没有这个键时用）。
///
/// 为什么需要它：只做"改已有行"是不够的 —— 上游把键挪段、或某天精简掉一行时，
/// 我们会静默地什么都不写（只在说明里提一句），用户看到的就是"设了 Tier/预设却没生效"。
/// 段不存在时追加一个新段，保证设置一定落得下去。
fn ini_insert_key(text: &str, section: &str, key: &str, value: &str) -> String {
    let mut out = String::with_capacity(text.len() + key.len() + value.len() + 4);
    let mut cur_section = String::new();
    let mut inserted = false;
    let mut section_seen = false;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        let is_header = t.starts_with('[') && t.ends_with(']');
        if is_header {
            // 走出目标段还没插进去 -> 插在段尾
            if cur_section == section && !inserted {
                out.push_str(&format!("{key}={value}\n"));
                inserted = true;
            }
            cur_section = t.to_owned();
            if cur_section == section {
                section_seen = true;
            }
        }
        out.push_str(line);
    }
    if cur_section == section && !inserted {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{key}={value}\n"));
        inserted = true;
    }
    if !inserted && !section_seen {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("\n{section}\n{key}={value}\n"));
    }
    out
}

/// 准备要部署的 INI，返回（文件路径, 给人看的改动说明）。
///
/// 只有"预设 = Auto（跟随上游出厂）且两个档位都是出厂值"时才直接返回资产里那份原文件；
/// 其余情况都会写一份 dlssg_sm86.deploy.ini（资产里那份原文件始终不动）。
///
/// optimized：0 原厂不加速 / 1 加速且画面与官方逐位一致 / 2 再加有损图像内核（出厂默认）/ 3 全部有损
/// max_frames：3 = 最高 4X（出厂默认）/ 5 = 最高 6X（仅 310.9 版，且要游戏自带插件支持）
// 这几个参数就是「部署档位」的原始输入，拆成结构体会让调用方多绕一层；
// 界面和自测都是逐项传的，保持一眼能对照 INI 的键。
#[allow(clippy::too_many_arguments)]
pub fn prepare_deploy_ini(
    upstream: &Path,
    optimized: u8,
    max_frames: u8,
    preset: &str,
    sr: bool,
    engine: &str,
    xess_flow: &str,
    xess_hudless: &str,
) -> Result<(PathBuf, Vec<String>)> {
    if !upstream.is_file() {
        bail!("缺少 {INI_REPO_PATH} —— 资产随安装包内置，请检查安装是否完整（或重新安装本程序）");
    }
    /* 空串 = 没选过（老配置），按出厂默认。
    **注意不能"是默认值就跳过写文件"**：资产里那份 dlssg_sm86.ini 是上游 stock，
    它的 Preset 是 Auto。默认 C 要想真的生效，就必须把 Preset=C 写进游戏目录那份 ——
    跳过写文件等于把 stock 的 Auto 部署出去，默认值形同虚设。
    所以只有"预设选 Auto（跟随上游）+ 两个档位也是出厂值"时，才原样用 stock 文件；
    其余情况一律生成 deploy.ini。 */
    let preset = if preset.is_empty() {
        DEFAULT_PRESET
    } else {
        preset
    };
    let engine = if engine.is_empty() { "DLSS" } else { engine };
    let flow = if xess_flow.is_empty() {
        "Middle"
    } else {
        xess_flow
    };
    // 0.4.1：Engine=XeSS 时最多 3（=4X），选 5（6X）会导致帧生成不启用（max_generated_frames_unsupported）
    let mut max_frames = max_frames;
    let mut xess_clamped = false;
    if engine == "XeSS" && max_frames > 3 {
        max_frames = 3;
        xess_clamped = true;
    }
    if preset == "A"
        && optimized == DEFAULT_OPTIMIZED
        && max_frames == DEFAULT_MAX_FRAMES
        && !sr
        && engine == "DLSS"
    {
        return Ok((upstream.to_path_buf(), Vec::new()));
    }
    let mut out = std::fs::read_to_string(upstream).context("读取 INI 失败")?;
    let mut notes: Vec<String> = Vec::new();
    if xess_clamped {
        notes.push("XeSS 引擎最多 4X（MaxGeneratedFrames 1-3），已自动降到 4X".to_owned());
    }
    /* 段名跟着上游 0.4.0：Optimized 与 Preset 在 [DLSSG]，MaxGeneratedFrames 在
    [FrameGeneration]。找不到段时退回"不限段"查找，这样老版 INI 也还能改。 */
    let preset_value = preset.to_owned();
    /* Auto 就是上游 stock 那份 INI 里的值（[DLSSG] Preset=Auto），不是"我们的改动"。
    用户选「跟随游戏/驱动」时不该往文件里写一行内容相同的 Preset=Auto ——
    那样文件白改一次、说明里也多一条假改动。 */
    let mut jobs: Vec<(Option<&str>, &str, String, &str)> = vec![
        (
            Some("[DLSSG]"),
            "Optimized",
            optimized.to_string(),
            "优化等级",
        ),
        (
            None,
            "MaxGeneratedFrames",
            max_frames.to_string(),
            "倍率上限",
        ),
    ];
    if preset != DEFAULT_PRESET {
        jobs.push((Some("[DLSSG]"), "Preset", preset_value, "DLSS-G 预设"));
    }
    /* 三个进阶开关：只在**与出厂值不同**时才写。deploy.ini 每次都从 stock 重新生成，
    所以用户改回默认时这些键自然消失，不需要"清除"逻辑。
    - [SuperResolution] Mode：超分加速（只对游戏用 DLSS Performance/Ultra 档时生效）
    - [FrameGeneration] Engine：第二引擎 XeSS（本项目自己的运行器，只 2X）
    - [XeSSFG] FlowResolution / UseHudless：只对 XeSS 引擎有意义 */
    /* 0.4.1 起 [SuperResolution] Mode 是**显式预设**（Off/M-INT8/L-INT8/M-FP16/L-FP16），
    0.4.0 的 Mode=On 已删除。这里按配置写入；空值/非法值回落到 Off。
    老 ini 里遗留的 Mode=On 也会被这次改写顺手迁移。 */
    let sr_mode = {
        let cfg = crate::util::load_config();
        let v = cfg.fg_sr_mode.trim().to_owned();
        const OK: [&str; 5] = ["Off", "M-INT8", "L-INT8", "M-FP16", "L-FP16"];
        if OK.iter().any(|x| x.eq_ignore_ascii_case(&v)) {
            let mut out = String::new();
            for x in OK {
                if x.eq_ignore_ascii_case(&v) {
                    out = x.to_owned();
                }
            }
            out
        } else if sr {
            // 0.4.0 的 Mode=On 语义 ≈ 用本项目 INT8 内核、预设 L/M → 取 L-INT8
            "L-INT8".to_owned()
        } else {
            "Off".to_owned()
        }
    };
    // 只在**真的要开超分**时才写：Opt Off 且原 ini 已是 Off 时不产生改动（也不多加一条说明）
    if !sr_mode.eq_ignore_ascii_case("Off") {
        jobs.push((Some("[SuperResolution]"), "Mode", sr_mode, "超分预设"));
    }
    if engine == "XeSS" {
        jobs.push((None, "Engine", "XeSS".to_owned(), "帧生成引擎"));
        if flow != "Middle" {
            jobs.push((
                Some("[XeSSFG]"),
                "FlowResolution",
                flow.to_owned(),
                "光流分辨率",
            ));
        }
        // 三态：Auto（不写，出厂）/ 1（强制）/ 0（不用）
        if !xess_hudless.eq_ignore_ascii_case("Auto") {
            jobs.push((
                Some("[XeSSFG]"),
                "UseHudless",
                xess_hudless.to_owned(),
                "无 HUD 合成",
            ));
        }
    }
    for (section, key, value, what) in jobs {
        let section_name = section.unwrap_or("");
        let hit = ini_set_key_in(&out, section, key, &value)
            .or_else(|| ini_set_key_in(&out, None, key, &value));
        match hit {
            Some(new) => {
                notes.push(format!("{what} {key}={value}"));
                out = new;
            }
            // 缺这一行就补上（上游改键名/挪段时最容易踩的坑）
            None => {
                out = if section_name.is_empty() {
                    format!(
                        "{out}{key}={value}
"
                    )
                } else {
                    ini_insert_key(&out, section_name, key, &value)
                };
                notes.push(format!(
                    "{what} {key}={value}（这份 INI 里原本没有，已补上）"
                ));
            }
        }
    }
    let dest = upstream.with_file_name("dlssg_sm86.deploy.ini");
    std::fs::write(&dest, &out).context("写入部署用 INI 失败")?;
    Ok((dest, notes))
}

// ---------------------------------------------------------------- 第三方 DLSS 运行库
//
// 为什么需要：很多游戏目录里没有 nvngx_dlssg.dll / nvngx_dlss.dll，这个 Mod 就不生效。
// 这两个文件由 NVIDIA 官方签名，这里从社区仓库的 release 里取（该仓库只做搬运打包，
// 我们解压后会校验签名者必须是 NVIDIA，否则丢弃）。

/// 内置镜像，按**测速值**从快到慢排。
///
/// 参考速率（拉 raw 上的 version.dll，每次 512 KB 样本，多轮）：
///   ghfile.geekertao.top   约 0.4 MB/s
///   ghfast.top             约 0.2 MB/s
///   gh-proxy.cn            约 0.07 MB/s
///   gh.xxooo.cf            约 0.06 MB/s
///   ghproxy.net            0.02 ~ 0.16 MB/s
///   raw 官方直连           0.00 ~ 0.06 MB/s   <- 基本不通
/// gh-proxy.com 在部分线路上最快（3.9 ~ 5.2 MB/s），但会对另一些线路返回 403，
/// 所以留着 —— download_auto 会挨个换源，不通就跳过。
///
/// **只留连得上的源。** ghproxy.cfd / ghps.cc / ghproxy.cdn.9i0i.com / ghp.icu /
/// gh-proxy.top / ghproxy.homeboyc.cn / gh.jasonzeng.dev / mirror.ghproxy.com
/// 全部连不上，已删掉：留着的死源只会在官方源也失败时挨个白等一次连接超时，
/// 纯粹拖慢用户。
/// 界面上的下载源只能从这里挑（「自动」或指定其中一个），不再支持手填地址。
pub const MIRRORS: [&str; 6] = [
    // 连得上的
    "https://gh-proxy.com/",
    "https://ghfast.top/",
    "https://ghfile.geekertao.top/",
    "https://ghproxy.net/",
    // 候选里同样连得上的两个
    "https://gh.xxooo.cf/",
    "https://gh-proxy.cn/",
];

/// 默认备用源（= 最快的那个镜像）。界面上「当前备用源」显示的就是它。
pub const DEFAULT_BACKUP_PREFIX: &str = "https://gh-proxy.com/";

/// 界面选中的下载源前缀 —— 做个归一化：去空格、补上结尾的斜杠（前缀是拼在官方
/// 地址前面的）。不像网址就返回 None，等于「自动」。
///
/// 界面上的下载源**只能从内置镜像里挑**，手填地址那个功能已经删掉。
pub fn normalize_source(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.len() < 8 || !s.contains("://") {
        return None;
    }
    Some(s.trim_end_matches('/').to_owned() + "/")
}

fn mirrors(custom: &str) -> Vec<String> {
    let selected = normalize_source(custom);
    let rest: Vec<String> = MIRRORS
        .iter()
        .filter(|m| selected.as_deref() != Some(*m))
        .map(|s| (*s).to_owned())
        .collect();
    let mut out = rank_mirrors(&rest);
    // 选中的源排最前面，其余镜像按测速值跟在后面兜底
    if let Some(sel) = selected {
        out.insert(0, sel);
    }
    out
}

// ---------------------------------------------------------- 选源：测速值记忆
//
// 为什么要这套东西：镜像的快慢**按用户线路和时间剧烈变化**。同一个
// gh-proxy.com，同一台机器，相隔一小时能从 6.9 MB/s 掉到 0.34 MB/s。
// 只记「上次哪个源成功了」根本察觉不到这种变化，用户就得陪着慢源一起等。

/// 一个源的测速记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeedSample {
    /// KB/s
    pub kbps: u64,
    /// unix 秒
    pub at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceSpeeds {
    pub entries: BTreeMap<String, SpeedSample>,
}

/// 排序用的分界线：测速值达到这个数（KB/s）的源算「快」，排在没测过的前面。
/// 注意它**只影响先试哪个源**，不会因为慢就中断下载 —— 慢源也让它下完。
pub const GOOD_SPEED_KBPS: u64 = 300;

/// 超过这段时间没再测过的记录就不算数 —— 镜像速率是按小时变的。
const SPEED_TTL_SECS: i64 = 6 * 3600;

fn speed_path() -> Result<PathBuf> {
    Ok(util::app_data_dir()?.join("source_speed.json"))
}

pub fn load_speeds() -> SourceSpeeds {
    speed_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_speeds(s: &SourceSpeeds) {
    if let Ok(p) = speed_path() {
        let _ = crate::util::write_json_atomic(&p, s);
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 记下一次测速值。prefix 空串表示官方源。
pub fn record_speed(prefix: &str, kbps: u64) {
    let mut s = load_speeds();
    s.entries.insert(
        prefix.to_owned(),
        SpeedSample {
            kbps,
            at: now_unix(),
        },
    );
    save_speeds(&s);
}

/// 取某个源的测速值。没测过、或记录太旧，都返回 None。
fn speed_of(s: &SourceSpeeds, prefix: &str) -> Option<u64> {
    let e = s.entries.get(prefix)?;
    if now_unix() - e.at > SPEED_TTL_SECS {
        return None;
    }
    Some(e.kbps)
}

/// 按测速值排序：确认够快的在前（越快越前），没测过的居中，确认太慢的垫底。
pub fn rank_mirrors(ms: &[String]) -> Vec<String> {
    let s = load_speeds();
    let items: Vec<(String, Option<u64>)> =
        ms.iter().map(|m| (m.clone(), speed_of(&s, m))).collect();
    rank_by_scores(&items)
}

/// 纯粹按 (源, 测速值) 排序，和磁盘状态无关，方便自测。
/// None = 没测过。
pub fn rank_by_scores(items: &[(String, Option<u64>)]) -> Vec<String> {
    let (mut good, mut unknown, mut slow) = (Vec::new(), Vec::new(), Vec::new());
    for (m, score) in items {
        match score {
            Some(k) if *k >= GOOD_SPEED_KBPS => good.push((*k, m.clone())),
            Some(k) => slow.push((*k, m.clone())),
            None => unknown.push(m.clone()),
        }
    }
    good.sort_by_key(|a| std::cmp::Reverse(a.0));
    slow.sort_by_key(|a| std::cmp::Reverse(a.0));
    let mut out: Vec<String> = good.into_iter().map(|(_, m)| m).collect();
    out.extend(unknown);
    out.extend(slow.into_iter().map(|(_, m)| m));
    out
}

/// 把源前缀变成给人看的名字。空串 = 官方源。
pub fn source_label(prefix: &str) -> String {
    let p = prefix.trim();
    if p.is_empty() {
        return "官方源".to_owned();
    }
    p.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_owned()
}

/// 下载中至少攒够这么多字节才值得记速率（小文件测出来的数没意义）。
const SPEED_RECORD_MIN_BYTES: u64 = 512 * 1024;

/// 运行库来源仓库
pub const DLSS_REPO: &str = "RankFTW/rhi-repo";

/// (release tag 前缀, 期望 tag, 压缩包文件名, 解出来的文件名, 界面显示名)
///
/// 压缩包名是从仓库 releases 里查出来写死的。有了它就能直接拼直链下载，
/// 不必先调 releases API 拿资产列表 —— 这一步正是配额用完后卡住下载的地方。
pub const DLSS_RUNTIME: [(&str, &str, &str, &str, &str); 2] = [
    (
        "dlssg-",
        "dlssg-310.9.1",
        "nvngx_dlssg_310.9.1.zip",
        "nvngx_dlssg.dll",
        "DLSS 帧生成运行库",
    ),
    (
        "dlss-",
        "dlss-310.9.1",
        "nvngx_dlss_310.9.1.zip",
        "nvngx_dlss.dll",
        "DLSS 超分运行库",
    ),
];

/// 部署时怎么处理两个 DLSS 运行库。
///
/// 上游（0.3.0 起）的说明里只要求「代理 DLL + INI」（运行库、模型、后端都内嵌在代理里），
/// 而**很多游戏目录本来就带着自己的** nvngx_dlssg.dll / nvngx_dlss.dll（和游戏自己的
/// DLSS / Streamline 版本配套）。无条件覆盖它们会出事：有用户遇到「工具部署后帧生成
/// 不生效，手动只放代理 + INI 却正常」。所以规则是：**已有的不动，缺的才补**。
///
/// 返回 (游戏目录里已有的名字, 缺的、需要补的名字)。
pub fn runtime_deploy_plan(target_dir: &Path) -> (Vec<&'static str>, Vec<&'static str>) {
    let mut have = Vec::new();
    let mut need = Vec::new();
    for (_prefix, _tag, _zip, dll_name, _label) in DLSS_RUNTIME {
        if target_dir.join(dll_name).is_file() {
            have.push(dll_name);
        } else {
            need.push(dll_name);
        }
    }
    (have, need)
}

#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    pub tag: String,
    pub asset_name: String,
    pub url: String,
    pub size: u64,
}

/// 按「镜像优先、官方兜底」的顺序下载一个不需要指纹校验的文件（运行库 zip）。
/// 所有源都失败时把错误一起报出来，方便看出到底卡在哪一环。
fn download_with_mirror(
    client: &reqwest::blocking::Client,
    official: &str,
    dest: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64, &str),
    // 只允许官方源。**没有签名可验的文件（SHA256SUMS.txt）必须走这条**：
    // 哈希清单和安装包如果来自同一个镜像，那份校验就等于自己给自己发证。
    official_only: bool,
    // 期望的字节数（发布 API 报的大小）；给 Some 时对不上就丢弃并换源。
    // 这是没有官方指纹可比时唯一能挡住「换个内容塞进来」的廉价信号。
    expect_bytes: Option<u64>,
) -> Result<u64> {
    let ms = if official_only {
        Vec::new()
    } else {
        mirrors("")
    };
    // 报错/日志里用来称呼这个文件（比如 nvngx_dlssg_310.9.1.zip）
    let label = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("运行库");
    // 依次试每个镜像，最后兜底官方源。只看能不能下完，不看速度 ——
    // 慢源就让它慢慢下，只有用户点取消或者源彻底不动才算数。
    try_sources(official, &ms, true, Some(cancel), |url, src| {
        progress(0, 0, src);
        let t0 = Instant::now();
        // 走的是同一条下载路径，只是没有可信的官方指纹可比（expect_etag = None）；
        // 内容真实性靠解压后的 NVIDIA 签名兜底，长度校验照样生效。
        match download(
            client,
            label,
            Sink {
                dest,
                cancel,
                progress: &mut *progress,
            },
            url,
            None,
            src,
        ) {
            Ok(dl) => {
                let tmp = part_path(dest);
                if let Some(want) = expect_bytes {
                    let got = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
                    if got != want {
                        let _ = std::fs::remove_file(&tmp);
                        crate::log::line(&format!(
                            "大小不符 {} <- {}：期望 {want} 字节，实际 {got} 字节，已丢弃",
                            dest.display(),
                            source_label(src)
                        ));
                        return Err(anyhow::anyhow!("字节数不符（期望 {want}，实际 {got}）"));
                    }
                }
                commit_download(dest)?;
                crate::log::line(&format!(
                    "下载成功 {} <- {}  {} 字节  {:.1}s",
                    dest.display(),
                    source_label(src),
                    dl.bytes,
                    t0.elapsed().as_secs_f64()
                ));
                Ok(dl.bytes)
            }
            Err(e) => {
                crate::log::line(&format!(
                    "下载失败 {} <- {}  {:.1}s：{e}",
                    dest.display(),
                    source_label(src),
                    t0.elapsed().as_secs_f64()
                ));
                Err(e)
            }
        }
    })
}

// ------------------------------------------------------------------ 软件自身更新
// 只做「便携版自替换」：下载新版 zip -> 比 SHA256 -> 解出 exe 暂存成 <当前 exe>.new
// -> 把正在运行的 exe 改名成 .old（这就是备份）-> 写入新版 -> 重启自己。
//
// **为什么一定要比 SHA256**：我们的 exe 没有代码签名，下载还走第三方镜像，
// 不比哈希就等于把「执行任意程序」的机会交给中间人。哈希取自发布时一起上传的
// SHA256SUMS.txt（同一次发布、同一个源）。

/// 我们仓库某个版本的发布资产直链。
pub fn self_release_url(tag: &str, asset: &str) -> String {
    format!("https://github.com/{SELF_REPO}/releases/download/{tag}/{asset}")
}

/// 从 SHA256SUMS.txt 里取某个文件的哈希（形如「哈希 文件名」，忽略注释和空行）。
pub fn parse_sha256sums(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let mut it = t.split_whitespace();
        let (Some(hash), Some(file)) = (it.next(), it.next()) else {
            continue;
        };
        if hash.len() == 64 && file.trim_start_matches('*') == name {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// 官方源的 SHA256SUMS.txt（首选路径）。
fn fetch_sums_official(
    client: &reqwest::blocking::Client,
    tag: &str,
    setup: &str,
    sums_path: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64, &str),
) -> Result<String> {
    download_with_mirror(
        client,
        &self_release_url(tag, "SHA256SUMS.txt"),
        sums_path,
        cancel,
        progress,
        true,
        None,
    )
    .context("下载 SHA256SUMS.txt 失败（校验和必须来自官方源，不能走镜像；网络不通时请到发布页手动下载）")?;
    let sums = std::fs::read_to_string(sums_path).context("读 SHA256SUMS.txt 失败")?;
    parse_sha256sums(&sums, setup).with_context(|| {
        format!(
            "这次发布的 SHA256SUMS.txt 里没有 {setup}（老版本没带这个文件），只能手动下载安装包"
        )
    })
}

/// 官方源不通时的退路：**多个镜像各取一份，两个以上镜像报同一个哈希才采信。**
///
/// 为什么是「两台一致」而不是「第一台成功就用」：单个镜像被控时，它可以同时换掉
/// 清单里的哈希和安装包，那样校验等于没做 —— 而下一步是直接运行安装包。
/// 两台互相独立的镜像同时被换、还换成一模一样的哈希，代价远高于收益。
/// 官方源一旦能通，永远优先官方（见 fetch_sums_official）。
fn fetch_sums_consensus(
    client: &reqwest::blocking::Client,
    tag: &str,
    setup: &str,
    sums_path: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64, &str),
) -> Result<String> {
    let official = self_release_url(tag, "SHA256SUMS.txt");
    let mut heard: Vec<(String, String)> = Vec::new();
    for (i, m) in mirrors("").into_iter().enumerate() {
        let url = format!("{m}{official}");
        let tmp = sums_path.with_file_name(format!("SHA256SUMS.mirror{i}.txt"));
        progress(0, 0, "SHA256SUMS.txt（镜像）");
        let got = download(
            client,
            "SHA256SUMS.txt",
            Sink {
                dest: &tmp,
                cancel,
                progress: &mut *progress,
            },
            &url,
            None,
            "镜像",
        );
        if got.is_ok() {
            if let Ok(t) = std::fs::read_to_string(&tmp) {
                if let Some(h) = parse_sha256sums(&t, setup) {
                    crate::log::line(&format!("自更新：镜像 {m} 给出的校验和 {h}"));
                    heard.push((m, h));
                }
            }
        }
    }
    for (m, h) in &heard {
        let agree = heard
            .iter()
            .filter(|(_, x)| x.eq_ignore_ascii_case(h))
            .count();
        if agree >= 2 {
            crate::log::line(&format!(
                "自更新：采用镜像一致的校验和（{m} 等 {agree} 个源）"
            ));
            return Ok(h.clone());
        }
    }
    bail!(
        "校验和拿不到：官方源不通，{} 个镜像里也没有两个给出一致的结果。请到发布页手动下载安装包（发布页里也写了官方 SHA256，可以自己核对）。",
        heard.len()
    )
}

/// 自更新的暂存结果。
pub struct StagedUpdate {
    /// 已经下载并校验好的安装程序（在临时目录里）
    pub installer: PathBuf,
    /// 安装程序的 SHA256（校验通过的那一个）
    pub sha256: String,
    /// 安装程序的字节数
    pub bytes: u64,
}

/// 下载新版本并暂存成 <当前 exe>.new，**不动**当前程序（换文件由 swap_in_place 做）。
pub fn stage_self_update(
    client: &reqwest::blocking::Client,
    version: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64, &str),
) -> Result<StagedUpdate> {
    let cur = std::env::current_exe().context("取不到当前程序路径")?;
    let dir = cur
        .parent()
        .map(Path::to_path_buf)
        .context("当前程序没有所在目录")?;
    if !crate::util::is_writable(&dir) {
        bail!(
            "程序所在目录不可写（{}），没法自动更新，请手动下载安装包",
            dir.display()
        );
    }
    let tag = format!("v{version}");
    let setup = format!("FrameGen-Manager-{tag}-setup.exe");
    let work = std::env::temp_dir().join("fgm-selfupdate");
    std::fs::create_dir_all(&work).context("建临时目录失败")?;
    let sums_path = work.join("SHA256SUMS.txt");
    let dest = work.join(&setup);

    // 1) 发布时给出的 SHA256 清单。**优先官方源**；官方源不通时退到「多镜像一致」。
    //    以前这里和安装包一样走镜像：于是「哈希」和「被哈希的文件」来自同一台
    //    第三方镜像，那份校验等于自己给自己发证 —— 镜像换掉两个文件就能通过，
    //    而下一步是静默执行安装包。清单只有几百字节，官方源不慢，所以首选官方。
    //    但**只走官方**会把连不上 github.com 的用户彻底挡在更新之外（实测：他们的
    //    版本号靠镜像投票拿得到、于是看得到更新提示，却必然卡在这一步），所以要有退路。
    progress(0, 0, "SHA256SUMS.txt");
    let want = match fetch_sums_official(client, &tag, &setup, &sums_path, cancel, progress) {
        Ok(w) => w,
        Err(e) => {
            crate::log::line(&format!(
                "自更新：官方源校验和拿不到（{e:#}），改用多镜像一致的结果"
            ));
            fetch_sums_consensus(client, &tag, &setup, &sums_path, cancel, progress)?
        }
    };

    // 2) 拿安装程序：只走发布资产 + 镜像，不走 jsDelivr。
    //    jsDelivr 对 .exe / .dll 一律返回 403（文本文件不受影响），那条路必然失败，
    //    留着只会每次更新白搭一次请求。
    let _ = std::fs::remove_file(&dest);
    progress(0, 0, "安装程序");
    download_with_mirror(
        client,
        &self_release_url(&tag, &setup),
        &dest,
        cancel,
        progress,
        false,
        None,
    )
    .with_context(|| format!("下载 {setup} 失败"))?;
    crate::log::line("自更新：安装程序来自发布资产（镜像）");

    // 3) 校验：哈希对不上就丢弃，绝不动用户的程序
    let bytes = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    let got = crate::util::sha256_file(&dest)?;
    if !got.eq_ignore_ascii_case(&want) {
        let _ = std::fs::remove_file(&dest);
        bail!("下载的安装程序校验不通过（期望 {want}，实际 {got}），已丢弃，没有动你的程序");
    }
    let mut head = [0u8; 2];
    if let Ok(mut f) = std::fs::File::open(&dest) {
        use std::io::Read;
        let _ = f.read_exact(&mut head);
    }
    if bytes < 1_000_000 || &head != b"MZ" {
        let _ = std::fs::remove_file(&dest);
        bail!("下载的安装程序看着不对（{bytes} 字节），已丢弃");
    }
    crate::util::clear_motw(&dest);
    Ok(StagedUpdate {
        installer: dest,
        sha256: got,
        bytes,
    })
}

/// 把 new_exe 换成当前程序本体：当前 exe 先改名成 .old（这就是备份），再把新版写回原位；
/// 写失败会把 .old 改回来，绝不留一个半截程序。
pub fn swap_in_place(cur: &Path, new_exe: &Path) -> Result<PathBuf> {
    let old = cur.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(cur, &old)
        .with_context(|| format!("没法把当前程序改名成 {}", old.display()))?;
    match std::fs::copy(new_exe, cur) {
        Ok(_) => {
            crate::util::clear_motw(cur);
            let _ = std::fs::remove_file(new_exe);
            Ok(old)
        }
        Err(e) => {
            let _ = std::fs::rename(&old, cur);
            Err(anyhow::Error::new(e).context("写入新版程序失败，已还原成原来的程序"))
        }
    }
}

/// 启动时清掉上一次自更新留下的 .old / .new，返回删掉几个。
pub fn cleanup_self_update_leftovers() -> usize {
    let Ok(cur) = std::env::current_exe() else {
        return 0;
    };
    let mut n = 0;
    for ext in ["exe.old", "exe.new"] {
        let p = cur.with_extension(ext);
        if p.is_file() && std::fs::remove_file(&p).is_ok() {
            n += 1;
        }
    }
    n
}

// ------------------------------------------------------------------ 测速

fn rd_u16le(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn rd_u32le(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

/// zip 里一个条目的元信息（不解压）
#[derive(Debug, Clone)]
pub struct ZipMeta {
    pub name: String,
    pub method: u16,
    pub comp_size: u64,
    /// 解压后的字节数（来自中央目录）。用来给解压设上限（解压炸弹）。
    pub uncomp_size: u64,
    /// 中央目录里的 CRC32。解压完要对一遍 —— 内容坏了要当场发现。
    pub crc32: u32,
    pub local_off: usize,
}

/// 列出 zip 里的所有文件。
///
/// 只支持 store(0) / deflate(8) 两种压缩方式 —— 上游的源码包和运行库的包都是这两种，
/// 而 zip64 / 加密包直接报错，不猜。
///
/// 注意这里是**只读文件头和中央目录**，不把整包读进内存：上游源码包有一百多 MB，
/// 整包读进来会让内存爆掉（这个程序的卖点之一就是轻量）。
pub fn zip_list(zip_path: &Path) -> Result<Vec<ZipMeta>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f =
        std::fs::File::open(zip_path).with_context(|| format!("打开 {}", zip_path.display()))?;
    let len = f.metadata()?.len() as usize;
    if len < 22 {
        bail!("zip 文件太小，不像是有效的压缩包");
    }
    // 尾部最多 22 + 65535 字节里找 EOCD
    let tail_len = (22 + 65535).min(len);
    let mut tail = vec![0u8; tail_len];
    f.seek(SeekFrom::Start((len - tail_len) as u64))?;
    f.read_exact(&mut tail)?;
    let mut eocd = None;
    let mut i = tail_len - 22;
    loop {
        if &tail[i..i + 4] == b"PK\x05\x06" {
            eocd = Some(i);
            break;
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    let eocd = eocd.context("找不到 zip 中央目录结尾，可能不是有效的 zip")?;
    let count = rd_u16le(&tail, eocd + 10) as usize;
    let cd_off = rd_u32le(&tail, eocd + 16) as usize;
    if cd_off >= len {
        bail!("zip 中央目录偏移越界");
    }

    // 中央目录理论上可以很大，但绝不可能是几百 MB。给个上限，
    // 免得一个畸形 zip 让这里直接申请巨量内存（分配失败 = 进程直接 abort）。
    let cd_len = len - cd_off;
    if cd_len > 64 * 1024 * 1024 {
        bail!("zip 中央目录异常大（{cd_len} 字节），拒绝解析");
    }
    let mut cd = vec![0u8; cd_len];
    f.seek(SeekFrom::Start(cd_off as u64))?;
    f.read_exact(&mut cd)?;

    let mut out = Vec::new();
    let mut p = 0usize;
    for _ in 0..count {
        if p + 46 > cd.len() || &cd[p..p + 4] != b"PK\x01\x02" {
            break;
        }
        let method = rd_u16le(&cd, p + 10);
        let crc32 = rd_u32le(&cd, p + 16);
        let comp_size = rd_u32le(&cd, p + 20) as u64;
        let uncomp_size = rd_u32le(&cd, p + 24) as u64;
        // zip64 把真实值放在扩展段、这里留 0xFFFFFFFF 哨兵。本程序不支持 zip64：
        // 与其按哨兵去解压出垃圾，不如明确报错（注释一直这么写，实现以前没做）。
        if comp_size == 0xFFFF_FFFF || uncomp_size == 0xFFFF_FFFF {
            bail!("这个 zip 用了 zip64 格式，本程序不支持（请用普通 zip 重新打包）");
        }
        let name_len = rd_u16le(&cd, p + 28) as usize;
        let extra_len = rd_u16le(&cd, p + 30) as usize;
        let comment_len = rd_u16le(&cd, p + 32) as usize;
        let local_off = rd_u32le(&cd, p + 42) as usize;
        let name = cd
            .get(p + 46..p + 46 + name_len)
            .map(|b| String::from_utf8_lossy(b).to_string())
            .unwrap_or_default();
        out.push(ZipMeta {
            name,
            method,
            comp_size,
            uncomp_size,
            crc32,
            local_off,
        });
        p += 46 + name_len + extra_len + comment_len;
    }
    // 条目数对不上说明中央目录被截断/被别的工具改过：静默少解析几个文件
    // 会让用户看到「导入成功」而其实要的文件根本没被看到。
    if out.len() != count {
        bail!(
            "zip 中央目录不完整（声明 {count} 个条目，只解出 {} 个）",
            out.len()
        );
    }
    Ok(out)
}

/// 把 zip 里的某个条目**流式**解到 dest，返回解出来的字节数。
/// 流式的意义同上：一百多 MB 的包不能整包读进内存。
pub fn zip_extract_to(zip_path: &Path, meta: &ZipMeta, dest: &Path) -> Result<u64> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f =
        std::fs::File::open(zip_path).with_context(|| format!("打开 {}", zip_path.display()))?;
    f.seek(SeekFrom::Start(meta.local_off as u64))?;
    let mut head = [0u8; 30];
    f.read_exact(&mut head)?;
    if &head[0..4] != b"PK\x03\x04" {
        bail!("zip 本地头损坏");
    }
    let l_name = rd_u16le(&head, 26) as u64;
    let l_extra = rd_u16le(&head, 28) as u64;
    f.seek(SeekFrom::Start(
        meta.local_off as u64 + 30 + l_name + l_extra,
    ))?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out =
        std::fs::File::create(dest).with_context(|| format!("创建 {}", dest.display()))?;
    let limited = f.take(meta.comp_size);
    // 解压炸弹：只限压缩输入是不够的（deflate 最大能膨胀上千倍）。
    // 输出一旦超过上限就当场停手并删掉半成品，别把用户磁盘写满。
    // 上限取「硬上限」和「中央目录声明的解压后大小」里更小的那个：
    // 声明得小、实际很大（解压炸弹）时也会当场停手。
    let (n, got_crc) = {
        let mut capped = LimitedWriter {
            inner: &mut out,
            left: MAX_EXTRACT_BYTES.min(meta.uncomp_size.max(1)),
            crc: flate2::Crc::new(),
        };
        let n = match meta.method {
            0 => std::io::copy(&mut { limited }, &mut capped)?,
            8 => {
                let mut dec = flate2::read::DeflateDecoder::new(limited);
                std::io::copy(&mut dec, &mut capped)?
            }
            m => bail!("不支持的 zip 压缩方式 {m}（只支持存储和 deflate）"),
        };
        (n, capped.crc.sum())
    };
    out.flush()?;
    // CRC 对不上说明内容坏了（中央目录的 CRC 是解压后内容的校验和）
    if meta.crc32 != 0 && got_crc != meta.crc32 {
        drop(out);
        let _ = std::fs::remove_file(dest);
        bail!(
            "解压出来的内容和 zip 记录对不上（CRC32 {:08X} != {:08X}），已丢弃",
            got_crc,
            meta.crc32
        );
    }
    Ok(n)
}

/// 解压输出的硬上限。DLSS 运行库最大约 56 MB，256 MB 足够宽松。
const MAX_EXTRACT_BYTES: u64 = 256 * 1024 * 1024;

/// 带上限的写出器：超限就报错，同时算 CRC32。
struct LimitedWriter<W: std::io::Write> {
    inner: W,
    left: u64,
    crc: flate2::Crc,
}

impl<W: std::io::Write> std::io::Write for LimitedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() as u64 > self.left {
            return Err(std::io::Error::other(
                "解压结果超过上限（可能是解压炸弹），已中止",
            ));
        }
        self.crc.update(buf);
        self.left -= buf.len() as u64;
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// 从 zip 里解出**指定名字**的 .dll 到 out_path，返回该条目名。
///
/// 必须点名：以前取「第一个 .dll」，于是任何第三方包只要塞一个别的 NVIDIA 签名
/// DLL（旧版、或系统里别的 DLL 改名）就能顶替要装的运行库。
pub fn zip_extract_dll(zip_path: &Path, out_path: &Path, want: &str) -> Result<String> {
    let list = zip_list(zip_path)?;
    let want_l = want.to_ascii_lowercase();
    let meta = list
        .iter()
        .find(|m| {
            !m.name.ends_with('/')
                && m.name
                    .rsplit(['/', '\\'])
                    .next()
                    .map(|b| b.eq_ignore_ascii_case(&want_l))
                    .unwrap_or(false)
        })
        .with_context(|| format!("zip 里没有 {want}"))?;
    let tmp = part_path(out_path);
    let n = zip_extract_to(zip_path, meta, &tmp)?;
    if n == 0 {
        let _ = std::fs::remove_file(&tmp);
        bail!("解压结果是空文件");
    }
    util::atomic_replace(&tmp, out_path)?;
    util::clear_motw(out_path);
    Ok(meta.name.clone())
}

/// 一个待同步的资产文件（etag 为空表示没有指纹可比）。
pub struct SyncItem {
    pub local: String,
    pub path: String,
    pub dest: std::path::PathBuf,
    pub etag: Option<String>,
    pub size: u64,
}

/// 给这些 appid 拉 Steam 的**竖版**封面（library_600x900，600×900 正好是卡片 .72 的比例）。
///
/// 为什么要它：DLSS5 卡片墙是「看封面挑游戏」，但资产目录的 covers 空着时只能回退到
/// exe 图标（几十像素），拉满整张卡就糊成一片（用户反馈）。这里把真正的封面缓存下来。
///
/// 返回 (下载, 已有跳过, 失败)。**失败不算错** —— 界面回退到图标就是了，
/// 不该因为一张封面下不动就把整件事判失败。
pub fn fetch_steam_covers(
    client: &reqwest::blocking::Client,
    app_ids: &[String],
    mut on_progress: impl FnMut(String, f32),
) -> Result<(usize, usize, usize)> {
    let dir = util::assets_dir()?.join("covers");
    std::fs::create_dir_all(&dir)?;
    let total = app_ids.len().max(1);
    let mut fetched = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    for (i, app) in app_ids.iter().enumerate() {
        // appid 是要拼进 URL 的：只收纯数字，脏值直接跳过（界面上的是 Steam appid）
        if app.is_empty() || !app.chars().all(|c| c.is_ascii_digit()) {
            failed += 1;
            continue;
        }
        let dest = dir.join(format!("{app}.jpg"));
        if std::fs::metadata(&dest)
            .map(|m| m.len() > 1024)
            .unwrap_or(false)
        {
            skipped += 1;
            continue;
        }
        on_progress(
            format!("正在获取封面 {}/{}（{app}）…", i + 1, total),
            (i as f32) / (total as f32),
        );
        match fetch_one_cover(client, app, &dest) {
            Ok(()) => fetched += 1,
            Err(e) => {
                crate::log::line(&format!("[covers] {app} 没拉到：{e}"));
                failed += 1;
            }
        }
    }
    Ok((fetched, skipped, failed))
}

/// 依次试几个 Steam CDN；先竖版，没有再退横版 header（会被裁，但比没有强）。
fn fetch_one_cover(client: &reqwest::blocking::Client, app: &str, dest: &Path) -> Result<()> {
    const HOSTS: [&str; 3] = [
        "https://cdn.cloudflare.steamstatic.com",
        "https://cdn.akamai.steamstatic.com",
        "https://steamcdn-a.akamaihd.net",
    ];
    // 从最清楚的一档往下试：2x 竖版（1200×1800，Steam 对较新的游戏才有）
    // -> 普通竖版（名字是 600x900，但老游戏实际给的是 300×450，仍然够卡片用）
    // -> 横版 header（会被卡片裁掉两边，总比没有强）
    let paths = [
        format!("/steam/apps/{app}/library_600x900_2x.jpg"),
        format!("/steam/apps/{app}/library_600x900.jpg"),
        format!("/steam/apps/{app}/header.jpg"),
    ];
    for path in paths {
        for host in HOSTS {
            let url = format!("{host}{path}");
            let Ok(resp) = client.get(&url).send() else {
                continue;
            };
            if !resp.status().is_success() {
                continue;
            }
            let Ok(bytes) = resp.bytes() else { continue };
            // JPEG 魔数 + 体积上限：CDN 出错时回的是 HTML，别把它当封面存下来
            if bytes.len() < 1024 || bytes.len() > 8 * 1024 * 1024 || &bytes[0..2] != b"\xFF\xD8" {
                continue;
            }
            let tmp = dest.with_extension("jpg.part");
            std::fs::write(&tmp, &bytes)?;
            util::atomic_replace(&tmp, dest)?;
            util::clear_motw(dest);
            return Ok(());
        }
    }
    anyhow::bail!("所有 CDN 都没有这张封面")
}

/// 下载计划：要下哪些文件、总字节、跳过几个。
pub struct DownloadPlan {
    pub items: Vec<SyncItem>,
    pub skipped: usize,
    pub total: u64,
}

/// 更新日志在仓库里的路径（走 raw 取，和版本号同一套路：不占 API 配额、镜像也拿得到）。
pub const RELEASE_NOTES_PATH: &str = "docs/release-notes.md";

/// 取「本次更新内容」的正文（给更新界面显示用）。
///
/// 与 fetch_latest_self_version 的区别：这里只是给人看的文本，
/// **不参与「要不要更新」的判断**，所以不需要「两个镜像报同一份」那套防投毒逻辑，
/// 官方源优先、拿不到就用第一个成功的镜像即可。
pub fn fetch_release_notes(client: &reqwest::blocking::Client) -> Option<String> {
    let official =
        format!("https://raw.githubusercontent.com/{SELF_REPO}/main/{RELEASE_NOTES_PATH}");
    if let Some(t) = fetch_text_one(client, &official) {
        return Some(clip_notes(t));
    }
    for m in mirrors("") {
        if let Some(t) = fetch_text_one(client, &format!("{m}{official}")) {
            return Some(clip_notes(t));
        }
    }
    None
}

fn fetch_text_one(client: &reqwest::blocking::Client, url: &str) -> Option<String> {
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(8))
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let t = resp.text().ok()?;
    if t.trim().is_empty() {
        None
    } else {
        Some(t)
    }
}

/// 只留**最新那一节**，太长再截断（界面上还要能看，不要糊满整屏）。
///
/// 用户点「检查新版本」想看的是「这次更新了什么」。从最新版本一路列到 1.0.4 的话，
/// 中间那些没单独发布过的版本号（1.1.x 就是）会让人以为自己漏装过什么。
fn clip_notes(t: String) -> String {
    // 文件开头是标题行，第一个 "## " 之后才是最新版本那一节
    let body = match t.split_once("## ") {
        Some((_, rest)) => rest,
        None => return clip_len(t),
    };
    let section = match body.split_once("\n## ") {
        Some((head, _)) => format!("## {head}"),
        None => format!("## {body}"),
    };
    clip_len(section)
}

fn clip_len(s: String) -> String {
    const MAX: usize = 4000;
    if s.len() <= MAX {
        return s;
    }
    let mut cut = MAX;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n…（完整内容见发布页面）", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare_reads_three_parts() {
        assert!(is_newer("1.2.2", "1.2.0"));
        assert!(!is_newer("1.2.0", "1.2.2"));
        assert!(!is_newer("1.2.2", "1.2.2"));
        assert!(is_newer("v1.3.0", "1.2.9"));
    }

    /// 资产修复：只补缺失/大小不对的，覆盖不碰；源里也没有的核心文件要如实报出来。
    #[test]
    fn repair_copies_missing_and_keeps_existing() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base =
            std::env::temp_dir().join(format!("fgm-asset-repair-{}-{stamp}", std::process::id()));
        // 模拟安装目录：bundled-assets / bundled-sm / bundled-dlss5 三套
        let app = base.join("app");
        let assets = base.join("assets");
        let fg = app.join("bundled-assets");
        std::fs::create_dir_all(fg.join("dlssg_sm86")).unwrap();
        std::fs::create_dir_all(app.join("bundled-sm").join("rtx30")).unwrap();
        std::fs::create_dir_all(app.join("bundled-sm").join("rtx20")).unwrap();
        std::fs::create_dir_all(app.join("bundled-dlss5")).unwrap();
        std::fs::create_dir_all(assets.join("dlssg_sm86")).unwrap();
        std::fs::write(fg.join("version.dll"), b"PROXY-OK").unwrap();
        std::fs::write(fg.join("dlssg_sm86").join("a.dll"), b"NEW-DLL").unwrap();
        std::fs::write(fg.join("same.txt"), b"SAME").unwrap();
        std::fs::write(
            app.join("bundled-sm").join("rtx30").join("version.dll"),
            b"SM-DLL",
        )
        .unwrap();
        std::fs::write(
            app.join("bundled-sm").join("rtx20").join("version.dll"),
            b"SM20",
        )
        .unwrap();
        std::fs::write(app.join("bundled-dlss5").join("Uplift.fx"), b"FX").unwrap();
        // 资产目录：version.dll 缺失、a.dll 内容坏了一半（大小不对）、same.txt 原样；
        // SM / DLSS5 两套都没铺
        std::fs::write(assets.join("dlssg_sm86").join("a.dll"), b"OLD").unwrap();
        std::fs::write(assets.join("same.txt"), b"SAME").unwrap();

        let r = repair_assets_at(Some(&app), &assets).unwrap();
        assert!(
            r.repaired.iter().any(|x| x == "version.dll"),
            "{:?}",
            r.repaired
        );
        assert!(
            r.repaired.iter().any(|x| x == "dlssg_sm86\\a.dll"),
            "{:?}",
            r.repaired
        );
        assert!(
            r.repaired
                .iter()
                .any(|x| x == "bundled-sm\\rtx30\\version.dll"),
            "SM 组件的 version.dll 也要补：{:?}",
            r.repaired
        );
        assert!(
            r.repaired.iter().any(|x| x == "DLSS5\\Uplift.fx"),
            "{:?}",
            r.repaired
        );
        assert_eq!(r.present, 1, "same.txt 应该按原样跳过");
        assert_eq!(
            std::fs::read(assets.join("version.dll")).unwrap(),
            b"PROXY-OK"
        );
        assert_eq!(
            std::fs::read(assets.join("dlssg_sm86").join("a.dll")).unwrap(),
            b"NEW-DLL"
        );
        assert_eq!(
            std::fs::read(assets.join("bundled-sm").join("rtx30").join("version.dll")).unwrap(),
            b"SM-DLL"
        );
        // 源里没有的核心代理（winmm.dll 等）必须报 missing，界面才能提示「被杀软删了」
        assert!(
            r.missing.iter().any(|x| x == "winmm.dll"),
            "{:?}",
            r.missing
        );
        // SM 两份都在就不该误报
        assert!(
            !r.missing.iter().any(|x| x.contains("rtx30")),
            "{:?}",
            r.missing
        );

        std::fs::remove_dir_all(&base).ok();
    }
}
