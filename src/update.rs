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
//! 替代方案（都已实测）：
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

use crate::scan::{self, GpuRoute};
use crate::util;
use std::sync::atomic::{AtomicBool, Ordering};

pub const REPO: &str = "sdli1995/dlssg_for_sm86";
pub const BRANCH: &str = "main";
pub const INI_REPO_PATH: &str = "dlssg_sm86.ini";

/// 代理入口 -> 仓库里的路径。
/// 关键：altnative/ 下的四个不是 version.dll 改个名，而是导出名不同的独立二进制
/// （体积都不一样），所以必须下载对应那一个，不能拿 version.dll 重命名。
pub fn proxy_repo_path(proxy: &str) -> &'static str {
    match proxy {
        "winmm.dll" => "altnative/winmm.dll",
        "dinput8.dll" => "altnative/dinput8.dll",
        "winhttp.dll" => "altnative/winhttp.dll",
        "dxgi.dll" => "altnative/dxgi.dll",
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
    /// 早先用 remote.name 查，导致 altnative 那四个永远被判定成「有更新」。
    ///
    /// 老版本的记录里没有 etag 字段，这时按「需要更新」处理，
    /// 重新下载一次就会补上，属于一次性成本。
    pub fn needs_update(&self, local_name: &str, remote_etag: &str) -> bool {
        self.files
            .get(local_name)
            .map(|l| l.etag.is_empty() || !l.etag.eq_ignore_ascii_case(remote_etag))
            .unwrap_or(true)
    }
}

pub fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        // 总超时只用来兜底「服务器彻底不响应」。判「慢」交给看门狗
        // （见 WATCHDOG_* / min_kbps），它在 3 秒内就能把慢源踢掉，
        // 比让用户干等一个总超时有用得多。
        // 300 秒是按最坏情况算的：28.9 MB 的运行库压缩包在 300 KB/s 下约 96 秒。
        .timeout(Duration::from_secs(300))
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

/// 原来这里记的是「每个主机上次哪个候选**成功**了」，现在换成了记**实测速率**
/// （见下面的 SourceSpeeds）。区别很重要：镜像的快慢是按用户线路和时间变的，
/// 「上次能连上」不代表「这次够快」，而慢源不换掉就是用户抱怨的那个问题。

/// 按候选顺序依次尝试，第一个成功的胜出。
///
/// **探测和下载的优先级是反的，这是故意的**：
/// * 探测（HEAD，拿 ETag 指纹）走**官方优先** —— 指纹要从 GitHub 自己那里拿才可信。
///   HEAD 很小，官方 raw 即使是慢速链路也能秒回。
/// * 下载走**镜像优先**，镜像之间再按**实测速率**从快到慢排（见 rank_mirrors）。
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
///   * gh-proxy 这类镜像**只代理资源文件、拒绝代理网页**（实测直接回
///     "Web page content is not allowed"），所以 releases 页面和 releases.atom 都抓不到；
///   * 而 raw 上的 Cargo.toml 只有 2KB，官方源和镜像都拿得到，且不占配额。
///
/// 前提：发布流程是「改版本号 -> 提交 -> 打标签 -> 发 Release」一条龙，
/// 所以 main 上的版本号等于最新已发布版本。改流程的话这里要跟着改。
///
/// **为什么不只信第一个成功的源**：raw.githubusercontent.com 前面有 CDN 缓存，
/// 仓库里刚改完 Cargo.toml 的那几分钟，缓存还在吐旧内容。实测发 0.4.0 时官方 raw
/// 有约 3 分钟仍然说 0.3.0 —— 而官方恰好是优先源，一旦它「成功」返回就直接采信了，
/// 于是还停在 0.3.0 的用户被告知「已是最新」，根本看不到更新提示。各家的缓存时机
/// 不一样，所以这里改成**所有源都问、取最大的版本号**。
pub fn fetch_latest_self_version(client: &reqwest::blocking::Client) -> Option<String> {
    let official = format!("https://raw.githubusercontent.com/{SELF_REPO}/main/Cargo.toml");
    let mut urls = vec![official.clone()];
    for m in mirrors("") {
        urls.push(format!("{m}{official}"));
    }

    // 所有源**并发**问，取报出来的最大版本号。
    // 并发是为了让总耗时约等于最慢的那一个请求，而不是几个请求相加。
    let (tx, rx) = std::sync::mpsc::channel();
    for url in urls {
        let c = client.clone();
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(fetch_self_version_one(&c, &url));
        });
    }
    // 把主线程手里这个发送端丢掉，rx.iter() 才会在那些线程都结束后收完
    drop(tx);

    pick_latest(rx.iter().flatten())
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
/// raw.githubusercontent.com（以及实测会透传的 ghproxy 镜像）返回的 ETag 就是
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
    try_sources(&official, &ms, false, None, |url, _src| {
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
        })
    })
    .map_err(|e| anyhow::anyhow!("拿不到 {repo_path} 的内容指纹（{e}）"))
}

