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

use crate::scan;
use crate::util;
use std::sync::atomic::{AtomicBool, Ordering};

pub const REPO: &str = "sdli1995/dlssg_for_sm86";
pub const BRANCH: &str = "main";
pub const INI_REPO_PATH: &str = "dlssg_sm86.ini";

/// 代理入口 -> 仓库里的路径。
///
/// 关键：备用入口不是 version.dll 改个名，而是导出名不同的独立二进制
/// （体积都不一样），所以必须下载对应那一个，不能拿 version.dll 重命名。
///
/// 上游 0.3.0 把它们放在 alternatives/（去掉 winhttp、新增 dbghelp 与 d3d12），0.3.1 不变。
///
/// 仓库里还有一个 310.1/ 目录 —— 那是**上游自己的老版本**，我们不再使用：
/// 0.3.1 起根目录这一套文件 20 系（SM75）和 30 系（SM86）都能用，出厂 INI
/// 不需要改任何键（内核族按物理显卡自动选，见上游 README「0.3.1」一节）。
pub fn proxy_repo_path(proxy: &str) -> &'static str {
    match proxy {
        "winmm.dll" => "alternatives/winmm.dll",
        "dbghelp.dll" => "alternatives/dbghelp.dll",
        "dinput8.dll" => "alternatives/dinput8.dll",
        "dxgi.dll" => "alternatives/dxgi.dll",
        "d3d12.dll" => "alternatives/d3d12.dll",
        _ => "version.dll",
    }
}

pub fn local_name(repo_path: &str) -> String {
    repo_path.rsplit('/').next().unwrap_or(repo_path).to_owned()
}
const USER_AGENT: &str = "FrameGen-Manager/0.1 (+https://github.com/sdli1995/dlssg_for_sm86)";

/// GitHub 未登录 API 每分钟/小时的配额用尽时的提示
const RATE_LIMIT_MSG: &str = "GitHub 接口配额用完了（未登录每小时 60 次）。\
本地文件状态不受影响，等一会儿再点「检查更新」即可。";

