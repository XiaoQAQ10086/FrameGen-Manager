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
use std::time::Duration;

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
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .context("创建 HTTP 客户端失败")
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
    let mirror = format!("{DEFAULT_BACKUP_PREFIX}{official}");
    let mut last = String::from("没有可用的源");

    for (label, url) in [("官方源", official), ("备用源", mirror)] {
        match client.head(&url).send() {
            Ok(resp) if resp.status().is_success() => {
                let size = content_length_of(&resp);
                match etag_of(&resp) {
                    Some(etag) => {
                        return Ok(RemoteFile {
                            name: repo_path.to_owned(),
                            size,
                            etag,
                        })
                    }
                    None => last = format!("{label} 没返回可用的 ETag"),
                }
            }
            Ok(resp) => last = format!("{label} 返回 HTTP {}", resp.status().as_u16()),
            Err(e) => last = format!("{label} {}", friendly_error(&e)),
        }
    }
    bail!("拿不到 {repo_path} 的内容指纹（{last}）")
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
    let url = format!("https://raw.githubusercontent.com/{REPO}/{BRANCH}/README.md");
    if let Ok(resp) = client.get(&url).send() {
        if let Ok(text) = resp.text() {
            if let Some(v) = extract_version(&text) {
                return Some(v);
            }
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
    mut progress: impl FnMut(u64, u64),
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
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&tmp);
            bail!("已取消下载");
        }
        let n = resp
            .read(&mut chunk)
            .map_err(|e| anyhow::anyhow!("下载中断：{}", e))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        got += n as u64;
        progress(got, total);
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
    Ok(Downloaded {
        bytes: buf.len() as u64,
        sha256: util::sha256_hex(&buf),
        blob_sha: util::git_blob_sha1(&buf),
        etag: resp_etag,
    })
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

/// 内置的备用下载源。官方 raw.githubusercontent.com 连不上时用这个前缀拼接。
/// 这个地址是实测可用的（拉下来的 ini 内容与官方完全一致）。
pub const DEFAULT_BACKUP_PREFIX: &str = "https://ghproxy.net/";

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
    progress: &mut dyn FnMut(u64, u64),
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
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&tmp);
            bail!("已取消下载");
        }
        let n = resp
            .read(&mut chunk)
            .map_err(|e| anyhow::anyhow!("下载中断：{}", e))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        progress(buf.len() as u64, total);
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, &buf)?;
    util::atomic_replace(&tmp, dest)?;
    util::clear_motw(dest);
    Ok(buf.len() as u64)
}

/// HEAD 任意 URL，拿 (大小, ETag)。发布资产也有 Content-Length，够界面显示用了。
/// 同样先官方后镜像，失败返回 None（只是显示不出大小，不影响下载）。
pub fn probe_url(client: &reqwest::blocking::Client, url: &str) -> Option<(u64, String)> {
    for u in [url.to_owned(), format!("{}{}", DEFAULT_BACKUP_PREFIX, url)] {
        if let Ok(r) = client.head(&u).send() {
            if r.status().is_success() {
                return Some((content_length_of(&r), etag_of(&r).unwrap_or_default()));
            }
        }
    }
    None
}

/// 先试官方地址，失败再试镜像前缀。
/// 两边都失败时把两个错误一起报出来，方便看出到底卡在哪一环。
fn download_with_mirror(
    client: &reqwest::blocking::Client,
    official: &str,
    dest: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<u64> {
    match download_raw(client, official, dest, cancel, progress) {
        Ok(n) => Ok(n),
        Err(e1) => {
            let mirror = format!("{}{}", DEFAULT_BACKUP_PREFIX, official);
            progress(0, 0);
            download_raw(client, &mirror, dest, cancel, progress)
                .map_err(|e2| anyhow::anyhow!("官方源失败（{e1}）；备用源也失败（{e2}）"))
        }
    }
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

/// 确保两个 DLSS 运行库都在本地：缺就下载 -> 解压 -> 删掉压缩包 -> 校验 NVIDIA 签名。
/// 返回两个 DLL 的本地路径。
pub fn ensure_dlss_runtime(
    client: &reqwest::blocking::Client,
    cancel: &AtomicBool,
    mut progress: impl FnMut(String, f32),
) -> Result<Vec<PathBuf>> {
    let dir = util::assets_dir()?;
    let mut out = Vec::new();

    for (prefix, tag, zip_name, dll_name, label) in DLSS_RUNTIME {
        let dest = dir.join(dll_name);

        if dest.is_file() && scan::identify_dll(&dest) == scan::FileIdentity::Nvidia {
            progress(format!("{label} 已就绪，跳过下载"), 1.0);
            out.push(dest);
            continue;
        }

        // 直链下载：这一步不消耗任何 GitHub API 配额
        let official = release_url(tag, zip_name);
        let zip_path = dir.join(zip_name);
        progress(format!("下载 {label}..."), 0.0);

        let direct = {
            let mut relay = |got: u64, total: u64| {
                let f = if total > 0 { got as f32 / total as f32 } else { 0.0 };
                progress(
                    format!(
                        "下载 {label} {} / {}",
                        util::format_bytes(got),
                        util::format_bytes(total)
                    ),
                    f,
                );
            };
            download_with_mirror(client, &official, &zip_path, cancel, &mut relay)
        };

        if let Err(e) = direct {
            // 直链彻底失败才回退去问 releases API：作者删包 / 改名时会走到这里
            let asset = find_release_zip(client, DLSS_REPO, prefix, tag).map_err(|e2| {
                anyhow::anyhow!("直链下载失败（{e}）；改用 Releases 接口也没成功：{e2}")
            })?;
            progress(format!("{label} 改用 Releases 接口重试"), 0.0);
            let mut relay = |got: u64, total: u64| {
                let t = if total > 0 { total } else { asset.size };
                let f = if t > 0 { got as f32 / t as f32 } else { 0.0 };
                progress(
                    format!(
                        "下载 {label} {} / {}",
                        util::format_bytes(got),
                        util::format_bytes(t)
                    ),
                    f,
                );
            };
            download_with_mirror(client, &asset.url, &zip_path, cancel, &mut relay)?;
        }

        progress(format!("解压 {label} ..."), 1.0);
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
                "{label} 就绪（已校验 NVIDIA 签名，{}）",
                util::format_bytes(size)
            ),
            1.0,
        );
        out.push(dest);
    }
    Ok(out)
}