/// 版本号同时出现在两处，格式略有不同：
///   README 首行  "# DLSSG Native 0.2.4"
///   INI 首行注释 "; Native 0.2.4. Restart the game after changing this file."
/// 所以只认 "Native " 这个锚点，两边都能匹配。
pub fn extract_version(text: &str) -> Option<String> {
    let idx = text.find("Native ")?;
    let rest = &text[idx + "Native ".len()..];
    let v: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let v = v.trim_end_matches('.').to_owned();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
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
    let mut n = 0;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map(|x| x == "part").unwrap_or(false) && std::fs::remove_file(&p).is_ok() {
            n += 1;
        }
    }
    n
}

/// 本地这份文件是不是已经是最新版。
///
/// **必须同时满足三条**：下载记录在、指纹和远端一致、而且文件真的还在且大小对得上。
/// 只看记录会造成「文件被删了却认为无需下载」—— asset_state 那边踩过同样的坑。
pub fn local_is_current(
    state: &UpdateState,
    local_name: &str,
    dest: &Path,
    remote_etag: &str,
) -> bool {
    let Some(r) = state.files.get(local_name) else {
        return false;
    };
    if r.etag.is_empty() || !r.etag.eq_ignore_ascii_case(remote_etag) {
        return false;
    }
    // metadata 拿不到（文件不存在）就是 false
    std::fs::metadata(dest)
        .map(|m| m.len() == r.bytes)
        .unwrap_or(false)
}

/// 自动选源下载一个仓库文件。这就是界面上「下载 / 更新资产」走的路径，
/// 不用用户再手点「改用备用源」。
///
/// **prefer_mirror 是有讲究的：**
/// * 大文件（15 MB 的代理 DLL）传 true —— 镜像实测快几十倍。代价是
///   gh-proxy.com **不转发 ETag**，那边 ETag 比对会落空，必须靠 `verify` 里的
///   签名校验兜住（这 5 个代理 DLL 都有本项目签名，镜像伪造不出来）。
/// * 小文件（581 B 的 ini）传 false —— 官方源再慢也是瞬间，而且官方**会**给
///   ETag，比对能真正生效。ini 没有签名，只能靠这个。
///
/// `verify` 失败时返回 Err 就会自动换下一个源重试。
pub fn download_auto(
    client: &reqwest::blocking::Client,
    repo_path: &str,
    dest: &Path,
    expect_etag: Option<&str>,
    cancel: &AtomicBool,
    custom_prefix: &str,
    prefer_mirror: bool,
    min_kbps: u64,
    verify: &dyn Fn(&Path) -> Result<()>,
    progress: &mut dyn FnMut(u64, u64, &str),
) -> Result<Downloaded> {
    let official = official_url(repo_path);
    let ms = mirrors(custom_prefix);
    let too_slow = AtomicBool::new(false);

    match download_pass(
        client, repo_path, dest, expect_etag, cancel, &official, &ms, prefer_mirror, min_kbps,
        verify, &too_slow, progress,
    ) {
        Ok(v) => Ok(v),
        Err(e) => {
            // 所有源都低于阈值时不能就这么失败：退回「不看速率」再试一遍。
            // 慢一点也总比下不下来强，用户至少还有取消按钮。
            if min_kbps > 0 && too_slow.load(Ordering::Relaxed) {
                // 一个达标的源都没有 —— 那就别挑速度了，用手上最快的那个继续
                progress(0, 0, "几个源都不够快，用最快的那个继续");
                download_pass(
                    client, repo_path, dest, expect_etag, cancel, &official, &ms, prefer_mirror, 0,
                    verify, &too_slow, progress,
                )
                .map_err(|e2| anyhow::anyhow!("{e2}（放宽速度要求后重试仍失败；先前：{e}）"))
            } else {
                Err(e)
            }
        }
    }
}