/// 远端文件信息。由 `probe_remote` 用 HEAD 拿到，不消耗任何 API 配额。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteFile {
    /// 仓库里的路径，如 altnative/winmm.dll
    pub name: String,
    pub size: u64,
    /// 内容的 SHA-256（64 位小写十六进制）。来自响应头的 ETag。
    pub etag: String,
    /// 这个指纹**是不是官方源给的**。
    ///
    /// 镜像给的指纹和官方对不上（gh-proxy.com 返回的是弱标签
    /// `W/"11378bae..."`，内容哈希也完全是另一个值）。拿镜像的指纹去比"内容变没变"，
    /// 就会出现「同一个文件每次都被判定为需要更新」—— 也就是用户报的
    /// 「不断重复下载、停不下来」。所以指纹只在可信时参与判定。
    pub etag_trusted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalFile {
    /// 本地这份文件的 git blob sha1（显示用，也是老版本判断更新的依据）
    #[serde(default)]
    pub blob_sha: String,
    /// 下载时官方源给的 ETag。判断「有没有更新」就靠它。
    #[serde(default)]
    pub etag: String,
    pub sha256: String,
    pub bytes: u64,
    pub downloaded_at: String,
    /// 是不是用户「手动导入」放进来的（不是下载来的）。
    /// 这种记录没有官方指纹可比 —— 判定「已就绪」只比文件在不在、大小对不对。
    #[serde(default)]
    pub imported: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateState {
    pub version: Option<String>,
    pub files: BTreeMap<String, LocalFile>,
}

impl UpdateState {
    /// 远端指纹和本地记录不同 -> 有更新。
    ///
    /// 注意要传**本地文件名**（比如 winmm.dll），不是仓库路径
    /// （altnative/winmm.dll）—— files 表是按本地文件名做 key 的。
    /// 别拿 remote.name 去查：那是仓库路径，altnative 那四个会被永远判定成「有更新」。
    ///
    /// 老版本的记录里没有 etag 字段，这时按「需要更新」处理，
    /// 重新下载一次就会补上，属于一次性成本。
    pub fn needs_update(&self, local_name: &str, remote: &RemoteFile) -> bool {
        let Some(l) = self.files.get(local_name) else {
            return true;
        };
        // 同上：指纹不可信时不凭它说「有更新」
        if !remote.etag_trusted {
            return false;
        }
        l.etag.is_empty() || !l.etag.eq_ignore_ascii_case(&remote.etag)
    }
}

pub fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        // 对下载来说这个值是「单次读取」的上限，不是整段下载的总上限：
        // reqwest 的阻塞读每调用一次就重新计时，所以只要服务器还在往外吐字节，
        // 多慢都能慢慢下完 —— 用户线路慢不该被掐断。
        // （按「平均速度低于 300 KB/s 就换源」会把慢线路掐断，只下到四分之一。）
        // 它现在只兜底一件事：源彻底不动了 —— 连续 60 秒一个字节都没有才判它死。
        // 这个值同时也是「点取消之后最长还要等多久」：读卡住时取消要等这次读返回，
        // 300 秒会让用户以为程序死了（界面上的按钮全是灰的）。
        .timeout(Duration::from_secs(60))
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

/// 从一组候选里挑出**最大**的版本号。解析不出来的直接忽略 ——
/// 宁可什么都不提示，也不能因为一个乱七八糟的字符串就误报有新版本。
pub fn pick_latest(versions: impl IntoIterator<Item = String>) -> Option<String> {
    let mut best: Option<String> = None;
    for v in versions {
        if parse_version(&v).is_none() {
            continue;
        }
        if best.as_ref().map(|b| is_newer(&v, b)).unwrap_or(true) {
            best = Some(v);
        }
    }
    best
}

/// 问一个源，拿它报的版本号。失败返回 None。
fn fetch_self_version_one(client: &reqwest::blocking::Client, url: &str) -> Option<String> {
    let resp = client.get(url).timeout(Duration::from_secs(8)).send().ok()?;
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

fn content_length_of(resp: &reqwest::blocking::Response) -> u64 {
    resp.headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
}

/// HEAD 一个仓库文件，拿它的内容指纹。**不消耗 API 配额。**
///
/// 先问官方 raw，官方不通再问镜像 —— 两边都会返回 ETag，所以走镜像时
/// 同样能拿到期望哈希，下载完照样能校验。
pub fn probe_remote(client: &reqwest::blocking::Client, repo_path: &str) -> Result<RemoteFile> {
    let official = official_url(repo_path);
    let ms = mirrors("");
    try_sources(&official, &ms, false, None, |url, prefix| {
        let resp = client
            .head(url)
            // 单个 HEAD 只有 1KB 不到，6 秒足够。raw 现在会间歇性卡十几秒，
            // 不给单请求超时的话，官方优先反而变成「每次先干等十几秒」。
            .timeout(Duration::from_secs(6))
            .send()
            .map_err(|e| anyhow::anyhow!(friendly_error(&e)))?;
        if !resp.status().is_success() {
            bail!("返回 HTTP {}", resp.status().as_u16());
        }
        let size = content_length_of(&resp);
        let etag = etag_of(&resp).ok_or_else(|| anyhow::anyhow!("没返回可用的 ETag"))?;
        Ok(RemoteFile {
            name: repo_path.to_owned(),
            size,
            etag,
            // prefix 为空 = 官方源回答的，指纹可信
            etag_trusted: prefix.is_empty(),
        })
    })
    .map_err(|e| anyhow::anyhow!("拿不到 {repo_path} 的内容指纹（{e}）"))
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

pub fn fetch_version(client: &reqwest::blocking::Client) -> Option<String> {
    let official = format!("https://raw.githubusercontent.com/{REPO}/{BRANCH}/README.md");
    let ms = mirrors("");
    // 这里也要能换源 + 限时：raw 卡十几秒会把整个「检查更新」拖住，
    // 而它只是用来在界面上显示一个版本号而已。
    let fetched = try_sources(&official, &ms, false, None, |url, _src| {
        let resp = client
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status().as_u16());
        }
        resp.text().map_err(|e| anyhow::anyhow!("{e}"))
    })
    .ok();

    if let Some(text) = fetched {
        if let Some(v) = extract_version(&text) {
            return Some(v);
        }
    }
    // 兜底：本地已下载的 INI 第一行注释里也有版本号，网络抖一下不至于显示「未知」
    let ini = util::assets_dir().ok()?.join(INI_REPO_PATH);
    let text = std::fs::read_to_string(ini).ok()?;
    extract_version(&text)
}

/// 给「手动导入」用的上游版本探测：**有上限**，不能让网络把导入流程拖住。
///
/// 只试官方地址和第一个镜像、每次 5 秒；都不通就返回 None（界面退回到上次检查的缓存值，
/// 并如实标注「上次检查的结果」）。导入窗口前面已经等了用户几秒，这里再多等半分钟
/// 去挨个试完所有镜像是不合理的。
pub fn fetch_version_quick(client: &reqwest::blocking::Client) -> Option<String> {
    let official = format!("https://raw.githubusercontent.com/{REPO}/{BRANCH}/README.md");
    let mut urls = vec![official.clone()];
    if let Some(m) = mirrors("").into_iter().next() {
        urls.push(format!("{m}{official}"));
    }
    for url in urls {
        let Ok(resp) = client.get(&url).timeout(Duration::from_secs(5)).send() else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(text) = resp.text() else { continue };
        if let Some(v) = extract_version(&text) {
            return Some(v);
        }
    }
    None
}

/// 本机资产目录里那份 mod 的版本号。
///
/// 只有 0.2.4 那代在 INI 第一行写了版本（`; Native 0.2.4.`）；0.3.0 起 INI 里不带
/// 版本号，本地也没有别的可靠来源 —— 所以这里会返回 None。界面要如实显示「未知」，
/// 不要去猜一个数字挂在那里。
pub fn local_asset_version() -> Option<String> {
    let ini = util::assets_dir().ok()?.join(INI_REPO_PATH);
    let text = std::fs::read_to_string(ini).ok()?;
    extract_version(&text)
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
    let mut name = dest.file_name().map(|s| s.to_os_string()).unwrap_or_default();
    name.push(".part");
    dest.with_file_name(name)
}

/// 清理资产目录里遗留的 .part 文件（进程被强杀时会留下）
pub fn clean_stale_partials() -> usize {
    let Ok(dir) = util::assets_dir() else { return 0 };
    let Ok(rd) = std::fs::read_dir(&dir) else { return 0 };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(7 * 24 * 3600));
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

/// 本地这份文件是不是已经是最新版。
///
/// **必须同时满足三条**：下载记录在、指纹和远端一致、而且文件真的还在且大小对得上。
/// 只看记录会造成「文件被删了却认为无需下载」（asset_state 那边同理）。
pub fn local_is_current(
    state: &UpdateState,
    local_name: &str,
    dest: &Path,
    remote: &RemoteFile,
) -> bool {
    let Some(r) = state.files.get(local_name) else {
        return false;
    };
    // 手动导入的文件没有官方指纹可比（用户就是下不动才导入的）：
    // 只要文件还在、大小对，就算已就绪，别动不动又去下一遍。
    if r.imported {
        return std::fs::metadata(dest)
            .map(|m| m.len() == r.bytes)
            .unwrap_or(false);
    }
    // 指纹**只在可信时**才拿来判定。探测落到镜像时指纹和官方对不上，
    // 用它比对会导致「文件明明在、每次都被判为需要更新」—— 用户看到的就是
    // 「不断重复下载」。指纹不可信时退化成「记录在 + 文件在且大小对得上」。
    if remote.etag_trusted && (r.etag.is_empty() || !r.etag.eq_ignore_ascii_case(&remote.etag)) {
        return false;
    }
    // metadata 拿不到（文件不存在）就是 false
    std::fs::metadata(dest)
        .map(|m| m.len() == r.bytes)
        .unwrap_or(false)
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
    let Sink { dest, cancel, progress } = sink;
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

/// 下载记录放在**资产目录里**（`assets\update_state.json`），和资产同一个家。
///
/// 老版本固定写在 %APPDATA% 下（程序还叫 DLSSG-Manager 时是那个文件夹，后来是
/// FrameGen-Manager），和资产分了家：用户把整个文件夹
/// 拷到另一台机器（或清了 %APPDATA%），文件明明都在，工具却查不到记录，会被判定成
/// 「需要下载」而白下 17~19 MB 的代理 DLL —— 这正是和「解压即用」冲突的地方。
/// 现在记录跟着资产走（config / logs / game_library 本来就在程序同级）。
fn state_path() -> Result<PathBuf> {
    Ok(util::assets_dir()?.join("update_state.json"))
}

/// 老位置，只用于一次性的兼容读取（见 load_state）。
/// 两个都认：改名前后 %APPDATA% 下的文件夹名不一样（FrameGen-Manager / DLSSG-Manager）。
fn legacy_state_paths() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(d) = util::app_data_dir() {
        out.push(d.join("update_state.json"));
    }
    if let Some(d) = util::legacy_app_data_dir() {
        out.push(d.join("update_state.json"));
    }
    out
}

/// 读记录。Ok(Some) = 读到了；Ok(None) = 没有这个文件；Err = 文件在但坏了。
fn read_state(p: &Path) -> std::result::Result<Option<UpdateState>, String> {
    if !p.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(p).map_err(|e| e.to_string())?;
    match serde_json::from_str::<UpdateState>(&text) {
        Ok(st) => Ok(Some(st)),
        Err(e) => Err(e.to_string()),
    }
}

pub fn load_state() -> UpdateState {
    if let Ok(p) = state_path() {
        match read_state(&p) {
            Ok(Some(st)) => return st,
            // 记录坏了**不能静默当成「没有记录」**：下一次保存会用空记录把它覆盖掉，
            // 用户再也查不出「为什么所有资产都被要求重新下载」。留一份 .corrupt 证据。
            Err(e) => {
                crate::log::line(&format!(
                    "下载记录解析失败（{e}）：已留一份 update_state.json.corrupt，本次按空记录处理"
                ));
                let _ = std::fs::rename(&p, p.with_extension("json.corrupt"));
            }
            Ok(None) => {}
        }
    }
    // 老版本留下的记录：读出来顺手迁到新位置。用户升级上来不该因为「记录换了地方」
    // 就重新下载一遍资产。老文件不删 —— 万一用户想退回去用老版本，那边还能用。
    for old in legacy_state_paths() {
        if let Ok(Some(st)) = read_state(&old) {
            let _ = save_state(&st);
            crate::log::line(&format!(
                "下载记录已从 {} 迁到资产目录（记录跟着资产走，换机器不用重下）",
                old.display()
            ));
            return st;
        }
    }
    UpdateState::default()
}

pub fn save_state(state: &UpdateState) -> Result<()> {
    // **原子写。**直接 write 会先把原文件截断：中途断电/被杀就只剩半截 JSON，
    // 下次启动所有资产都会被判成「需要重新下载」（白下十几 MB）。
    let p = state_path()?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    crate::util::atomic_replace(&tmp, &p).context("保存下载记录失败")
}

pub fn asset_path(name: &str) -> Result<PathBuf> {
    Ok(util::assets_dir()?.join(name))
}

/// 清空下载记录（返回清掉了几条）。
///
/// 一次性迁移用：老版本可以切到 310.1 版，那些用户本地那份 version.dll 是旧的。
/// 探测落到镜像时指纹不可信，判定会退化成「记录里的字节数 == 远端字节数」——
/// 旧记录和旧文件对得上，于是**静默跳过下载**，用户以为升级了其实还是老版本。
/// 所以启动时把记录清一次，逼它重新下一份正确的。
pub fn clear_download_records() -> usize {
    let mut st = load_state();
    let n = st.files.len();
    if n > 0 {
        st.files.clear();
        let _ = save_state(&st);
    }
    n
}

// ------------------------------------------------- 部署用的 INI（只改用户主动选的档位）

/// 出厂默认的「优化等级」（上游 0.3.2 的出厂 INI：Optimized=1）
pub const DEFAULT_OPTIMIZED: u8 = 1;
/// 出厂默认的「倍率上限」（3 = 最高 4X；5 = 最高 6X）
pub const DEFAULT_MAX_FRAMES: u8 = 3;

/// 把「键=值」这一行改掉，其它内容（包括注释和顺序）原样保留。
/// 找不到这个键就返回 None —— 调用方保持原样并说明一句，而不是报错。
fn ini_set_key(text: &str, key: &str, value: &str) -> Option<String> {
    let mut found = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if !t.starts_with(';') && !t.starts_with('#') {
            if let Some(rest) = t.strip_prefix(key) {
                if rest.strip_prefix('=').is_some() {
                    out.push_str(&format!("{key}={value}"));
                    if line.ends_with('\n') {
                        out.push('\n');
                    }
                    found = true;
                    continue;
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

/// 准备要部署的 INI，返回（文件路径, 给人看的改动说明）。
///
/// 出厂 INI 本来就是对的，所以**两个值都是默认值时直接返回资产里那份原文件，一个字都不改**；
/// 只有用户主动选了别的档位才写一份 dlssg_sm86.deploy.ini（资产里那份原文件始终不动）。
///
/// optimized：0 原厂不加速 / 1 加速且画面与官方逐位一致（出厂默认）/ 2 再加有损图像内核 / 3 全部有损
/// max_frames：3 = 最高 4X（出厂默认）/ 5 = 最高 6X（仅 310.9 版，且要游戏自带插件支持）
pub fn prepare_deploy_ini(
    upstream: &Path,
    optimized: u8,
    max_frames: u8,
) -> Result<(PathBuf, Vec<String>)> {
    if !upstream.is_file() {
        bail!("还没有下载 {INI_REPO_PATH}, 请先点「下载 / 更新资产」");
    }
    if optimized == DEFAULT_OPTIMIZED && max_frames == DEFAULT_MAX_FRAMES {
        return Ok((upstream.to_path_buf(), Vec::new()));
    }
    let mut out = std::fs::read_to_string(upstream).context("读取 INI 失败")?;
    let mut notes: Vec<String> = Vec::new();
    for (key, value, what) in [
        ("Optimized", optimized.to_string(), "优化等级"),
        ("MaxGeneratedFrames", max_frames.to_string(), "倍率上限"),
    ] {
        match ini_set_key(&out, key, &value) {
            Some(new) => {
                notes.push(format!("{what} {key}={value}"));
                out = new;
            }
            // 上游以后改键名/精简掉这一项时不能报错，照旧部署、说明一句
            None => notes.push(format!("{what}：这份 INI 里没有 {key} 这一行，保持原样")),
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
        if let Ok(t) = serde_json::to_string_pretty(s) {
            let _ = std::fs::write(p, t);
        }
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
    s.entries.insert(prefix.to_owned(), SpeedSample { kbps, at: now_unix() });
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

/// release 资产的直链。github.com 直连不通时，调用方会在前面拼镜像前缀。
pub fn release_url(tag: &str, asset_name: &str) -> String {
    format!("https://github.com/{DLSS_REPO}/releases/download/{tag}/{asset_name}")
}

#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    pub tag: String,
    pub asset_name: String,
    pub url: String,
    pub size: u64,
}

fn pick_zip_asset(release: &serde_json::Value, tag: &str) -> Option<ReleaseAsset> {
    for a in release.get("assets")?.as_array()? {
        let name = a.get("name").and_then(|x| x.as_str()).unwrap_or_default();
        if !name.to_ascii_lowercase().ends_with(".zip") {
            continue;
        }
        let url = a
            .get("browser_download_url")
            .and_then(|x| x.as_str())?
            .to_owned();
        let size = a.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
        return Some(ReleaseAsset {
            tag: tag.to_owned(),
            asset_name: name.to_owned(),
            url,
            size,
        });
    }
    None
}

/// 找 release 里的 zip 资产。
/// 先按 preferred_tag 精确找；找不到就退到最新的、tag 以 prefix 开头的 release，
/// 这样指定版本被删掉时不会直接失败。
pub fn find_release_zip(
    client: &reqwest::blocking::Client,
    repo: &str,
    prefix: &str,
    preferred_tag: &str,
) -> Result<ReleaseAsset> {
    let url = format!("https://api.github.com/repos/{repo}/releases/tags/{preferred_tag}");
    if let Ok(resp) = client.get(&url).send() {
        if resp.status().is_success() {
            if let Ok(text) = resp.text() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(a) = pick_zip_asset(&v, preferred_tag) {
                        return Ok(a);
                    }
                }
            }
        }
    }

    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=30");
    let resp = client.get(&url).send().context("查询 Releases 失败")?;
    let status = resp.status();
    if status.as_u16() == 403 || status.as_u16() == 429 {
        bail!(RATE_LIMIT_MSG);
    }
    if !status.is_success() {
        bail!("查询 Releases 返回 HTTP {}", status.as_u16());
    }
    let text = resp.text().context("读取响应失败")?;
    let list: Vec<serde_json::Value> =
        serde_json::from_str(&text).context("解析 Releases JSON 失败")?;
    for r in &list {
        let tag = r.get("tag_name").and_then(|x| x.as_str()).unwrap_or_default();
        if tag.starts_with(prefix) {
            if let Some(a) = pick_zip_asset(r, tag) {
                return Ok(a);
            }
        }
    }
    bail!("在 {repo} 里找不到 tag 以 {prefix} 开头的 release 资产")
}

/// HEAD 任意 URL，拿 (大小, ETag)。发布资产也有 Content-Length，够界面显示用了。
/// 同样先官方后镜像，失败返回 None（只是显示不出大小，不影响下载）。
pub fn probe_url(client: &reqwest::blocking::Client, url: &str) -> Option<(u64, String)> {
    // release 直链是 github.com，墙内直连要干等连接超时 —— 而这里只是问个文件大小
    // 给界面显示，没有「指纹必须来自 GitHub」的要求（zip 解压后靠 NVIDIA 签名校验），
    // 所以直接走镜像优先。
    let ms = mirrors("");
    try_sources(url, &ms, true, None, |u, _src| {
        let r = client
            .head(u)
            .timeout(Duration::from_secs(6))
            .send()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if !r.status().is_success() {
            bail!("HTTP {}", r.status().as_u16());
        }
        Ok((content_length_of(&r), etag_of(&r).unwrap_or_default()))
    })
    .ok()
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
    let ms = if official_only { Vec::new() } else { mirrors("") };
    // 报错/日志里用来称呼这个文件（比如 nvngx_dlssg_310.9.1.zip）
    let label = dest.file_name().and_then(|n| n.to_str()).unwrap_or("运行库");
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

    // 1) 发布时给出的 SHA256 清单。**只走官方源，不走镜像。**
    //    以前这里和安装包一样走镜像：于是「哈希」和「被哈希的文件」来自同一台
    //    第三方镜像，那份校验等于自己给自己发证 —— 镜像换掉两个文件就能通过，
    //    而下一步是静默执行安装包。清单只有几百字节，走官方源不慢。
    progress(0, 0, "SHA256SUMS.txt");
    download_with_mirror(
        client,
        &self_release_url(&tag, "SHA256SUMS.txt"),
        &sums_path,
        cancel,
        progress,
        true,
        None,
    )
    .context("下载 SHA256SUMS.txt 失败（校验和必须来自官方源，不能走镜像；网络不通时请到发布页手动下载）")?;
    let sums = std::fs::read_to_string(&sums_path).context("读 SHA256SUMS.txt 失败")?;
    let want = parse_sha256sums(&sums, &setup).with_context(|| {
        format!("这次发布的 SHA256SUMS.txt 里没有 {setup}（老版本没带这个文件），只能手动下载安装包")
    })?;

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
    std::fs::rename(cur, &old).with_context(|| format!("没法把当前程序改名成 {}", old.display()))?;
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

/// 一个源的测速结果。
#[derive(Debug, Clone)]
pub struct SourceSpeed {
    /// 镜像前缀。空串 = 官方源（这一行只做展示，不可选）。
    pub prefix: String,
    /// 给人看的名字
    pub label: String,
    /// 测速值（KB/s）。error 非空时无意义。
    pub kbps: u64,
    pub error: Option<String>,
}

/// 测速时读的样本大小。够判断数量级了，又不会真的把 15 MB 拖下来。
const SPEED_TEST_BYTES: u64 = 512 * 1024;
/// 单个源的测速上限。慢源不能在测速阶段把用户卡住。
const SPEED_TEST_CAP_SECS: f64 = 4.0;

/// 测一个源的下载速率：只读前若干字节就断开，**不落盘、不校验**。
fn measure_source(client: &reqwest::blocking::Client, url: &str, cancel: &AtomicBool) -> Result<u64> {
    let mut resp = client
        .get(url)
        .timeout(Duration::from_secs(SPEED_TEST_CAP_SECS as u64 + 6))
        .send()
        .map_err(|e| anyhow::anyhow!(friendly_error(&e)))?;
    if !resp.status().is_success() {
        bail!("HTTP {}", resp.status().as_u16());
    }
    let t0 = Instant::now();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut got: u64 = 0;
    while got < SPEED_TEST_BYTES {
        if cancelled(Some(cancel)) {
            bail!("{}", CANCELLED_MSG);
        }
        if t0.elapsed().as_secs_f64() >= SPEED_TEST_CAP_SECS {
            break;
        }
        let n = resp.read(&mut chunk).map_err(|e| anyhow::anyhow!("{e}"))?;
        if n == 0 {
            break;
        }
        got += n as u64;
    }
    let el = t0.elapsed().as_secs_f64();
    // 连 32 KB 都拿不到就别报速率了，报上去会误导用户
    if got < 32 * 1024 || el <= 0.05 {
        bail!("{:.1} 秒里只拿到 {} 字节", el, got);
    }
    Ok((got as f64 / el / 1024.0) as u64)
}

/// 把所有候选源测一遍。顺序：官方 raw、各镜像、官方 Release 直链。
///
/// 为什么这件事必须由用户自己的机器来做：镜像快慢是按**用户线路**变的。
/// 开发者这边 gh-proxy 快，不代表用户的线路也快，反过来也一样。
pub fn speed_test_all(
    client: &reqwest::blocking::Client,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(String),
) -> Vec<SourceSpeed> {
    let raw = official_url("version.dll");
    let mut targets: Vec<(String, String, String)> = Vec::new();
    targets.push(("官方源（raw）".to_owned(), String::new(), raw.clone()));
    for m in MIRRORS {
        targets.push((source_label(m), m.to_owned(), format!("{m}{raw}")));
    }
    targets.push((
        "官方源（Release 直链）".to_owned(),
        String::new(),
        release_url("dlssg-310.9.1", "nvngx_dlssg_310.9.1.zip"),
    ));

    let mut out = Vec::new();
    for (label, prefix, url) in targets {
        if cancelled(Some(cancel)) {
            break;
        }
        on_progress(format!("正在测速：{label}"));
        let r = measure_source(client, &url, cancel);
        let (kbps, error) = match r {
            Ok(k) => {
                // 官方源的那两行不记：下载顺序里官方永远排最后，记了也没用
                if !prefix.is_empty() {
                    record_speed(&prefix, k);
                }
                (k, None)
            }
            Err(e) => (0, Some(e.to_string())),
        };
        out.push(SourceSpeed { prefix, label, kbps, error });
    }
    out
}

// ---------------------------------------------------------------- ZIP 单文件解压
//
// 只需要从 zip 里取出一个 DLL，所以手写一个最小读取器：
// 尾部找中央目录 -> 选中 .dll 条目 -> 读本地头算数据偏移 -> flate2 解压。
// 这样就不必引入 zip crate。

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
    let mut f = std::fs::File::open(zip_path)
        .with_context(|| format!("打开 {}", zip_path.display()))?;
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
    let mut f = std::fs::File::open(zip_path)
        .with_context(|| format!("打开 {}", zip_path.display()))?;
    f.seek(SeekFrom::Start(meta.local_off as u64))?;
    let mut head = [0u8; 30];
    f.read_exact(&mut head)?;
    if &head[0..4] != b"PK\x03\x04" {
        bail!("zip 本地头损坏");
    }
    let l_name = rd_u16le(&head, 26) as u64;
    let l_extra = rd_u16le(&head, 28) as u64;
    f.seek(SeekFrom::Start(meta.local_off as u64 + 30 + l_name + l_extra))?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = std::fs::File::create(dest)
        .with_context(|| format!("创建 {}", dest.display()))?;
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
    fn flush(&mut self) -> std::io::Result<()> { self.inner.flush() }
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

/// 一个待下载的 DLSS 运行库。界面算「总进度」要用到它的 size。
#[derive(Debug, Clone)]
pub struct RuntimeStep {
    pub prefix: &'static str,
    pub tag: &'static str,
    pub zip_name: &'static str,
    pub dll_name: &'static str,
    pub label: &'static str,
    pub url: String,
    pub size: u64,
}

/// 进度条用的上下文。字节数决定进度条走多远，步数只用来写「第 n/N 步」。
#[derive(Debug, Clone, Copy)]
pub struct ProgressCtx {
    pub base_bytes: u64,
    pub total_bytes: u64,
    pub base_step: usize,
    pub total_steps: usize,
}

/// 列出**还需要下载**的 DLSS 运行库：本地已有且是 NVIDIA 签名的不列进来。
/// size 是 HEAD 问来的（失败就是 0，只影响进度条的分母）。
pub fn dlss_runtime_plan(client: &reqwest::blocking::Client) -> Vec<RuntimeStep> {
    let mut out = Vec::new();
    for (prefix, tag, zip_name, dll_name, label) in DLSS_RUNTIME {
        let have = asset_path(dll_name)
            .map(|p| p.is_file() && scan::identify_dll(&p) == scan::FileIdentity::Nvidia)
            .unwrap_or(false);
        if have {
            continue;
        }
        let url = release_url(tag, zip_name);
        let size = probe_url(client, &url).map(|(n, _)| n).unwrap_or(0);
        out.push(RuntimeStep {
            prefix,
            tag,
            zip_name,
            dll_name,
            label,
            url,
            size,
        });
    }
    out
}

/// 把 plan 里的运行库逐个下下来：下载 -> 解压 -> 删掉压缩包 -> 校验 NVIDIA 签名。
/// 返回全部 DLL 的本地路径（包括本来就有的）。
///
/// 进度按**全局字节**报：ctx.base_bytes 是这批之前已经下好的字节数。
/// 这样界面上的进度条是「总进度」，不会每换一个文件就回零。
pub fn ensure_dlss_runtime(
    client: &reqwest::blocking::Client,
    cancel: &AtomicBool,
    plan: &[RuntimeStep],
    ctx: ProgressCtx,
    mut progress: impl FnMut(String, f32),
) -> Result<Vec<PathBuf>> {
    let dir = util::assets_dir()?;
    let mut out = Vec::new();

    // 已经在本地的也一起返回，只是它们不在 plan 里（不用再下）
    for (_p, _t, _z, dll_name, _l) in DLSS_RUNTIME {
        let p = dir.join(dll_name);
        if p.is_file() && scan::identify_dll(&p) == scan::FileIdentity::Nvidia {
            out.push(p);
        }
    }

    let frac = |done: u64| -> f32 {
        if ctx.total_bytes > 0 {
            (done as f64 / ctx.total_bytes as f64).min(1.0) as f32
        } else {
            0.0
        }
    };

    let mut done = ctx.base_bytes;
    for (i, step) in plan.iter().enumerate() {
        if cancelled(Some(cancel)) {
            bail!("{}", CANCELLED_MSG);
        }
        let (prefix, tag, zip_name, dll_name, label) =
            (step.prefix, step.tag, step.zip_name, step.dll_name, step.label);
        let step_no = ctx.base_step + i + 1;
        let dest = dir.join(dll_name);
        let zip_path = dir.join(zip_name);
        progress(
            format!("第 {step_no}/{} 步 · 下载 {label}...", ctx.total_steps),
            frac(done),
        );

        let direct = {
            let mut relay = |got: u64, len: u64, src: &str| {
                let t = if len > 0 { len } else { step.size };
                progress(
                    format!(
                        "第 {step_no}/{} 步 · 下载 {label} {} / {} · {}",
                        ctx.total_steps,
                        util::format_bytes(got),
                        util::format_bytes(t),
                        src
                    ),
                    frac(done + got),
                );
            };
            // 运行库包没有官方指纹可比（发布 API 只给大小），所以这里至少把
            // 「发布侧报的字节数」当一道闸：内容换了个大小不一样的就直接丢弃换源。
            download_with_mirror(client, &step.url, &zip_path, cancel, &mut relay, false, Some(step.size))
        };

        let got_bytes: u64 = match direct {
            Ok(n) => n,
            Err(e) => {
                // 直链彻底失败才回退去问 releases API：作者删包 / 改名时会走到这里
                let asset = find_release_zip(client, DLSS_REPO, prefix, tag).map_err(|e2| {
                    anyhow::anyhow!("直链下载失败（{e}）；改用 Releases 接口也没成功：{e2}")
                })?;
                progress(
                    format!(
                        "第 {step_no}/{} 步 · {label} 改用 Releases 接口重试",
                        ctx.total_steps
                    ),
                    frac(done),
                );
                let mut relay = |got: u64, len: u64, src: &str| {
                    let t = if len > 0 { len } else { asset.size };
                    progress(
                        format!(
                            "第 {step_no}/{} 步 · 下载 {label} {} / {} · {}",
                            ctx.total_steps,
                            util::format_bytes(got),
                            util::format_bytes(t),
                            src
                        ),
                        frac(done + got),
                    );
                };
                download_with_mirror(
                    client,
                    &asset.url,
                    &zip_path,
                    cancel,
                    &mut relay,
                    false,
                    Some(asset.size),
                )?
            }
        };
        done += got_bytes;

        progress(
            format!("第 {step_no}/{} 步 · 解压 {label} ...", ctx.total_steps),
            frac(done),
        );
        // 点名要哪一份：不接受「包里第一个 .dll」，否则塞个别的 NVIDIA 签名 DLL 就能顶替
        let extracted = zip_extract_dll(&zip_path, &dest, step.dll_name)?;
        // 按用户要求：解压完就删掉压缩包
        let _ = std::fs::remove_file(&zip_path);

        let id = scan::identify_dll(&dest);
        if id != scan::FileIdentity::Nvidia {
            let _ = std::fs::remove_file(&dest);
            bail!(
                "{label} 解出来的 {extracted} 不是 NVIDIA 官方签名（判定为 {}），已丢弃",
                id.label()
            );
        }
        let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        progress(
            format!(
                "第 {step_no}/{} 步 · {label} 就绪（已校验 NVIDIA 签名，{}）",
                ctx.total_steps,
                util::format_bytes(size)
            ),
            frac(done),
        );
        out.push(dest);
    }
    Ok(out)
}

