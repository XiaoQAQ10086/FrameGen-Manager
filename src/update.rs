//! 更新模块。
//!
//! 重要：上游 sdli1995/dlssg_for_sm86 **没有 GitHub Releases，也没有 Tags**。
//! 文件是直接提交在 main 分支根目录的（version.dll / dlssg_sm86.ini），
//! 所以「检查 Releases」这条路根本不存在，这里改用 contents API 的 git blob sha 作为变更指纹。

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteFile {
    pub name: String,
    pub blob_sha: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalFile {
    pub blob_sha: String,
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
    /// 远端 sha 和本地记录不同 -> 有更新。
    ///
    /// 注意要传**本地文件名**（比如 winmm.dll），不是仓库路径
    /// （altnative/winmm.dll）—— files 表是按本地文件名做 key 的。
    /// 早先用 remote.name 查，导致 altnative 那四个永远被判定成「有更新」。
    pub fn needs_update(&self, local_name: &str, remote_blob_sha: &str) -> bool {
        self.files
            .get(local_name)
            .map(|l| l.blob_sha != remote_blob_sha)
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

pub fn fetch_remote(client: &reqwest::blocking::Client, name: &str) -> Result<RemoteFile> {
    let url = format!("https://api.github.com/repos/{REPO}/contents/{name}?ref={BRANCH}");
    let resp = client.get(&url).send().context("请求 GitHub API 失败")?;
    let status = resp.status();
    if status.as_u16() == 403 || status.as_u16() == 429 {
        bail!(RATE_LIMIT_MSG);
    }
    if !status.is_success() {
        bail!("GitHub API 返回 {}", status);
    }
    let text = resp.text().context("读取响应失败")?;
    let v: serde_json::Value = serde_json::from_str(&text).context("解析 JSON 失败")?;
    let blob_sha = v
        .get("sha")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_owned();
    let size = v.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
    if blob_sha.is_empty() {
        bail!("GitHub 响应里没有 sha 字段");
    }
    Ok(RemoteFile {
        name: name.to_owned(),
        blob_sha,
        size,
    })
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

/// 下载并校验。
///
/// - url 由调用方决定（官方源或备用源）
/// - cancel 置位时立刻中断，且磁盘上不留任何残留
///
/// 校验方式：用下载到的字节算出 git blob sha1，必须等于 API 报的 sha。
pub fn download(
    client: &reqwest::blocking::Client,
    remote: &RemoteFile,
    dest: &Path,
    url: &str,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<()> {
    let tmp = part_path(dest);
    let _ = std::fs::remove_file(&tmp);

    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| anyhow::anyhow!(friendly_error(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("下载 {} 返回 HTTP {}", remote.name, status.as_u16());
    }
    let total = resp
        .content_length()
        .filter(|n| *n > 0)
        .unwrap_or(remote.size);

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

    let actual = util::git_blob_sha1(&buf);
    if actual != remote.blob_sha {
        let _ = std::fs::remove_file(&tmp);
        bail!(
            "完整性校验失败：{} 的内容与仓库记录不一致，已丢弃（镜像可能返回了错误内容）",
            remote.name
        );
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, &buf)?;
    util::atomic_replace(&tmp, dest)?;
    util::clear_motw(dest);
    Ok(())
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

/// (release tag 前缀, 期望 tag, 解出来的文件名, 界面显示名)
pub const DLSS_RUNTIME: [(&str, &str, &str, &str); 2] = [
    ("dlssg-", "dlssg-310.9.1", "nvngx_dlssg.dll", "DLSS 帧生成运行库"),
    ("dlss-", "dlss-310.9.1", "nvngx_dlss.dll", "DLSS 超分运行库"),
];

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

/// 列一个目录下的所有文件。
///
/// 一次调用就能拿到该目录下全部文件的 blob sha —— 比逐文件查询省得多。
/// GitHub 未认证 API 每小时只有 60 次配额，逐文件查六个文件就吃掉 6 次。
pub fn fetch_dir_listing(
    client: &reqwest::blocking::Client,
    dir_path: &str,
) -> Result<Vec<RemoteFile>> {
    let url = if dir_path.is_empty() {
        format!("https://api.github.com/repos/{REPO}/contents?ref={BRANCH}")
    } else {
        format!("https://api.github.com/repos/{REPO}/contents/{dir_path}?ref={BRANCH}")
    };
    let resp = client.get(&url).send().context("请求 GitHub API 失败")?;
    let status = resp.status();
    if status.as_u16() == 403 || status.as_u16() == 429 {
        bail!(RATE_LIMIT_MSG);
    }
    if !status.is_success() {
        bail!("GitHub API 返回 {}", status.as_u16());
    }
    let text = resp.text().context("读取响应失败")?;
    Ok(parse_dir_listing(&text))
}

/// 解析 contents API 的目录列举响应。抽出来是为了能离线测试。
pub fn parse_dir_listing(text: &str) -> Vec<RemoteFile> {
    let mut out = Vec::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return out;
    };
    let Some(arr) = v.as_array() else {
        return out;
    };
    for e in arr {
        // 目录项和符号链接跳过
        if e.get("type").and_then(|x| x.as_str()) != Some("file") {
            continue;
        }
        let name = e.get("name").and_then(|x| x.as_str()).unwrap_or_default();
        let path = e.get("path").and_then(|x| x.as_str()).unwrap_or(name);
        let blob_sha = e.get("sha").and_then(|x| x.as_str()).unwrap_or_default();
        let size = e.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
        if name.is_empty() || blob_sha.is_empty() {
            continue;
        }
        out.push(RemoteFile {
            name: path.to_owned(),
            blob_sha: blob_sha.to_owned(),
            size,
        });
    }
    out
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
    mut progress: impl FnMut(u64, u64),
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

    for (prefix, tag, dll_name, label) in DLSS_RUNTIME {
        let dest = dir.join(dll_name);

        if dest.is_file() && scan::identify_dll(&dest) == scan::FileIdentity::Nvidia {
            progress(format!("{label} 已就绪，跳过下载"), 1.0);
            out.push(dest);
            continue;
        }

        let asset = find_release_zip(client, DLSS_REPO, prefix, tag)?;
        let zip_path = dir.join(&asset.asset_name);
        progress(
            format!("下载 {label}（{}）", util::format_bytes(asset.size)),
            0.0,
        );

        download_raw(client, &asset.url, &zip_path, cancel, |got, total| {
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
        })?;

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