/// 按候选顺序走一遍。抽成函数是因为「太慢」时要能整体再走一遍，
/// 写成一个闭包会和 progress 的可变借用打架。
#[allow(clippy::too_many_arguments)]
pub fn download_pass(
    client: &reqwest::blocking::Client,
    repo_path: &str,
    dest: &Path,
    expect_etag: Option<&str>,
    cancel: &AtomicBool,
    official: &str,
    ms: &[String],
    prefer_mirror: bool,
    min_kbps: u64,
    verify: &dyn Fn(&Path) -> Result<()>,
    too_slow: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64, &str),
) -> Result<Downloaded> {
    try_sources(official, ms, prefer_mirror, Some(cancel), |url, src| {
        match download(
            client, repo_path, dest, url, expect_etag, cancel, src, min_kbps, progress,
        ) {
            Ok(dl) => {
                verify(dest)?;
                Ok(dl)
            }
            Err(e) => {
                if e.to_string().starts_with(TOO_SLOW_PREFIX) {
                    too_slow.store(true, Ordering::Relaxed);
                }
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

/// 下载并校验。
///
/// - url 由调用方决定（官方源或备用源）
/// - cancel 置位时立刻中断，且磁盘上不留任何残留
///
/// 校验方式（不再依赖 GitHub API）：
///   1. 先 HEAD 拿到内容指纹（ETag），下载后用**响应里的 ETag** 和它比对；
///   2. 再比对 Content-Length，防止被截断。
///
/// 说明：raw.githubusercontent.com 的 ETag 是 GitHub 自己的内容哈希（不是 SHA-256，
/// 本地算不出来），所以这里比的是「两次请求说的是不是同一份内容」。
/// 官方源直连时 HTTPS 本身已经保证了内容真实性，这一步主要是防镜像返回错东西。
pub fn download(
    client: &reqwest::blocking::Client,
    repo_path: &str,
    dest: &Path,
    url: &str,
    expect_etag: Option<&str>,
    cancel: &AtomicBool,
    // 正在用的是哪个源（空串 = 官方源），跟着进度一起报给界面
    source: &str,
    // 低于这个速率（KB/s）就中止并换源。0 = 不看速率（兜底那一遍用）
    min_kbps: u64,
    progress: &mut dyn FnMut(u64, u64, &str),
) -> Result<Downloaded> {
    let tmp = part_path(dest);
    let _ = std::fs::remove_file(&tmp);

    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| anyhow::anyhow!(friendly_error(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("下载 {repo_path} 返回 HTTP {}", status.as_u16());
    }
    let resp_etag = etag_of(&resp);
    let total = resp.content_length().filter(|n| *n > 0).unwrap_or(0);

    let mut buf: Vec<u8> = Vec::with_capacity(total as usize);
    let mut chunk = vec![0u8; 64 * 1024];
    let mut got: u64 = 0;
    // 看门狗：前 3 秒不判（TLS 握手 + 慢启动），之后一旦实测速率低于阈值就
    // 立刻放弃这个源。这是「慢」和「坏」的分界 —— 坏源有连接超时兜着，
    // 慢源以前没有任何机制，用户只能眼睁睁看 30 MB 一点点爬完。
    let t0 = Instant::now();
    let watch = min_kbps > 0 && (total == 0 || total >= WATCHDOG_MIN_BYTES);
    // 进度回调里那第三段文字在这里算一次 —— 别每 64 KB 都新分配一个 String
    let tag = format!("经 {}", source_label(source));
    let tag_slow = format!("经 {} 速度不达标，换下一个", source_label(source));
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
        buf.extend_from_slice(&chunk[..n]);
        got += n as u64;
        progress(got, total, &tag);
        if watch {
            let el = t0.elapsed().as_secs_f64();
            if el >= WATCHDOG_GRACE_SECS {
                let kbps = got as f64 / el / 1024.0;
                if kbps < min_kbps as f64 {
                    let _ = std::fs::remove_file(&tmp);
                    // 让界面说清这次是「太慢」而不是「坏了」：
                    // 用户看到的是「速度不达标，换下一个」，比字节数卡着不动好懂得多。
                    progress(got, total, &tag_slow);
                    bail!("{TOO_SLOW_PREFIX}（实测 {kbps:.0} KB/s，低于 {min_kbps} KB/s）");
                }
            }
        }
    }

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
    if total > 0 && buf.len() as u64 != total {
        let _ = std::fs::remove_file(&tmp);
        bail!(
            "下载不完整：{repo_path} 期望 {total} 字节，实际只收到 {} 字节，已丢弃",
            buf.len()
        );
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, &buf)?;
    util::atomic_replace(&tmp, dest)?;
    util::clear_motw(dest);
    record_download_speed(source, buf.len() as u64, t0.elapsed().as_secs_f64());
    Ok(Downloaded {
        bytes: buf.len() as u64,
        sha256: util::sha256_hex(&buf),
        blob_sha: util::git_blob_sha1(&buf),
        etag: resp_etag,
    })
}

/// 下载成功后把实测速率记下来，下次排序就有依据了。
/// 太小的样本不记 —— 581 字节的 ini 算出来的数没有意义。
fn record_download_speed(source: &str, bytes: u64, secs: f64) {
    if bytes < SPEED_RECORD_MIN_BYTES || secs < 0.3 {
        return;
    }
    record_speed(source, (bytes as f64 / secs / 1024.0) as u64);
}

fn state_path() -> Result<PathBuf> {
    Ok(util::app_data_dir()?.join("update_state.json"))
}

pub fn load_state() -> UpdateState {
    state_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_state(state: &UpdateState) -> Result<()> {
    std::fs::write(state_path()?, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

pub fn asset_path(name: &str) -> Result<PathBuf> {
    Ok(util::assets_dir()?.join(name))
}

// ---------------------------------------------------------------- 按显卡改写 INI

/// 读 INI 里某个键的值（忽略注释行）
fn ini_get(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with(';') || t.starts_with('#') {
            continue;
        }
        if let Some(rest) = t.strip_prefix(key) {
            if let Some(v) = rest.strip_prefix('=') {
                return Some(v.trim().to_owned());
            }
        }
    }
    None
}

/// 改写 INI 里某个键的值，保留其它内容和注释。找不到该键返回 None。
fn ini_set(text: &str, key: &str, value: &str) -> Option<String> {
    let mut found = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if !t.starts_with(';') && !t.starts_with('#') {
            if let Some(rest) = t.strip_prefix(key) {
                if rest.strip_prefix('=').is_some() {
                    out.push_str(&format!("{}={}", key, value));
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

/// 部署用的 INI 计划
pub struct IniPlan {
    /// 实际要部署的文件（可能是改写过的副本）
    pub path: PathBuf,
    /// 人类可读的修改说明；空表示和上游原文件一致
    pub changes: Vec<String>,
}

/// 按显卡路由准备要部署的 INI。
/// RTX 30 系保持上游默认；RTX 20 / GTX 16 系必须把 Router 改成 SM75，否则完全无效。
/// 改写后写到单独的文件，上游原文件保持不动（用于比对哈希）。
pub fn prepare_deploy_ini(route: GpuRoute, gpu_name: Option<&str>) -> Result<IniPlan> {
    let upstream = util::assets_dir()?.join(INI_REPO_PATH);
    if !upstream.is_file() {
        bail!("还没有下载 {}, 请先点「下载 / 更新资产」", INI_REPO_PATH);
    }
    let text = std::fs::read_to_string(&upstream).context("读取 INI 失败")?;

    let mut changes = Vec::new();
    let out_text = if route == GpuRoute::Sm75 {
        let before = ini_get(&text, "Router").unwrap_or_default();
        let patched = ini_set(&text, "Router", "SM75").context("INI 里找不到 Router 项，无法改写")?;
        if before != "SM75" {
            changes.push(format!(
                "Router：上游默认 {} → 改为 SM75。原因：本机显卡是 {}，属于 Turing / SM75 架构，走 SM86 路由不会生效。",
                if before.is_empty() { "未读到".to_owned() } else { before },
                gpu_name.unwrap_or("RTX 20 / GTX 16 系")
            ));
        }
        patched
    } else {
        text
    };

    let dest = util::assets_dir()?.join("dlssg_sm86.deploy.ini");
    std::fs::write(&dest, &out_text).context("写入部署用 INI 失败")?;
    Ok(IniPlan {
        path: dest,
        changes,
    })
}

// ---------------------------------------------------------------- 第三方 DLSS 运行库
//
// 为什么需要：很多游戏目录里没有 nvngx_dlssg.dll / nvngx_dlss.dll，这个 Mod 就不生效。
// 这两个文件由 NVIDIA 官方签名，这里从社区仓库的 release 里取（该仓库只做搬运打包，
// 我们解压后会校验签名者必须是 NVIDIA，否则丢弃）。

/// 内置镜像，按**实测速度**从快到慢排。
///
/// 2026-09 本机实测（拉 raw 上的 version.dll，每次 8 MB 样本，跑两轮）：
///   gh-proxy.com   3.9 ~ 5.2 MB/s
///   ghfast.top     0.5 ~ 0.9 MB/s
///   ghproxy.net    0.02 ~ 0.16 MB/s   <- 原来内置的是它，慢到基本不可用
///   raw 官方直连   0.00 ~ 0.06 MB/s   <- 基本不通
/// 换成 gh-proxy.com 之后，48 MB 资产从十几分钟降到十几秒。
pub const MIRRORS: [&str; 3] = [
    "https://gh-proxy.com/",
    "https://ghfast.top/",
    "https://ghproxy.net/",
];

/// 默认备用源（= 最快的那个镜像）。界面上「当前备用源」显示的就是它。
pub const DEFAULT_BACKUP_PREFIX: &str = "https://gh-proxy.com/";

/// 实际要试的镜像列表，按**实测速率**从快到慢排。
///
/// 用户选中的源（界面上的测速列表，或手填的前缀）排在最前面，其余内置镜像跟在
/// 后面兜底 —— 选中的源整个挂掉时不至于直接失败。
fn mirrors(custom: &str) -> Vec<String> {
    let c = custom.trim();
    let rest: Vec<String> = MIRRORS
        .iter()
        .filter(|m| **m != c)
        .map(|s| (*s).to_owned())
        .collect();
    let rest = rank_mirrors(&rest);
    if c.is_empty() {
        rest
    } else {
        let mut out = vec![c.to_owned()];
        out.extend(rest);
        out
    }
}

// ---------------------------------------------------------- 选源：实测速率记忆
//
// 为什么要这套东西：镜像的快慢**按用户线路和时间剧烈变化**。实测同一个
// gh-proxy.com，同一台机器，相隔一小时能从 6.9 MB/s 掉到 0.34 MB/s。
// 只记「上次哪个源成功了」根本察觉不到这种变化，用户就得陪着慢源一起等。

/// 一个源的实测速率记录。
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

/// 低于这个速率（KB/s）就认为这个源慢得没法用：既用来触发换源，也用来排序。
pub const DEFAULT_MIN_SPEED_KBPS: u64 = 300;

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

/// 记下一次实测速率。prefix 空串表示官方源。
pub fn record_speed(prefix: &str, kbps: u64) {
    let mut s = load_speeds();
    s.entries.insert(prefix.to_owned(), SpeedSample { kbps, at: now_unix() });
    save_speeds(&s);
}

/// 取某个源的实测速率。没测过、或记录太旧，都返回 None。
fn speed_of(s: &SourceSpeeds, prefix: &str) -> Option<u64> {
    let e = s.entries.get(prefix)?;
    if now_unix() - e.at > SPEED_TTL_SECS {
        return None;
    }
    Some(e.kbps)
}

/// 按实测速率排序：确认够快的在前（越快越前），没测过的居中，确认太慢的垫底。
pub fn rank_mirrors(ms: &[String]) -> Vec<String> {
    let s = load_speeds();
    let items: Vec<(String, Option<u64>)> =
        ms.iter().map(|m| (m.clone(), speed_of(&s, m))).collect();
    rank_by_scores(&items)
}

/// 纯粹按 (源, 实测速率) 排序，和磁盘状态无关，方便自测。
/// None = 没测过。
pub fn rank_by_scores(items: &[(String, Option<u64>)]) -> Vec<String> {
    let (mut good, mut unknown, mut slow) = (Vec::new(), Vec::new(), Vec::new());
    for (m, score) in items {
        match score {
            Some(k) if *k >= DEFAULT_MIN_SPEED_KBPS => good.push((*k, m.clone())),
            Some(k) => slow.push((*k, m.clone())),
            None => unknown.push(m.clone()),
        }
    }
    good.sort_by(|a, b| b.0.cmp(&a.0));
    slow.sort_by(|a, b| b.0.cmp(&a.0));
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

// ------------------------------------------------------------------ 看门狗

/// 换源文案。上层靠这个前缀区分「这个源太慢」和「这个源坏了」，
/// 因为「所有源都太慢」时要放宽速度要求再试一遍，不能让用户下不了。
pub const TOO_SLOW_PREFIX: &str = "这个源太慢";

/// 只对大文件开看门狗。581 字节的 ini 秒下完，判速没意义。
const WATCHDOG_MIN_BYTES: u64 = 4 * 1024 * 1024;

/// 宽限期：TLS 握手 + TCP 慢启动都要时间，太早判会误杀好源。
const WATCHDOG_GRACE_SECS: f64 = 3.0;

/// 下载中至少攒够这么多字节才值得记速率（小文件测出来的数没意义）。
const SPEED_RECORD_MIN_BYTES: u64 = 512 * 1024;

/// 运行库来源仓库
pub const DLSS_REPO: &str = "RankFTW/rhi-repo";

/// (release tag 前缀, 期望 tag, 压缩包文件名, 解出来的文件名, 界面显示名)
///
/// 压缩包名是实测从仓库 releases 里查出来写死的。有了它就能直接拼直链下载，
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

/// 下载任意 URL 到文件（第三方 release 资产没有 git blob sha 可对，所以单独一个函数）。
pub fn download_raw(
    client: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
    cancel: &AtomicBool,
    // 同 download()：哪个源、速率低于多少就换源
    source: &str,
    min_kbps: u64,
    progress: &mut dyn FnMut(u64, u64, &str),
) -> Result<u64> {
    let tmp = part_path(dest);
    let _ = std::fs::remove_file(&tmp);

    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| anyhow::anyhow!(friendly_error(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("下载返回 HTTP {}", status.as_u16());
    }
    let total = resp.content_length().filter(|n| *n > 0).unwrap_or(0);

    let mut buf: Vec<u8> = Vec::with_capacity(total as usize);
    let mut chunk = vec![0u8; 64 * 1024];
    let mut got: u64 = 0;
    let t0 = Instant::now();
    let watch = min_kbps > 0 && (total == 0 || total >= WATCHDOG_MIN_BYTES);
    // 进度回调里那第三段文字在这里算一次 —— 别每 64 KB 都新分配一个 String
    let tag = format!("经 {}", source_label(source));
    let tag_slow = format!("经 {} 速度不达标，换下一个", source_label(source));
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
        buf.extend_from_slice(&chunk[..n]);
        got += n as u64;
        progress(got, total, &tag);
        if watch {
            let el = t0.elapsed().as_secs_f64();
            if el >= WATCHDOG_GRACE_SECS {
                let kbps = got as f64 / el / 1024.0;
                if kbps < min_kbps as f64 {
                    let _ = std::fs::remove_file(&tmp);
                    // 让界面说清这次是「太慢」而不是「坏了」：
                    // 用户看到的是「速度不达标，换下一个」，比字节数卡着不动好懂得多。
                    progress(got, total, &tag_slow);
                    bail!("{TOO_SLOW_PREFIX}（实测 {kbps:.0} KB/s，低于 {min_kbps} KB/s）");
                }
            }
        }
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, &buf)?;
    util::atomic_replace(&tmp, dest)?;
    util::clear_motw(dest);
    record_download_speed(source, got, t0.elapsed().as_secs_f64());
    Ok(buf.len() as u64)
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
    min_kbps: u64,
    progress: &mut dyn FnMut(u64, u64, &str),
) -> Result<u64> {
    let ms = mirrors("");
    let too_slow = AtomicBool::new(false);

    let run = |min_kbps: u64,
                   too_slow: &AtomicBool,
                   progress: &mut dyn FnMut(u64, u64, &str)|
     -> Result<u64> {
        try_sources(official, &ms, true, Some(cancel), |url, src| {
            progress(0, 0, src);
            match download_raw(client, url, dest, cancel, src, min_kbps, progress) {
                Ok(n) => Ok(n),
                Err(e) => {
                    if e.to_string().starts_with(TOO_SLOW_PREFIX) {
                        too_slow.store(true, Ordering::Relaxed);
                    }
                    Err(e)
                }
            }
        })
    };

    match run(min_kbps, &too_slow, progress) {
        Ok(v) => Ok(v),
        Err(e) => {
            // 同 download_auto：所有源都太慢时就放宽速度要求再走一遍
            if min_kbps > 0 && too_slow.load(Ordering::Relaxed) {
                progress(0, 0, "几个源都不够快，用最快的那个继续");
                run(0, &too_slow, progress)
                    .map_err(|e2| anyhow::anyhow!("{e2}（放宽速度要求后重试仍失败；先前：{e}）"))
            } else {
                Err(e)
            }
        }
    }
}

// ------------------------------------------------------------------ 测速

/// 一个源的测速结果。
#[derive(Debug, Clone)]
pub struct SourceSpeed {
    /// 镜像前缀。空串 = 官方源（这一行只做展示，不可选）。
    pub prefix: String,
    /// 给人看的名字
    pub label: String,
    /// 实测 KB/s。error 非空时无意义。
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

/// 从 zip 里解出第一个 .dll 到 out_path，返回该条目名。
pub fn zip_extract_dll(zip_path: &Path, out_path: &Path) -> Result<String> {
    let data = std::fs::read(zip_path).with_context(|| format!("读取 {}", zip_path.display()))?;
    if data.len() < 22 {
        bail!("zip 文件太小，不像是有效的压缩包");
    }

    // 1. 从尾部往前找 EOCD (PK\x05\x06)
    let min = data.len().saturating_sub(22 + 65535);
    let mut i = data.len() - 22;
    let mut eocd = None;
    loop {
        if &data[i..i + 4] == b"PK\x05\x06" {
            eocd = Some(i);
            break;
        }
        if i <= min {
            break;
        }
        i -= 1;
    }
    let eocd = eocd.context("找不到 zip 中央目录结尾，可能不是有效的 zip")?;

    let count = rd_u16le(&data, eocd + 10) as usize;
    let cd_off = rd_u32le(&data, eocd + 16) as usize;
    if cd_off >= data.len() {
        bail!("zip 中央目录偏移越界");
    }

    // 2. 遍历中央目录，选第一个 .dll
    let mut p = cd_off;
    let mut chosen: Option<(String, u16, u64, usize)> = None;
    for _ in 0..count {
        if p + 46 > data.len() || &data[p..p + 4] != b"PK\x01\x02" {
            break;
        }
        let method = rd_u16le(&data, p + 10);
        let comp_size = rd_u32le(&data, p + 20) as u64;
        let name_len = rd_u16le(&data, p + 28) as usize;
        let extra_len = rd_u16le(&data, p + 30) as usize;
        let comment_len = rd_u16le(&data, p + 32) as usize;
        let local_off = rd_u32le(&data, p + 42) as usize;
        let name = data
            .get(p + 46..p + 46 + name_len)
            .map(|b| String::from_utf8_lossy(b).to_string())
            .unwrap_or_default();
        if chosen.is_none() && name.to_ascii_lowercase().ends_with(".dll") {
            chosen = Some((name, method, comp_size, local_off));
        }
        p += 46 + name_len + extra_len + comment_len;
    }
    let (name, method, comp_size, local_off) = chosen.context("zip 里没有 .dll 文件")?;

    // 3. 读本地头算数据起点（本地头的名字/扩展区长度未必和中央目录一致）
    if local_off + 30 > data.len() || &data[local_off..local_off + 4] != b"PK\x03\x04" {
        bail!("zip 本地头损坏");
    }
    let l_name_len = rd_u16le(&data, local_off + 26) as usize;
    let l_extra_len = rd_u16le(&data, local_off + 28) as usize;
    let data_off = local_off + 30 + l_name_len + l_extra_len;
    let end = data_off + comp_size as usize;
    if end > data.len() {
        bail!("zip 数据区越界");
    }
    let raw = &data[data_off..end];

    // 4. 解压
    let out: Vec<u8> = match method {
        0 => raw.to_vec(),
        8 => {
            let mut v = Vec::new();
            flate2::read::DeflateDecoder::new(raw)
                .read_to_end(&mut v)
                .context("Deflate 解压失败")?;
            v
        }
        m => bail!("不支持的 zip 压缩方式 {m}"),
    };
    if out.is_empty() {
        bail!("解压结果是空文件");
    }

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = part_path(out_path);
    std::fs::write(&tmp, &out)?;
    util::atomic_replace(&tmp, out_path)?;
    util::clear_motw(out_path);
    Ok(name)
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
    min_kbps: u64,
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

        let got_bytes: u64;
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
            download_with_mirror(client, &step.url, &zip_path, cancel, min_kbps, &mut relay)
        };

        match direct {
            Ok(n) => got_bytes = n,
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
                got_bytes = download_with_mirror(
                    client, &asset.url, &zip_path, cancel, min_kbps, &mut relay,
                )?;
            }
        }
        done += got_bytes;

        progress(
            format!("第 {step_no}/{} 步 · 解压 {label} ...", ctx.total_steps),
            frac(done),
        );
        let extracted = zip_extract_dll(&zip_path, &dest)?;
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

