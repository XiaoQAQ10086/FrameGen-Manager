//! 部署 / 备份 / 还原。
//!
//! 目录布局：
//!   %APPDATA%\DLSSG-Manager\backups\<game_key>\manifest.json
//!   %APPDATA%\DLSSG-Manager\backups\<game_key>\files\<name>.bak

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::backup_restore::{self, BackupPlan, BackupRef};
use crate::util;

// 代理入口名单**只在 scan.rs 里硬编码一份**：`scan::PROXY_ALL` = 现用的 6 个
// （alternatives/ 那一套）+ 老归档包才有的 winhttp.dll。这里换成部署侧习惯的名字引用它，
// 免得两处各写一遍、以后上游改名时漏改一处。
//
// 为什么两个版本的名字都要认：用户从老版切到新版后，目录里残留的 winhttp.dll
// 会被当成第三方文件而拒绝处理（同一时刻只应存在一个代理入口）。
use crate::scan::PROXY_ALL as PROXY_ENTRIES;

pub const INI_NAME: &str = "dlssg_sm86.ini";
/// 插件**运行时**在旁边建的目录（INI 里 [Logging] Directory=dlssg_sm86\logs；
/// 相对路径的 bundle 缓存也可能落在这一层）。它不是我们部署的文件，所以清单里没有 ——
/// 但不清掉的话，用户点完「还原」目录里还杵着它（用户明确要求一并删除）。
pub const RUNTIME_DIR: &str = "dlssg_sm86";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupEntry {
    pub rel_path: String,
    pub existed_before: bool,
    pub backup_name: Option<String>,
    pub original_sha256: Option<String>,
    pub deployed_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub game_key: String,
    pub game_dir: PathBuf,
    pub deployed_at: String,
    pub proxy_name: String,
    pub files: Vec<BackupEntry>,
    /// 「正在安装」标记：清单**先落盘**（true），文件全部写完并校验通过后再落成 false。
    ///
    /// 为什么要有它：进程被硬终止（断电 / 任务管理器结束）时不会走进任何回滚分支，
    /// 光看清单会以为"已部署"，而文件其实只写了一半。有标记就能如实告诉用户"上次没装完"。
    /// 老清单里没有这个字段 → serde default 当 false（即"装完了"），行为与从前一致。
    #[serde(default)]
    pub installing: bool,
}

/// 旧记录里的代理入口这次不用了，该怎么处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanAction {
    /// 不碰
    Keep,
    /// 我们新放的：直接从目录里删掉
    Remove,
    /// 那个位置**原本就有文件**（用户手动装的、或旧版本装的同项目文件）：
    /// 也要从目录里删掉（否则两个代理并存），但**备份记录必须留下来**，
    /// 否则「还原」再也放不回原件，就变成还原也管不到的孤儿。
    RemoveButKeepRecord,
}

/// 决定一条旧记录该怎么处置。
///
/// 抽成纯函数是为了能单独测：真实的代理 DLL 带本项目签名，自测里造不出来。
///
/// 判定故意保守 —— 只有「确实是我们当初写进去的那一份」才动：
///   * 不是代理入口 -> 不碰（ini 每次都在清单里，走不到这）
///   * 这次还要用 -> 不碰
///   * 文件已经不在了 -> 不碰
///   * 内容不是我们写的那份（用户换过）-> 不碰
pub fn plan_orphan(
    is_ours_to_sweep: bool,
    in_payload: bool,
    file_present: bool,
    still_ours: bool,
    existed_before: bool,
) -> OrphanAction {
    // is_ours_to_sweep = 代理入口，或"我们曾经铺在游戏根目录、现在不再需要"的运行库
    if !is_ours_to_sweep || in_payload || !file_present || !still_ours {
        return OrphanAction::Keep;
    }
    if existed_before {
        OrphanAction::RemoveButKeepRecord
    } else {
        OrphanAction::Remove
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployState {
    NotDeployed,
    /// 本工具部署的（有 manifest，可以一键还原）
    Deployed {
        proxy: String,
        deployed_at: String,
        /// 这次部署是 0.3.x 的**旧布局**（运行库还铺在游戏根目录、没有 dlssg_sm86\ 目录）。
        ///
        /// 为什么要在界面上说出来：老用户升级到新版后，游戏目录里**还是旧布局** ——
        /// 帧生成照常能用，但拿不到 0.4.0 的新运行库（310.9.16）和新内核。
        /// 光看"已部署"三个字，用户根本不知道要再点一次「部署」才会换成新的。
        legacy_layout: bool,
        /// 代理文件已经被覆盖或丢失（实测最常见的：用户后来又去开了 Smooth Motion，
        /// 解锁组件也叫 version.dll，把帧生成的代理顶掉了）。
        /// **记录还在、但帧生成已经失效** —— 不吭声的话界面会一直显示"已部署"。
        proxy_replaced: bool,
        /// 上一轮写入没走完（进程被硬终止）。见 BackupManifest.installing。
        installing: bool,
    },
    /// 目录里有本项目的文件，但不是本工具部署的（用户之前手动装的）
    ManuallyInstalled {
        files: Vec<String>,
    },
    /// 目录里有代理 DLL，但不是本项目的 —— 可能是别的 Mod
    Occupied {
        files: Vec<String>,
    },
    /// Smooth Motion 的解锁组件占着 version.dll（和帧生成同一个文件名，必须分开认）
    SmDeployed,
}

impl DeployState {
    pub fn label(&self) -> String {
        match self {
            DeployState::NotDeployed => "未部署".to_owned(),
            DeployState::Deployed {
                proxy,
                legacy_layout,
                proxy_replaced,
                installing,
                ..
            } => {
                // 前缀必须仍是"已部署"：界面的 stateCls 与还原按钮可用性都按它判断
                if *proxy_replaced {
                    format!("已部署 ({proxy}) · 代理文件已被覆盖或丢失（帧生成暂时失效）")
                } else if *installing {
                    format!("已部署 ({proxy}) · 上次安装未完成（建议重新部署或还原）")
                } else if *legacy_layout {
                    format!("已部署 ({proxy}) · 旧版资产，建议重新部署")
                } else {
                    format!("已部署 ({proxy})")
                }
            }
            DeployState::ManuallyInstalled { files } => {
                // 「非本工具部署」容易被读成「部署失败了」，这里说清是「没有本工具的记录」
                format!("已安装，本工具无记录（{}）", files.join("、"))
            }
            // 「入口被占用」听起来像有外来模组在抢占 —— 但实际大多是**游戏自带的同名文件**
            // （玩家反馈：霍格沃茨自带 dxgi.dll、CS2 自带微软签名的 dbghelp.dll）。
            // 改成中性描述：占名是事实（部署该入口会先备份再覆盖），但不该像异常。
            DeployState::Occupied { files } => format!("已有同名单文件: {}", files.join(", ")),
            DeployState::SmDeployed => "已被 Smooth Motion 占用".to_owned(),
        }
    }
}

fn manifest_path(dir: &Path) -> Result<PathBuf> {
    Ok(util::backups_dir()?
        .join(util::dir_key(dir))
        .join("manifest.json"))
}

/// manifest 的状态。**「文件在但读不出来」必须和「压根没有这份文件」分开**：
/// 前者意味着我们可能把一个已部署目录当成从未部署过，那就会把我们自己放进去的
/// 文件记成「游戏原件」覆盖掉真正的原件 —— 后果是原件永久丢失。
pub enum ManifestState {
    Missing,
    Ok(Box<BackupManifest>),
    Broken(String),
}

/// 严格读一次 manifest：能区分「没有」和「坏了」。
pub fn manifest_state(dir: &Path) -> ManifestState {
    let Ok(p) = manifest_path(dir) else {
        return ManifestState::Missing;
    };
    if !p.is_file() {
        return ManifestState::Missing;
    }
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) => return ManifestState::Broken(e.to_string()),
    };
    match serde_json::from_str::<BackupManifest>(&text) {
        Ok(m) => ManifestState::Ok(Box::new(m)),
        Err(e) => ManifestState::Broken(e.to_string()),
    }
}

/// 写 manifest：**必须原子**（见 `util::write_json_atomic` 的说明）。
fn write_manifest(base: &Path, m: &BackupManifest) -> Result<()> {
    util::write_json_atomic(&base.join("manifest.json"), m)
}

pub fn load_manifest(dir: &Path) -> Option<BackupManifest> {
    let p = manifest_path(dir).ok()?;
    let t = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&t).ok()
}

/// 代理 DLL 的指纹（0.4.1 起上游不再带数字签名，只能靠内容认）。
///
/// **这是代理的唯一来源**：资产清单 `ASSET_INVENTORY` 只管核心文件，不再重复列代理。
/// 单测 `proxy_table_matches_bundled_assets` 会拿 `bundled-assets/` 里的真实文件核对，
/// 换资产时忘了改这里会直接测挂。
const PROXY_ASSET_SHA256: &[(&str, u64, &str)] = &[
    (
        "version.dll",
        815_240,
        "275ffa781b2d244ad269d1d65d4e5b68a12a3f89ff2baaec58d100eb5a1ff411",
    ),
    (
        "d3d12.dll",
        815_744,
        "e188a992d1ff1ef1ebb51bfc544ac132536d717f2fa2ec77642f9d506cdea58e",
    ),
    (
        "dbghelp.dll",
        832_648,
        "82c04383044657bcb4bb45b7061d8fb27a753bc222b37f6712c5affb20848f5f",
    ),
    (
        "dinput8.dll",
        814_216,
        "8181129b3754bbea4a91bcc61f50a7e28f6fb939b880e2d38eb7a5b4ffbcc8ad",
    ),
    (
        "dxgi.dll",
        815_752,
        "96e05da6572f6e4808b7f7402ccc9c584fc2257d7e5ea2478fc725663bdc27b4",
    ),
    (
        "winmm.dll",
        825_984,
        "1a584a9b0333e0c3be4ab32e00901c545ca16a620a56784a67cacf003c041739",
    ),
];

/// 已知的 ReShade（Addon 版）模块指纹：dxgi/d3d11/… 只是换了文件名，内容一致。
/// 只在"版本资源读不出来"时兜底；平时靠版本串里的 `ReShade` 字样识别（对升级免疫）。
const RESHADE_KNOWN: &[(u64, &str)] = &[(
    5_592_064,
    "0cee63f9c9f13f3ac909c5b4903f4dbb4b719a7ab3b4f13b0deaf83c814b94f7",
)];

/// 这个文件是不是 ReShade 本体（DLSS5 部署时装进去的，不属于帧生成资产）。
///
/// T2：以前写死 6.8.0 的哈希，ReShade 一升级就会把它的 dxgi.dll 判成"帧生成入口被占用"。
/// 现在优先看 PE 版本串里有没有 `ReShade`，读不到再退回已知哈希表。
fn is_reshade_module(p: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if meta.len() < 256 * 1024 {
        return false;
    }
    if meta.len() > 64 * 1024 * 1024 {
        return false;
    }
    if let Some(txt) = crate::util::file_version_text(p) {
        if txt.to_ascii_lowercase().contains("reshade") {
            return true;
        }
    }
    if RESHADE_KNOWN.iter().any(|(s, _)| *s == meta.len()) {
        return crate::util::sha256_file(p)
            .map(|h| {
                RESHADE_KNOWN
                    .iter()
                    .any(|(_, sha)| h.eq_ignore_ascii_case(sha))
            })
            .unwrap_or(false);
    }
    false
}

/// 这个文件是不是我们内置的代理。
///
/// **不再单独维护一份哈希表**（T1：两份表迟早漏改一份，上一版就因此把新代理判成"被覆盖"）。
/// 直接看 `ASSET_INVENTORY` 里"名字不含路径分隔符"的条目：那 6 个就是代理。
/// 只按内容认（不要求文件名），改名后照样认得出。
pub fn proxy_is_bundled(path: &std::path::Path) -> bool {
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return false,
    };
    let mut size_hit = false;
    for (_, s, _) in PROXY_ASSET_SHA256.iter() {
        if *s == size {
            size_hit = true;
            break;
        }
    }
    if !size_hit {
        return false;
    }
    let got = match crate::util::sha256_file(path) {
        Ok(h) => h,
        Err(_) => return false,
    };
    PROXY_ASSET_SHA256
        .iter()
        .any(|(_, _, sha)| got.eq_ignore_ascii_case(sha))
}

pub fn state_of(dir: &Path) -> DeployState {
    if let Some(m) = load_manifest(dir) {
        // 记录还在 ≠ 文件还在：Smooth Motion 的解锁组件和帧生成的代理同名（version.dll），
        // 用户后开 SM 就会把代理顶掉。这里如实看一次目录，别让界面报"已部署"骗人。
        let proxy_path = dir.join(&m.proxy_name);
        let proxy_replaced = crate::sm::has_our_asset(dir)
            || !proxy_path.is_file()
            || !crate::scan::identify_dll(&proxy_path).is_ours();
        return DeployState::Deployed {
            proxy: m.proxy_name,
            deployed_at: m.deployed_at,
            // 0.4.0 起运行库在 dlssg_sm86\ 里；这个目录不在，说明这份部署还是 0.3.x 的布局
            legacy_layout: !dir.join(RUNTIME_DIR).is_dir(),
            proxy_replaced,
            installing: m.installing,
        };
    }
    // Smooth Motion 的解锁组件也叫 version.dll —— 先认出它，
    // 否则会被当成"没部署"（允许再部署，覆盖掉它）或者"别人的 Mod"。
    if crate::sm::has_our_asset(dir) {
        return DeployState::SmDeployed;
    }
    // 没有 manifest，就靠签名判断目录里的文件是不是本项目的。
    // 这样用户自己手动安装的情况也能被识别出来，而不是一律显示「未部署」。
    let mut ours: Vec<String> = Vec::new();
    let mut others: Vec<String> = Vec::new();
    for e in PROXY_ENTRIES {
        let p = dir.join(e);
        if !p.is_file() {
            continue;
        }
        if crate::scan::identify_dll(&p).is_ours() {
            ours.push(e.to_owned());
        } else {
            others.push(e.to_owned());
        }
    }

    if !ours.is_empty() {
        // 顺便看看两个 DLSS 运行库在不在
        for n in ["nvngx_dlssg.dll", "nvngx_dlss.dll"] {
            if dir.join(n).is_file() {
                ours.push(n.to_owned());
            }
        }
        return DeployState::ManuallyInstalled { files: ours };
    }

    // ReShade（DLSS5 装的）本体也叫 dxgi.dll 等名字 —— 它不是帧生成资产，
    // 不能因此把游戏判成"入口被占用"（否则帧生成页会给出无意义的还原入口）。
    others.retain(|n| !is_reshade_module(&dir.join(n)));

    if others.is_empty() {
        DeployState::NotDeployed
    } else {
        DeployState::Occupied { files: others }
    }
}

/// 要部署的一个文件
#[derive(Debug, Clone)]
pub struct DeployFile {
    /// 部署到游戏目录里的文件名
    pub dest_name: String,
    /// 源文件路径
    pub src: PathBuf,
}

impl DeployFile {
    pub fn new(dest_name: &str, src: impl Into<PathBuf>) -> Self {
        Self {
            dest_name: dest_name.to_owned(),
            src: src.into(),
        }
    }
}

/// 目标目录里有没有「本项目的另一个代理入口」——也就是本次不打算用、又不是本工具
/// 部署过的那些。
///
/// 为什么需要它：上游 README 明确要求「每次只保留本项目的一个代理」。如果旧的那个是
/// 用户**手动**装进去的，本工具的 manifest 里没有记录，原来那套清理就没有依据可查，
/// 而那道「目录里还有别的东西」的提醒又只针对非本项目文件 —— 结果两个代理并存，
/// 用户还以为部署成功了。所以界面上要拿这个结果去问用户一句。
pub fn find_extra_own_proxies(target_dir: &Path, proxy: &str) -> Vec<String> {
    let recorded: Vec<String> = load_manifest(target_dir)
        .map(|m| m.files.into_iter().map(|e| e.rel_path).collect())
        .unwrap_or_default();
    PROXY_ENTRIES
        .iter()
        .copied()
        .filter(|e| *e != proxy)
        .filter(|e| {
            let p = target_dir.join(e);
            p.is_file()
                && crate::scan::identify_dll(&p).is_ours()
                && !recorded.iter().any(|r| r == e)
        })
        .map(str::to_owned)
        .collect()
}

/// 部署。步骤：冲突检查 -> 占用检查 -> 备份 -> 写 manifest -> 原子写入 -> 校验（失败即回滚）
///
/// proxy 是代理入口名（决定按哪个入口判冲突），files 是本次要写入的全部文件。
/// extra_proxies 是用户在弹窗里确认要移除的「本项目的另一个代理入口」。
pub fn deploy(
    target_dir: &Path,
    proxy: &str,
    files: &[DeployFile],
    extra_proxies: &[String],
) -> Result<String> {
    if !target_dir.is_dir() {
        bail!("游戏目录不存在: {}", target_dir.display());
    }
    /* 写不进去就别开始，而且把话说清楚。
    实测：目录没写权限时以前会一路走到写文件才失败，报的是
    「写入临时文件 …\.version.dll.tmp」—— 用户完全看不懂。原神默认装在
    C:\Program Files，普通权限就是写不进去，这是最常见的踩坑。 */
    if !crate::util::is_writable(target_dir) {
        bail!(
            "这个目录现在写不进去（{}）。\n\n最常见的原因：游戏装在 C:\\Program Files 这类需要管理员权限的地方。\n解决办法：右键本程序 →「以管理员身份运行」再部署；或者先把游戏完全退出。",
            target_dir.display()
        );
    }
    if !PROXY_ENTRIES.contains(&proxy) {
        bail!("不支持的代理入口: {}", proxy);
    }
    if files.is_empty() {
        bail!("没有要部署的文件");
    }
    for f in files {
        if !f.src.is_file() {
            bail!("缺少源文件: {}", f.src.display());
        }
    }

    // 记录读不出来就**不要**接着部署：那会把我们自己上次放进去的文件当成游戏原件
    // 覆盖掉真正的备份。停下来让用户处理，比悄悄毁掉原件强。
    let existing_manifest = match manifest_state(target_dir) {
        ManifestState::Missing => None,
        ManifestState::Ok(m) => Some(*m),
        ManifestState::Broken(e) => bail!(
            "这个目录的部署记录读不出来（{e}）。为免把游戏原件当成本工具的文件覆盖掉，已停止部署：请把 {} 改名或删除后再试。",
            manifest_path(target_dir)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "记录文件".to_owned())
        ),
    };

    // 1. 冲突检查
    //
    // 关键点：目标位置已有同名文件时，先看它的签名者。
    //   - "DLSSG Native Project" 签名 -> 本项目之前装的，可以安全覆盖
    //   - 其它签名者 / 无签名        -> 第三方的，拒绝覆盖（除非它不是代理入口，
    //                                   比如游戏自带的 nvngx_dlssg.dll，那个本来就要升级）
    let mut overwrite_own: Vec<String> = Vec::new();
    let mut overwrite_other: Vec<String> = Vec::new();
    // 认出来是「Smooth Motion 留下的解锁组件」的入口，单独记账
    let mut sm_asset_here: Vec<String> = Vec::new();
    for f in files {
        let dst = target_dir.join(&f.dest_name);
        if !dst.is_file() {
            continue;
        }
        /* **先判占用，再判签名**。游戏还在运行时文件常被独占，签名也读不出来 ——
        于是会误报成「签名者：无签名 → 不是本项目的文件，请换个代理入口」，
        用户照着去换入口，真实原因却是"游戏没退干净"（还要退启动器和反作弊进程）。 */
        match crate::util::lock_state(&dst) {
            crate::util::LockState::InUse => bail!(
                "{} 正被占用 —— 游戏（或它的启动器、反作弊进程）还在运行。请完全退出游戏后再部署。",
                f.dest_name
            ),
            crate::util::LockState::NoAccess => bail!(
                "{} 现在读不了也写不了：文件带了只读属性，或者权限不足。请去掉只读属性，或用「以管理员身份运行」打开本程序。",
                f.dest_name
            ),
            crate::util::LockState::Free => {}
        }
        let is_proxy = crate::scan::PROXY_PRIORITY.contains(&f.dest_name.as_str());
        let is_dll = f.dest_name.to_ascii_lowercase().ends_with(".dll");

        if is_dll {
            let id = crate::scan::identify_dll(&dst);
            if id.is_ours() {
                overwrite_own.push(f.dest_name.clone());
                continue;
            }
            /* Smooth Motion 的解锁组件也叫 version.dll，但它是社区打包的、没有我们的签名，
            按签名判会被当成「别人的文件」直接拒掉 —— 而它其实是本工具放进去的。
            按内容哈希认出来，交给下面那段单独处置。 */
            if crate::sm::is_our_asset_file(&dst) {
                sm_asset_here.push(f.dest_name.clone());
                continue;
            }
            if is_proxy {
                bail!(
                    "{} 已存在，签名者是「{}」，不是本项目的文件，拒绝覆盖。请换个代理入口，或先手动备份/移除它。",
                    f.dest_name,
                    id.label()
                );
            }
            overwrite_other.push(format!("{}（{}）", f.dest_name, id.label()));
        } else if is_proxy {
            bail!("{} 已存在且不是本工具部署的，拒绝覆盖。", f.dest_name);
        }
    }

    let mut notes: Vec<String> = Vec::new();
    /* Smooth Motion 留下（或正在用）的解锁组件：两个都用 version.dll。
    覆盖动作照做 —— 用户可以选择忽略冲突提示继续部署 —— 但要把话说清楚：
    这次覆盖了什么、Smooth Motion 会变成什么样。 */
    if !sm_asset_here.is_empty() {
        notes.push(SM_CONFLICT_NOTE.to_owned());
        notes.push(format!(
            "已覆盖 Smooth Motion 的解锁组件（原文件已备份）：{}。Smooth Motion 将不再生效",
            sm_asset_here.join("、")
        ));
    }
    // 认出来"本来就是我们自己的文件"（所以不备份、只覆盖）的条目
    let mut ours_overwritten: Vec<String> = Vec::new();
    if !overwrite_own.is_empty() {
        notes.push(format!(
            "已覆盖本项目的旧文件：{}",
            overwrite_own.join("、")
        ));
    }
    if !overwrite_other.is_empty() {
        notes.push(format!(
            "已覆盖游戏原有的同名文件（已备份）：{}",
            overwrite_other.join("、")
        ));
    }
    if existing_manifest.is_none() {
        let others: Vec<&str> = PROXY_ENTRIES
            .iter()
            .copied()
            .filter(|e| {
                *e != proxy
                    && target_dir.join(e).is_file()
                    && !crate::scan::identify_dll(&target_dir.join(e)).is_ours()
            })
            .collect();
        if !others.is_empty() {
            notes.push(format!(
                "目录里还有 {}，不是本项目文件，建议确认是否冲突",
                others.join("、")
            ));
        }
    }

    // 2. 占用检查
    for f in files {
        let p = target_dir.join(&f.dest_name);
        if p.exists() {
            match util::lock_state(&p) {
                util::LockState::InUse => {
                    bail!("{} 正被占用，请先完全退出游戏。", f.dest_name)
                }
                util::LockState::NoAccess => bail!(
                    "{} 现在写不进去：文件带了只读属性，或者权限不足。去掉只读 / 换个可写的游戏目录再试。",
                    f.dest_name
                ),
                util::LockState::Free => {}
            }
        }
    }

    // 3. 读源文件并算哈希
    // **不再把源文件整份读进内存**：代理 DLL 30 MB，两个运行库加起来 60 多 MB，
    // 全量入内存会让一次部署瞬时多占近 90 MB（和「轻量」的定位冲突）。
    // 只算哈希，真正的内容在第 6 步按文件复制。
    let mut payload: Vec<(String, PathBuf, String)> = Vec::new();
    for f in files {
        let sha = util::sha256_file(&f.src).with_context(|| format!("读取 {}", f.src.display()))?;
        payload.push((f.dest_name.clone(), f.src.clone(), sha));
    }

    let key = util::dir_key(target_dir);
    let base = util::backups_dir()?.join(&key);
    let files_dir = base.join("files");
    std::fs::create_dir_all(&files_dir)?;
    // 从这里开始就在动这个目录的备份了：先拿锁，避免两个实例同时写同一份记录
    let _lock = DirLock::acquire(&base)?;

    // 4. 备份
    //
    // 重部署（上游更新后再点一次「部署」）时必须沿用**第一次**的备份。
    // 否则会把「我们上次部署进去的文件」当成游戏原文件重新备份一遍，
    // 把真正的原件覆盖掉 —— 之后「还原」只能还原出我们自己部署的版本。
    let prev: BTreeMap<String, BackupEntry> = existing_manifest
        .as_ref()
        .map(|m| {
            m.files
                .iter()
                .map(|e| (e.rel_path.clone(), e.clone()))
                .collect()
        })
        .unwrap_or_default();

    let mut entries = Vec::new();
    for (name, _bytes, sha) in &payload {
        // 之前部署过，而且它的原始备份还在 -> 直接沿用，不重新备份
        if let Some(p) = prev.get(name) {
            let backup_intact = match &p.backup_name {
                Some(bn) => files_dir.join(bn).is_file(),
                // 当时本来就没有原文件，那也不需要备份
                None => !p.existed_before,
            };
            if backup_intact {
                entries.push(BackupEntry {
                    rel_path: name.clone(),
                    existed_before: p.existed_before,
                    backup_name: p.backup_name.clone(),
                    original_sha256: p.original_sha256.clone(),
                    deployed_sha256: sha.clone(),
                });
                continue;
            }
        }

        let dst = target_dir.join(name);
        let present = dst.is_file();
        /* 这个位置的东西如果是**我们自己放进去的**（老版本铺的、或用户手动装的），
        就不能记成"游戏原件" —— 见 is_our_artifact 的说明。
        注意：有部署记录的那条路在上面已经沿用旧备份了，走到这里说明**没有记录**
        （手动装的目录、或记录被清过），正是最容易误判成游戏原件的情况。 */
        let ours_here = present
            && std::fs::read(&dst)
                .map(|b| is_our_artifact(&dst, name, &b))
                .unwrap_or(false);
        if ours_here {
            ours_overwritten.push(name.clone());
        }
        let existed = present && !ours_here;
        let mut backup_name = None;
        let mut original_sha256 = None;
        if existed {
            // 「读原件 + 落成 .bak」走公共层：备份名统一扁平化（见 backup_file_name），
            // 0.4.0 那批 dlssg_sm86\backend\x.dll 的条目也不会变成嵌套路径。
            let orig = backup_restore::snapshot_one(target_dir, name, &files_dir)?
                .ok_or_else(|| anyhow::anyhow!("{name} 读不出来，无法备份原件"))?;
            original_sha256 = Some(util::sha256_hex(&orig));
            backup_name = Some(backup_restore::backup_file_name(name));
        }
        entries.push(BackupEntry {
            rel_path: name.clone(),
            existed_before: existed,
            backup_name,
            original_sha256,
            deployed_sha256: sha.clone(),
        });
    }

    /* 这里才轮到说这句话：ours_overwritten 是上面那个循环填的。
    放早了会说空话（曾经就是这样，提示根本没出现过）。 */
    if !ours_overwritten.is_empty() {
        notes.push(format!(
            "已是本项目文件，已直接覆盖、没有当成游戏原件备份：{}",
            ours_overwritten.join("、")
        ));
    }

    // 4b. 用户确认要移除的「另一个本项目代理」也必须先备份。
    // 记成 existed_before=true 的条目，这样「还原」还能把原件放回去 ——
    // 上游的步骤是「备份到单独目录，再移出游戏目录」，两步都不能少。
    // 已经在旧 manifest 里记过账的交给第 8 步处理，这里不重复备份
    // （重复备份会用当前文件覆盖掉真正的原件）。
    let mut extra_removed: Vec<String> = Vec::new();
    for name in extra_proxies {
        if !PROXY_ENTRIES.contains(&name.as_str()) || name == proxy {
            continue;
        }
        if prev.contains_key(name) {
            continue;
        }
        if entries.iter().any(|e| &e.rel_path == name) {
            continue;
        }
        let dst = target_dir.join(name);
        if !dst.is_file() {
            continue;
        }
        let orig = backup_restore::snapshot_one(target_dir, name, &files_dir)?
            .ok_or_else(|| anyhow::anyhow!("{name} 读不出来，无法备份原件"))?;
        let sha = util::sha256_hex(&orig);
        let bn = backup_restore::backup_file_name(name);
        entries.push(BackupEntry {
            rel_path: name.clone(),
            existed_before: true,
            backup_name: Some(bn),
            original_sha256: Some(sha.clone()),
            deployed_sha256: sha,
        });
        extra_removed.push(name.clone());
    }

    // 5. 先写 manifest，保证后面任何失败都能回滚
    let mut manifest = BackupManifest {
        game_key: key,
        game_dir: target_dir.to_path_buf(),
        deployed_at: util::now_utc(),
        proxy_name: proxy.to_owned(),
        files: entries,
        // 先标「正在安装」：接下来这几十行要是被硬终止，下一次启动能看出来没装完
        installing: true,
    };
    write_manifest(&base, &manifest)?;

    // 6. 原子写入。**中途失败也要回滚**：manifest 已经落盘了，只写一半会让界面
    //    显示「已部署」而文件其实是混的。回滚的成败如实写进错误信息。
    for (name, src, _sha) in &payload {
        let dst = target_dir.join(name);
        // 目标在子目录里（0.4.0 的 dlssg_sm86\backend\...）时先把目录建出来，
        // 否则 copy 一步就失败，用户看到的是「写入 dlssg_sm86\backend\x.dll 失败」。
        if let Some(parent) = dst.parent() {
            if !parent.is_dir() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建目录 {}", parent.display()))?;
            }
        }
        let tmp =
            util::long_path(&target_dir.join(format!(".{}.tmp", name.replace(['\\', '/'], "_"))));
        // 报错里用**目标文件名**，不要把 .xxx.tmp 这种内部细节甩给用户
        let step = std::fs::copy(util::long_path(src), &tmp)
            .with_context(|| format!("写入 {}", name))
            .and_then(|_| util::atomic_replace(&tmp, &dst));
        if let Err(e) = step {
            let _ = std::fs::remove_file(&tmp);
            // 直接走内部函数：锁已经在本函数手里，不能再抢一次
            let rb = restore_with(&manifest);
            return Err(anyhow::anyhow!("{e}；回滚{}", rollback_text(&rb)));
        }
        util::clear_motw(&dst);
    }

    // 7. 校验，失败回滚
    for (name, _bytes, sha) in &payload {
        let got = util::sha256_file(&target_dir.join(name))?;
        if got != *sha {
            let rb = restore_with(&manifest);
            bail!("{} 部署后校验失败；回滚{}", name, rollback_text(&rb));
        }
    }

    // 8. 清掉这次不用的代理入口
    //
    // 三个来源，规则见 plan_orphan：
    //   * 用户在弹窗里确认要移除的「另一个本项目代理」（extra_removed）—— 上面已备份
    //   * 旧 manifest 记过账、这次不用的：本来空着、是我们放的就直接删；那位置
    //     **原本就有文件**的也要删（不然两个代理并存），但备份记录必须搬进新
    //     manifest，否则「还原」再也放不回原件
    //   * 其它：不碰
    //
    // 另外：凡是有原件备份、这次又不部署的条目，记录都要一直传下去 —— 包括文件
    // 已经不在了的。因为「还原」收尾会把整个备份目录删掉，记录一丢，原件就永久
    // 拿不回来了。
    let mut removed: Vec<String> = Vec::new();
    let mut carried: Vec<BackupEntry> = Vec::new();
    let mut kept_record: Vec<String> = Vec::new();
    // 移不掉的（被占用 / 只读 / 权限）：必须说出来，而且记录要留着 ——
    // 否则界面显示「已部署」、目录里却还立着本项目的旧代理，「还原」也清不掉。
    let mut failed_remove: Vec<String> = Vec::new();
    // 被清掉的 0.3.x 遗留运行库（单独列，好在界面上说清楚这是"升级清理"）
    let mut removed_legacy: Vec<String> = Vec::new();

    for name in &extra_removed {
        let p = target_dir.join(name);
        if std::fs::remove_file(&p).is_ok() {
            kept_record.push(name.clone());
        } else {
            failed_remove.push(name.clone());
        }
    }

    if let Some(old) = &existing_manifest {
        for e in &old.files {
            if payload.iter().any(|(n, _, _)| *n == e.rel_path) {
                continue;
            }
            let p = match crate::util::safe_join(target_dir, &e.rel_path) {
                Ok(p) => p,
                Err(_) => {
                    // 清单里的路径非法（被篡改/损坏）：跳过，绝不按原始字符串拼路径去删/写
                    continue;
                }
            };
            let present = p.is_file();
            let still_ours = present
                && util::sha256_file(&p)
                    .map(|h| h == e.deployed_sha256)
                    .unwrap_or(false);
            let has_backup = e.existed_before && e.backup_name.is_some();
            // 0.3.x 铺在游戏根目录、0.4.0 不再需要的运行库：只要这份确实是我们当初
            // 放进去的，就一并清掉 —— 否则游戏可能优先加载根目录那份旧的。
            // 只清**我们自己放过的**：记录里 existed_before=true 的（游戏原本就有的那份）
            // 绝不动。
            if !e.existed_before && LEGACY_ROOT_LIBS.contains(&e.rel_path.as_str()) {
                if present && still_ours {
                    if std::fs::remove_file(&p).is_ok() {
                        removed_legacy.push(e.rel_path.clone());
                    } else {
                        /* 删不掉（游戏正开着、文件被占用）：**记录必须传下去** ——
                        下面 failed_remove 的说明写着"本次记录已保留，下次点「还原」仍可处理"，
                        记录一丢那句话就是假的，那个旧运行库再也没人清得掉。
                        restore 只在"文件仍是 deployed_sha256 那份"时才删（deploy.rs 第 2 步），
                        所以传下去是安全的。 */
                        failed_remove.push(e.rel_path.clone());
                        carried.push(e.clone());
                    }
                }
                continue;
            }
            let action = plan_orphan(
                PROXY_ENTRIES.contains(&e.rel_path.as_str()),
                false,
                present,
                still_ours,
                e.existed_before,
            );
            let mut keep_record = false;
            match action {
                OrphanAction::Keep => {
                    // 只要有原件备份，这条记录**必须**传下去：还原收尾会把整个备份
                    // 目录删掉，记录一丢，那份原件就永久拿不回来了。
                    // （以前这里还要求「文件不在了或还是我们那份」，于是用户自己改过
                    //   那个文件时记录被丢掉、备份被连带删除 —— 和本文件的约定相反。）
                    keep_record = has_backup;
                }
                OrphanAction::Remove => {
                    if std::fs::remove_file(&p).is_ok() {
                        removed.push(e.rel_path.clone());
                    } else {
                        // 删不掉：记录留着，下次还原还能处理它
                        failed_remove.push(e.rel_path.clone());
                        keep_record = has_backup;
                    }
                }
                OrphanAction::RemoveButKeepRecord => {
                    if std::fs::remove_file(&p).is_ok() {
                        kept_record.push(e.rel_path.clone());
                    } else {
                        failed_remove.push(e.rel_path.clone());
                    }
                    keep_record = has_backup;
                }
            }
            if keep_record && !carried.iter().any(|c| c.rel_path == e.rel_path) {
                carried.push(e.clone());
            }
        }
    }

    if !removed.is_empty() {
        notes.push(format!(
            "已移除上次部署、这次不用的代理入口：{}（避免同时存在两个代理）",
            removed.join("、")
        ));
    }
    if !removed_legacy.is_empty() {
        notes.push(format!(
            "已清掉根目录里 0.3.x 时代的运行库：{}（新版运行库在 {RUNTIME_DIR}\\runtime\\，留着旧的会被游戏优先加载）",
            removed_legacy.join("、")
        ));
    }
    if !kept_record.is_empty() {
        notes.push(format!(
            "已按你的选择移出本项目的其它代理入口：{}（上游要求每次只留一个），原件在备份里，点「还原」可以恢复",
            kept_record.join("、")
        ));
    }
    if !failed_remove.is_empty() {
        notes.push(format!(
            "没能移出这些旧代理入口（可能正被占用或只读）：{} —— 本次记录已保留，下次点「还原」仍可处理",
            failed_remove.join("、")
        ));
    }

    if !carried.is_empty() {
        // 把记录补进刚落盘的新 manifest，否则「还原」找不到它们
        manifest
            .files
            .retain(|m| !carried.iter().any(|c| c.rel_path == m.rel_path));
        manifest.files.extend(carried);
        write_manifest(&base, &manifest)?;
    }

    // 文件全部写完、校验也过了：把「正在安装」落成「安装完成」。
    manifest.installing = false;
    write_manifest(&base, &manifest)?;

    let names: Vec<&str> = payload.iter().map(|(n, _, _)| n.as_str()).collect();
    let mut msg = format!(
        "已部署 {} 个文件 -> {}：{}",
        names.len(),
        target_dir.display(),
        names.join("、")
    );
    if !notes.is_empty() {
        msg.push_str(" | ");
        msg.push_str(&notes.join(" | "));
    }
    Ok(msg)
}

/// 「这个游戏目录正在被操作」的锁文件。
///
/// 两个实例（或两个解压目录）同时部署/还原同一个游戏目录时，会往同一份 manifest
/// 和同一批 .bak 上写 —— 互相覆盖之后原件就找不回来了。UI 里的 busy 只挡得住同一个进程。
pub struct DirLock(PathBuf);

impl DirLock {
    /// 拿这把锁。DLSS5 那边（dlss5.rs）也用它：同一个游戏目录的备份记录
    /// 不能被两个实例同时写，理由见上面这段。
    pub fn acquire(base: &Path) -> Result<Self> {
        std::fs::create_dir_all(base)?;
        let p = base.join(".lock");
        if let Some(lock) = try_create(&p) {
            return Ok(lock);
        }
        // 已经有一把锁：先看它是不是**崩溃残留**。
        // 为什么必须判（实测踩到）：sidecar 被强杀 / 任务管理器结束进程 / 断电之后，
        // 这个文件会留下来 —— 那时这个游戏就永远部署不了，界面只会让他去删一个
        // 藏在 %APPDATA% 备份目录里的文件，基本等于卡死。
        if lock_is_stale(&p) {
            crate::log::line(&format!("清理陈旧的部署锁：{}", p.display()));
            let _ = std::fs::remove_file(&p);
            if let Some(lock) = try_create(&p) {
                return Ok(lock);
            }
        }
        bail!(
            "另一个窗口正在操作这个游戏目录（{}）。等它结束后再试；\
             如果确认没有别的窗口在跑，把这个文件删掉即可。",
            p.display()
        )
    }
}

/// 独占创建锁文件，并把「谁拿的、什么时候拿的」写进去。
fn try_create(p: &Path) -> Option<DirLock> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(p)
        .ok()?;
    let _ = writeln!(f, "{}", std::process::id());
    let _ = writeln!(f, "{}", util::now_epoch_secs());
    Some(DirLock(p.to_path_buf()))
}

/// 锁文件是不是崩溃残留：写进去的进程号已经不在，或者时间戳超过 10 分钟。
/// 读不出来（空文件 / 内容坏了）也算残留 —— 那种锁谁也解释不了。
fn lock_is_stale(p: &Path) -> bool {
    /// 部署本身几秒到一两分钟，10 分钟足够把「真在跑」和「死锁」分开。
    const STALE_SECS: u64 = 600;
    let Ok(text) = std::fs::read_to_string(p) else {
        return true;
    };
    let mut it = text.lines();
    let pid: u32 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let when: u64 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    // 没有时间戳 = 老版本写的、或者写了一半的锁：归属根本判断不了，按残留处理
    // （实测：旧版留下的空 .lock 会把一个游戏永久锁死）。
    if when == 0 {
        return true;
    }
    if util::now_epoch_secs().saturating_sub(when) > STALE_SECS {
        return true;
    }
    pid > 0 && !process_alive(pid)
}

/// 那个进程还在吗（OpenProcess + GetExitCodeProcess == STILL_ACTIVE）。
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        let _ = CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 把回滚的成败说清楚。以前无论成败都写「已自动回滚」—— 回滚失败时那是撒谎。
fn rollback_text(r: &Result<RestoreOutcome>) -> String {
    match r {
        Ok(o) if o.complete() => format!("成功（{}）", o.msg),
        Ok(o) => format!("**没有完全回滚**：{}", o.msg),
        Err(e) => {
            format!("**失败**：{e}（游戏目录可能停在半部署状态，请点「还原」再试一次）")
        }
    }
}

/// 「还原」的结果：给人看的话 + 这次**没删掉**的本工具文件（相对路径）。
///
/// leftover 非空 = 部分还原。调用方必须**保留部署记录**：记录一删，目录里剩下的文件
/// 就成了「本工具无记录」（界面显示「已安装，本工具无记录」），用户只能去点「清理」，
/// 而「清理」没有原件备份 —— 真正的游戏原件再也放不回来。
pub struct RestoreOutcome {
    pub msg: String,
    pub leftover: Vec<String>,
}

impl RestoreOutcome {
    /// 目录已经收干净（记录可以安全删掉）。
    pub fn complete(&self) -> bool {
        self.leftover.is_empty()
    }
}

fn restore_with(manifest: &BackupManifest) -> Result<RestoreOutcome> {
    let target_dir = manifest.game_dir.as_path();
    let base = util::backups_dir()?.join(&manifest.game_key);

    // 1) 先把所有备份读出来校验完。**任何一份坏了都要在动游戏目录之前中止** ——
    //    以前是先无条件删掉部署文件、再逐个读备份：备份坏掉时游戏目录已经被删了
    //    一半，原件又没放回去，留下一个半残的游戏目录。
    let files_dir = base.join("files");
    let refs: Vec<BackupRef> = manifest
        .files
        .iter()
        .filter_map(|e| {
            if !e.existed_before {
                return None;
            }
            let bn = e.backup_name.as_deref()?;
            Some(BackupRef {
                rel: &e.rel_path,
                backup_name: bn,
                original_sha256: e.original_sha256.as_deref(),
            })
        })
        .collect();
    let loaded = backup_restore::load_originals(&files_dir, &refs)?;

    let mut skipped: Vec<String> = Vec::new();
    // 记录里有、但文件读不出来（被游戏 / 杀毒软件独占）：既不能确认归属、更不能删，
    // 只能如实报出来并保留记录。以前这里直接 continue —— 记录随后被删，用户看到的是
    // 「本工具无记录」，目录里却还留着我们的文件。
    let mut unreadable: Vec<(String, String)> = Vec::new();
    // 记录说"这个位置原本就有文件"，但那份"原件"其实是我们自己放进去的 —— 不能写回
    let mut healed: Vec<String> = Vec::new();
    // 游戏自带的 NVIDIA DLSS 运行库：没有原件备份时**不删**（删了游戏会丢帧生成选项）
    let mut kept_game_libs: Vec<String> = Vec::new();
    let mut plan = BackupPlan::new();
    for ((rel, _existed, bytes), r) in loaded.entries.into_iter().zip(refs.iter()) {
        // 清单里的路径非法（被篡改/损坏）：跳过，绝不按原始字符串拼路径去删/写
        if crate::util::safe_join(target_dir, &rel).is_err() {
            skipped.push(rel);
            continue;
        }
        let src = files_dir.join(r.backup_name);
        /* 那份"游戏原件"本身如果是本项目放的文件（按签名或 0.3.x 指纹认），就**不写回**：
        写回去等于把我们自己的 mod 文件当成游戏原件塞进游戏目录，而且从此被当成
        游戏文件、本工具再也清不掉 —— 实测复现过的恶性 bug（手动装过 0.3.x 的目录，
        老版本会把我们自己的代理备份成"游戏原件"）。这份备份留在备份目录里不动，
        用户想核对还能找到。 */
        if is_our_artifact(&src, &rel, &bytes) {
            healed.push(rel);
            continue;
        }
        plan.push(rel, true, bytes);
    }

    // 2) 只删「确认还是我们部署的那一份」的文件。用户换成别的 mod、或游戏更新
    //    了自己那份 nvngx_dlssg.dll 之后，那个文件已经不是我们的了 —— 删掉它
    //    等于替用户丢文件，而且没有备份能救。
    for e in &manifest.files {
        let p = match crate::util::safe_join(target_dir, &e.rel_path) {
            Ok(p) => p,
            Err(_) => {
                if !skipped.contains(&e.rel_path) {
                    skipped.push(e.rel_path.clone());
                }
                continue;
            }
        };
        if !p.is_file() {
            continue;
        }
        let bytes = match std::fs::read(&p) {
            Ok(b) => b,
            Err(io_err) => {
                unreadable.push((e.rel_path.clone(), io_err.to_string()));
                continue;
            }
        };
        /* 上一次「部分还原」已经把原件放回去了（重试时就会看到）：这一条不用再动，
        更不能当成「内容被别人改过」去跳过 —— 那会让部署记录永远删不掉。
        直接拿备份里的原件字节比，比哈希更准（老记录可能没有 original_sha256）。 */
        let already_original = plan
            .get(&e.rel_path)
            .map(|(_, existed_before, original)| *existed_before && original == &bytes)
            .unwrap_or(false);
        if already_original {
            continue;
        }
        let same_as_deployed =
            !e.deployed_sha256.is_empty() && util::sha256_hex(&bytes) == e.deployed_sha256;
        /* 内容不是这次部署的那份时还有第二种可能：**位置被本工具放的别的东西顶掉了**
        （最典型的是 Smooth Motion 的解锁组件，它也占 version.dll）。那同样是我们放的，
        要一并收走 —— 留着会让游戏继续加载到 Smooth Motion 的组件，而用户以为还原干净了。 */
        let still_ours = same_as_deployed || is_our_artifact(&p, &e.rel_path, &bytes);
        if still_ours {
            // 有原件的那几条由回滚写回原件（等于覆盖掉它），这里只登记「这次新建的」
            if plan.existed_before(&e.rel_path) != Some(true) {
                if is_protected_game_lib(&e.rel_path) {
                    // 没有备份能证明这个位置原本是什么：保留它。删掉游戏自带的
                    // nvngx_dlssg.dll 会让 DLSS 帧生成选项直接消失（Cyberpunk 实测）。
                    kept_game_libs.push(e.rel_path.clone());
                } else {
                    plan.push(e.rel_path.clone(), false, Vec::new());
                }
            }
        } else {
            skipped.push(e.rel_path.clone());
        }
    }

    // 3) 先把原件放回去，再删我们放进去的那些。顺序反过来的话，中途失败会留下
    //    「我们的删了、原件也没回来」的状态（顺序由 backup_restore::rollback 保证）。
    let rb = backup_restore::rollback(target_dir, &plan)?;
    // 3b) 插件运行时在旁边建的目录（日志 / bundle 缓存）。它不是我们部署的文件，
    //     所以 manifest 里没有；但不删的话，用户点完「还原」目录里还留着它。
    let (swept, sweep_err) = sweep_runtime_dir(target_dir);

    // 3c) **真正删掉了吗？** 游戏 / 启动器 / 反作弊还开着时，version.dll 这类文件
    //     会被独占，删不掉；杀毒软件也可能正扫着刚写进去的 DLL。以前这里不看结果、
    //     直接按「已还原」收尾并把记录删了 —— 目录里留下我们的文件、记录却没了，
    //     界面翻成「本工具无记录」，用户只能再点一次「清理」，游戏原件再也放不回来。
    let mut leftover: Vec<String> = rb.failed.iter().map(|(rel, _)| rel.clone()).collect();
    leftover.extend(unreadable.iter().map(|(rel, _)| rel.clone()));
    if sweep_err.is_some() {
        leftover.push(format!("{RUNTIME_DIR}\\"));
    }

    /* 4) 以下情况都要保留备份目录并如实说明，别把「原件还在」这一点弄丢：
    · 有文件被用户改过（skipped）：不能删，否则原件跟着没了
    · 记下的"游戏原件"其实是我们自己的文件（healed）：那份备份也不能删 ——
      用户可能想核对它到底是不是游戏自带的
    · 还有本工具的文件没删掉（leftover）：记录留着才能再点一次「还原」收尾 */
    let keep_backups =
        !skipped.is_empty() || !healed.is_empty() || !unreadable.is_empty() || !leftover.is_empty();
    let mut msg = if keep_backups {
        format!(
            "部分还原：恢复 {} 个原始文件、删除 {} 个本工具写入的文件",
            rb.restored, rb.removed
        )
    } else {
        format!(
            "已还原：恢复 {} 个原始文件、删除 {} 个本工具写入的文件",
            rb.restored, rb.removed
        )
    };
    if !skipped.is_empty() {
        msg.push_str(&format!(
            "；{} 个文件的内容已不是本工具部署的那份，为避免误删已跳过（{}）",
            skipped.len(),
            skipped.join("、")
        ));
    }
    if !healed.is_empty() {
        // 括号里的文件名去掉（用户要求）：数量已经说明问题，名字在这一屏是噪音
        msg.push_str(&format!(
            "；另有 {} 个位置当年记下的「游戏原件」其实是本项目自己放的文件，没有写回游戏目录",
            healed.len()
        ));
    }
    if swept {
        msg.push_str(&format!("；另清理插件运行时留下的目录：{RUNTIME_DIR}\\"));
    }
    if !unreadable.is_empty() {
        let shown: Vec<String> = unreadable
            .iter()
            .take(5)
            .map(|(rel, why)| format!("{rel}（{why}）"))
            .collect();
        msg.push_str(&format!(
            "；{} 个文件读不出来、没法确认归属，已跳过（{}）",
            unreadable.len(),
            shown.join("、")
        ));
    }
    if !kept_game_libs.is_empty() {
        msg.push_str(&format!(
            "\n\n· 保留了 {}（游戏自带的 DLSS 文件；删掉会让游戏里的 DLSS 帧生成选项消失，需要时用 Steam「验证文件完整性」恢复）。",
            kept_game_libs.join("、")
        ));
    }
    if !rb.failed.is_empty() {
        let shown: Vec<String> = rb
            .failed
            .iter()
            .take(5)
            .map(|(rel, why)| format!("{rel}（{why}）"))
            .collect();
        let more = rb.failed.len().saturating_sub(5);
        msg.push_str(&format!(
            "；{} 个文件没能删掉：{}{}",
            rb.failed.len(),
            shown.join("、"),
            if more > 0 {
                format!(" 等 {more} 个")
            } else {
                String::new()
            }
        ));
    }
    if let Some(e) = &sweep_err {
        msg.push_str(&format!(
            "；插件运行时目录 {RUNTIME_DIR}\\ 也没能删掉（{e}）"
        ));
    }
    if !leftover.is_empty() {
        msg.push_str(
            "\n\n原因多半是游戏（或它的启动器 / 反作弊）还开着，或杀毒软件正在扫描这些文件。\
             **部署记录已经保留**：请完全退出游戏后再点一次「还原」，就能把它收干净。",
        );
    }
    if keep_backups {
        if leftover.is_empty() {
            msg.push_str(&format!(
                "\n\n备份保留在 {}，确认不需要了可以手动删除。",
                base.display()
            ));
        } else {
            msg.push_str(&format!(
                "\n\n备份还在 {}，先别删 —— 下一次「还原」还要靠它把原件放回去。",
                base.display()
            ));
        }
    } else {
        let _ = std::fs::remove_dir_all(&base);
    }
    Ok(RestoreOutcome { msg, leftover })
}

/// 删掉插件运行时建的 dlssg_sm86 目录。返回（是否真删掉了, 删不掉的原因）——
/// 目录不存在算「没删」，不算失败（它本来就不在）。
fn sweep_runtime_dir(dir: &Path) -> (bool, Option<String>) {
    let p = dir.join(RUNTIME_DIR);
    if !p.is_dir() {
        return (false, None);
    }
    match std::fs::remove_dir_all(&p) {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    }
}

/// 目录里「本项目的帧生成文件」清单 —— 给「用户手动装过、没有本工具记录」的目录用。
///
/// 判据（保守，避免误删别人的东西）：
///   1. 代理入口：**按签名认**（本项目的自签证书），不看文件名 —— 所以就算用户手动
///      放了 version.dll/winmm.dll…，只要是我们签的那份就能认出来；
///   2. 两份 DLSS 运行库：它们由 NVIDIA 官方签名，游戏自带的也是同一份签名，
///      光看文件分不出是谁放的 —— 所以**只在目录里确实有本项目的代理入口时**才算进来
///      （没有代理入口就不存在帧生成 mod，也就没必要动运行库）；
///   3. dlssg_sm86.ini：同理，只在有代理入口时才认。
pub fn manual_files(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in PROXY_ENTRIES {
        let p = dir.join(n);
        if p.is_file() && crate::scan::identify_dll(&p).is_ours() {
            out.push((*n).to_owned());
        }
    }
    if out.is_empty() {
        return out;
    }
    // nvngx_dlssg.dll / nvngx_dlss.dll 不列入：它们和游戏自带的是同一份 NVIDIA DLL，
    // 没有备份就删除会直接破坏游戏的 DLSS 帧生成（Cyberpunk 2077 实测）。
    // 有部署记录时由 restore 按备份处理；没有记录时一律不动。
    let p = dir.join(INI_NAME);
    if p.is_file()
        && std::fs::read(&p)
            .map(|b| is_known_ours(INI_NAME, &b))
            .unwrap_or(false)
    {
        out.push(INI_NAME.to_owned());
    }
    // 插件运行时建的目录（日志 / bundle 缓存）。同上：只有确实有本项目的代理入口时
    // 才算 —— 免得把一个碰巧同名、其实是用户自己东西的目录删掉。
    if dir.join(RUNTIME_DIR).is_dir() {
        out.push(format!("{RUNTIME_DIR}\\（整个目录）"));
    }
    out
}

/// 「没有部署记录」时的清理：把本项目的帧生成文件删掉。
///
/// 与 restore 的区别：restore 有备份、能把游戏原件放回去；这里**没有备份**，
/// 只能删（用户手动装的，或者记录丢了）。所以只删能确认是本项目的文件，
/// 删完复查一遍并如实报告。
pub fn cleanup_manual(dir: &Path) -> Result<String> {
    let files = manual_files(dir);
    if files.is_empty() {
        bail!("这个目录里没有找到本项目的帧生成文件（{}）", dir.display());
    }
    // 和其它写操作共用同一把锁：避免和另一个实例的部署/还原撞在一起
    let _lock = DirLock::acquire(&util::backups_dir()?.join(util::dir_key(dir)))?;
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    for n in &files {
        // 清单里可能带「（整个目录）」这种后缀（运行时目录），按实际类型删
        let raw = n.split('（').next().unwrap_or(n).trim_end_matches('\\');
        let p = util::long_path(&dir.join(raw));
        let r = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        match r {
            Ok(_) => deleted.push(n.clone()),
            Err(e) => failed.push(format!("{n}（{e}）")),
        }
    }
    // 归属存疑的一律没删（见 manual_files / is_known_ours）。如实说出来：
    // 既免得用户以为「清理完就干净了」，也免得下次又被当成我们的文件。
    let kept: Vec<String> = ["nvngx_dlssg.dll", "nvngx_dlss.dll", INI_NAME]
        .iter()
        .filter(|n| {
            let p = dir.join(*n);
            p.is_file()
                && std::fs::read(&p)
                    .map(|b| !is_known_ours(n, &b))
                    .unwrap_or(false)
        })
        .map(|n| (*n).to_owned())
        .collect();
    let mut msg = format!("已删除 {} 项：{}", deleted.len(), deleted.join("、"));
    if !failed.is_empty() {
        msg.push_str(&format!("\n· 没删掉的：{}", failed.join("、")));
    }
    if !kept.is_empty() {
        msg.push_str(&format!(
            "\n· 这些同名文件的内容对不上我们发布过的任何一版，无法确认是本工具放的，已保留：{}",
            kept.join("、")
        ));
    }
    msg.push_str(&format!(
        "\n· 注意：这个目录**没有找到部署记录**（记录可能已经被清理过），所以只能删掉本工具的文件，游戏原件无法自动恢复。\n· 原件如果还在，可以到备份目录里找：{}",
        util::backups_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "（读不到备份目录）".to_owned())
    ));
    let left = manual_files(dir);
    if left.is_empty() {
        msg.push_str("\n· 复查通过：目录里已经没有本项目的帧生成文件了。");
    } else {
        msg.push_str(&format!("\n· 复查还有残留：{}", left.join("、")));
    }
    Ok(msg)
}

pub fn restore(target_dir: &Path) -> Result<String> {
    Ok(restore_report(target_dir)?.msg)
}

/// 同 restore，但把「还有哪些文件没删掉」也带出来：界面据此显示「部分还原」，
/// 而不是把没删干净报成「已还原」。
pub fn restore_report(target_dir: &Path) -> Result<RestoreOutcome> {
    let Some(m) = load_manifest(target_dir) else {
        bail!(
            "没有找到本工具对该目录的部署记录（{}）",
            target_dir.display()
        );
    };
    // 和 deploy 用同一把锁：两个实例同时读写同一份备份会互相覆盖 ——
    // 那正是「原件找不回来」的典型路径。加在读到记录之后，
    // 这样「本来就没部署过」的目录不会平白多一个锁文件。
    let _lock = DirLock::acquire(&util::backups_dir()?.join(&m.game_key))?;
    restore_with(&m)
}

/// 部署前的准备结果（纯数据，不含任何 UI 状态）。
pub struct PreparedDeploy {
    pub files: Vec<DeployFile>,
    pub notes: Vec<String>,
}

/// 组装本次要写入的文件（代理 DLL + 缺的运行库 + INI）。
///
/// 与 egui 版 do_deploy() 的步骤一一对应，语义完全相同：
///   1. 代理 DLL 本体；
///   2. 游戏目录已有的 DLSS 运行库不动，缺的才补；
///   3. INI：出厂默认用资产原文件，改过档位才生成 deploy.ini。
// 参数就是「部署档位」的原始输入，逐项传递便于和 INI 的键一一对照。
#[allow(clippy::too_many_arguments)]
pub fn prepare_deploy(
    dir: &Path,
    proxy: &str,
    fg_optimized: u8,
    fg_frames: u8,
    fg_preset: &str,
    fg_sr: bool,
    fg_engine: &str,
    fg_xess_flow: &str,
    fg_xess_hudless: &str,
) -> Result<PreparedDeploy> {
    let mut files = Vec::new();
    let mut notes = Vec::new();

    /* 部署前先把内置资产按分发表核一遍（0.4.0 起资产是一整个目录，缺一个文件
    在游戏里的表现都是"没生效"）。核不过就拒绝部署并列出是哪个文件 ——
    用户看到的是"资产不完整，请重装"，而不是去游戏里反复试。 */
    let bad_assets = verify_asset_inventory(&crate::update::asset_path(proxy)?);
    if !bad_assets.is_empty() {
        anyhow::bail!(
            "内置资产校验不通过（{} 项）：\n· {}\n\n这通常是安装不完整或被安全软件清掉了，请重新安装本程序（资产随安装包分发）。",
            bad_assets.len(),
            bad_assets.join("\n· ")
        );
    }

    let dll = crate::update::asset_path(proxy)?;
    if !dll.is_file() {
        anyhow::bail!(
            "资产缺失（{proxy}）—— 资产随安装包内置，请检查安装是否完整（或重新安装本程序）"
        );
    }
    files.push(DeployFile::new(proxy, dll));

    /* 0.4.0 起运行库不再铺在游戏根目录（搬进 dlssg_sm86\runtime\），所以：
    * 老用户（0.3.x 我们铺过根目录运行库）—— 那两份**是我们放的**，不能当"游戏自带的"
      留着：新版会在 dlssg_sm86\runtime\ 里带自己的运行库，根目录再留一份旧的
      可能被游戏优先加载，表现就是"升级上来了还是旧行为"。这里直接不把它们放进
      本次负载，交给部署第 8 步的清单比对删掉（记录里 existed_before=false，可以安全删）。
    * 从没用过本工具、但游戏目录自带运行库的目录 —— 照旧不动（那是游戏自己的东西）。 */
    /* 0.3.x 时代运行库内嵌在代理里，这里按"已有的不动、缺的才补"把两份 DLSS 运行库
    铺到游戏目录。**0.4.0 起这条规则失效了**：运行库搬进了 dlssg_sm86\runtime\，
    再往游戏根目录铺一份 66 MB 的旧库，既浪费又有被优先加载的风险。
    所以 0.4.0 布局下这一整套直接跳过；根目录残留的旧库由下面那段负责清掉。 */
    let (have, need) = if RUNTIME_IS_SEPARATE_DIR {
        (Vec::new(), Vec::new())
    } else {
        crate::update::runtime_deploy_plan(dir)
    };
    if !RUNTIME_IS_SEPARATE_DIR {
        let prev = load_manifest(dir);
        let we_put_it = |n: &str| {
            prev.as_ref()
                .map(|m| m.files.iter().any(|e| e.rel_path == n && !e.existed_before))
                .unwrap_or(false)
        };
        // 这两个列表此时恒为空（上面已短路），保留这段只为让"如果哪天又需要往根目录
        // 铺运行库"的改动有落点；真正的清理在部署第 8 步按记录做。
        let _ = we_put_it;
    }
    for name in &have {
        notes.push(format!("游戏目录已有 {name}，用游戏自带的那份（不覆盖）"));
    }
    for name in &need {
        let p = crate::update::asset_path(name).unwrap_or_default();
        if !p.is_file() {
            anyhow::bail!(
                "缺少运行库 {name} —— 资产随安装包内置，请检查安装是否完整（或重新安装本程序）"
            );
        }
        files.push(DeployFile::new(name, p));
    }

    let asset_ini = crate::update::asset_path(crate::update::INI_REPO_PATH).unwrap_or_default();
    // 老 0.4.0 的 ini 里有 Mode=On（= 原本开着超分）：迁移时按“开”处理，避免升级后静默关掉
    let legacy_sr_on = std::fs::read_to_string(dir.join(INI_NAME))
        .map(|s| s.lines().any(|l| l.trim().eq_ignore_ascii_case("Mode=On")))
        .unwrap_or(false);
    let (ini_path, ini_notes) = crate::update::prepare_deploy_ini(
        &asset_ini,
        fg_optimized,
        fg_frames,
        fg_preset,
        fg_sr || legacy_sr_on,
        fg_engine,
        fg_xess_flow,
        fg_xess_hudless,
    )?;
    for n in ini_notes {
        notes.push(n);
    }
    files.push(DeployFile::new(INI_NAME, ini_path));

    /* 0.4.0 起运行库/后端/内核搬进了 dlssg_sm86\ 目录：只放代理 DLL 什么都不会发生，
    所以整个目录都要铺进游戏目录，并**逐个文件进清单**（「还原」才收得干净）。
    代理旁边那个 30 MB 的时代结束了：现在的代理不到 1 MB。 */
    let runtime_src = crate::update::asset_path(RUNTIME_DIR)?;
    if runtime_src.is_dir() {
        let mut count = 0usize;
        for entry in walkdir::WalkDir::new(&runtime_src)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&runtime_src)
                .map(|r| {
                    r.to_string_lossy()
                        .replace('/', std::path::MAIN_SEPARATOR_STR)
                })
                .unwrap_or_default();
            if rel.is_empty() {
                continue;
            }
            files.push(DeployFile::new(
                &format!("{RUNTIME_DIR}{MAIN_SEP}{rel}"),
                entry.path().to_path_buf(),
            ));
            count += 1;
        }
        // 齐不齐要在这里拦住：少一个文件游戏里就是"没生效"，而用户完全看不出为什么。
        for must in [BACKEND_REL, KERNELS_REL, RUNTIME_LIB_REL] {
            let want = format!("{RUNTIME_DIR}{MAIN_SEP}{must}");
            if !files.iter().any(|f| f.dest_name == want) {
                anyhow::bail!(
                    "内置资产不完整：缺少 {want}。请重新安装本程序（资产随安装包分发）。"
                );
            }
        }
        notes.push(format!(
            "已铺入 {RUNTIME_DIR}\\ 运行库目录（{count} 个文件）"
        ));
    }

    Ok(PreparedDeploy { files, notes })
}

/// 我们**内置**的那一份 0.4.0 资产的分发表：(相对路径, 字节数, sha256)。
///
/// 为什么要这张表：资产是随安装包分发的二进制，一旦被安全软件"清理"、被半截下载覆盖、
/// 或者我们自己打包时缺了一个文件，表现都是**游戏里没生效**，而用户完全看不出原因。
/// 部署前逐个核对大小与哈希，不符就拒绝部署并说清是哪个文件 —— 宁可现在报错，
/// 也不要让用户带着一堆文件去游戏里试。
///
/// 数值全部来自上游 0.4.0 完整包里的官方 manifest.json（我方已逐字节核对过）。
/// 上游换版本时这张表要跟着换，否则会拦住正常部署 —— 这是有意的：
/// 宁可拦住让人改表，也不要放一份来路不明的资产进游戏目录。
pub const ASSET_INVENTORY: &[(&str, u64, &str)] = &[
    (
        "version.dll",
        815_240,
        "275ffa781b2d244ad269d1d65d4e5b68a12a3f89ff2baaec58d100eb5a1ff411",
    ),
    (
        "dlssg_sm86.ini",
        10_092,
        "97713bfa9032121a6acfadf3fe1ec9aca467f27ee9d8f80095d3bf0f1267d298",
    ),
    (
        "dlssg_sm86\\backend\\sm86_backend.dll",
        2_312_840,
        "d1b56f6ee9a58ba6284804303cf903535c7acbf4fdce3320f6fd21f50569d357",
    ),
    (
        "dlssg_sm86\\kernels\\fg_kernels.dll",
        11_875_464,
        "f59a45a04eddda2f2ebae4b8685b05af4432a80ce87cc78d83e4b9414e079a20",
    ),
    (
        "dlssg_sm86\\runtime\\nvngx_dlssg.dll",
        7_506_544,
        "cadee1aa6db3907babdc3285da2aebc4553d92b89c38ba3eae8abd8bba1bea72",
    ),
    (
        "dlssg_sm86\\xefg\\xefg_kernels.dll",
        16_351_880,
        "e72ed96bd5ea51743ca9e3b07a7d8af522eeb6a34cf4143f0ff1147d7dff13bf",
    ),
];

pub const SR_INVENTORY: &[(&str, u64, &str)] = &[
    (
        "dlssg_sm86\\sr\\sr_kernels.dll",
        106_570_376,
        "b8d5d4cac3142647630b72a4a29271de35679646d48bdd2b70a71876da0fae78",
    ),
    (
        "dlssg_sm86\\sr\\nvngx_dlss.dll",
        74_187_376,
        "858ccff96360c3098db9264cd6f13d04a027e09b6ce7ad5bbc297570e824b4f2",
    ),
];

/// 按分发表检查资产（**只看在不在、大小对不对**，不算哈希）。
///
/// 和 verify_asset_inventory 的分工：这个给界面用，每次聚焦窗口都会调，
/// 所以必须是纯 stat 的廉价操作；哈希校验留在部署前做（那时一次就够，慢一点无妨）。
pub fn asset_inventory_gaps(root: &Path) -> Vec<String> {
    let mut bad = Vec::new();
    for (rel, size, _sha) in ASSET_INVENTORY.iter().chain(SR_INVENTORY.iter()) {
        let p = root.join(rel);
        match std::fs::metadata(&p) {
            Ok(m) if m.len() == *size => {}
            Ok(m) => bad.push(format!("{rel}（大小 {} ≠ {}）", m.len(), size)),
            Err(_) => bad.push(format!("{rel}（缺失）")),
        }
    }
    bad
}

/// 列出 dlssg_sm86\\ 运行库目录里的文件（相对该目录的路径 + 字节数），按路径排序。
/// 界面用它把"新版资产到底铺了哪些文件"摊开给人看。
pub fn runtime_dir_files(root: &Path) -> Vec<(String, u64)> {
    let dir = root.join(RUNTIME_DIR);
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(&dir)
            .map(|r| {
                r.to_string_lossy()
                    .replace('/', std::path::MAIN_SEPARATOR_STR)
            })
            .unwrap_or_default();
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if !rel.is_empty() {
            out.push((rel, size));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 按上面的分发表检查资产目录。返回不合格的说明（空 = 全部合格）。
///
/// 只查分发那一套（代理 + INI + dlssg_sm86\ 里必须有那几个）；超分 sr\ 是可选项，
/// 单独查它在不在、大小对不对，但不因为它缺就拒绝部署。
pub fn verify_asset_inventory(runtime_src: &Path) -> Vec<String> {
    let mut bad = Vec::new();
    let root = runtime_src.parent().unwrap_or(runtime_src);
    for (rel, size, sha) in ASSET_INVENTORY {
        let p = root.join(rel);
        if !p.is_file() {
            bad.push(format!("{rel}（缺失）"));
            continue;
        }
        let got_size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        if got_size != *size {
            bad.push(format!("{rel}（大小 {got_size} ≠ {size}）"));
            continue;
        }
        match crate::util::sha256_file(&p) {
            Ok(h) if h.eq_ignore_ascii_case(sha) => {}
            Ok(h) => bad.push(format!(
                "{rel}（哈希 {} ≠ {}）",
                &h[..8.min(h.len())],
                &sha[..8]
            )),
            Err(e) => bad.push(format!("{rel}（读不出来：{e}）")),
        }
    }
    bad
}

/// 游戏自带的 NVIDIA DLSS 运行库。它们和本工具内置/旧版发布的 DLL 可能是**同一份文件**
/// （NVIDIA 官方签名、逐字节相同），按名字/大小/哈希都无法区分归属。
///
/// 因此：**任何清理/还原路径都不许在没有逐字节备份的情况下删除它们** ——
/// 删掉 Cyberpunk 这类游戏的 nvngx_dlssg.dll，游戏里的 DLSS 帧生成选项会直接消失，
/// 只能靠 Steam「验证文件完整性」重新下载（2026-10 用户实测）。
pub const PROTECTED_GAME_LIBS: [&str; 2] = ["nvngx_dlssg.dll", "nvngx_dlss.dll"];

/// 这个路径（可以是子目录里的）是不是游戏自带的 DLSS 运行库。
pub fn is_protected_game_lib(name: &str) -> bool {
    let norm = name.replace('/', "\\").to_ascii_lowercase();
    // dlssg_sm86\runtime\ 下的是我们随包部署的现代运行库，不是游戏自带文件；
    // 它必须能被正常备份/还原，否则重复部署会被当成游戏原件。
    if norm.starts_with("dlssg_sm86\\") || norm.contains("\\dlssg_sm86\\") {
        return false;
    }
    let short = norm.rsplit('\\').next().unwrap_or(&norm);
    PROTECTED_GAME_LIBS
        .iter()
        .any(|n| short.eq_ignore_ascii_case(n))
}

/// 0.3.x 那一套（我们 1.0.x 版本发布时内置的资产）的精确内容指纹：(文件名, 字节数, sha256)。
///
/// 为什么需要它：**那两份 NVIDIA 运行库没法按签名认** —— 它们是 NVIDIA 官方签名的，
/// 游戏自己带的也是同一份签名，光看签名分不出是谁放的。只能按内容认。
///
/// 数据来源：直接从我们 1.0.7 安装包里的资产算出来的；其中 nvngx_dlssg.dll 的哈希
/// 与上游发行包 THIRD_PARTY_NOTICES.txt 里记的 310.9.1 哈希逐字一致，可交叉印证。
pub const LEGACY_ARTIFACTS: &[(&str, u64, &str)] = &[
    (
        "version.dll",
        30_021_920,
        "c3934a09399f022504227c72df0bf8c0de55f9a08880dddde898c5262cefa838",
    ),
    (
        "winmm.dll",
        30_033_184,
        "bc3cef25c1ccbbdccdd93db9fae845104bb6c71fd433240e26641881a7e46658",
    ),
    (
        "dbghelp.dll",
        30_039_840,
        "dfe7f44baa68d237dfe040d4b4cfc9bd44b01e661f3051996db1144e6bb388cb",
    ),
    (
        "dinput8.dll",
        30_021_408,
        "4fbad08bb9360a1cd1309a82586353484fe64b7220199344ab83fae308176c6b",
    ),
    (
        "dxgi.dll",
        30_021_920,
        "52e67020a725c74d5dee62aee4dc166a2f4e5c3adabf04ecab3005ab968fc24d",
    ),
    (
        "d3d12.dll",
        30_021_920,
        "64b7ecc3f6d70011feed8dfaa20ce500522e8b04170142e2a2000146b4d7ac71",
    ),
    (
        "nvngx_dlssg.dll",
        7_460_976,
        "ff6e90eb78b827927dff5b4ecc6b1c870c2e9bca29ed9f48c7d348cc9e170b82",
    ),
    (
        "nvngx_dlss.dll",
        58_956_912,
        "3975567b8943c53acce397f2b72380092f84f162d00b0d2c7d08a1025c563983",
    ),
    (
        "dlssg_sm86.ini",
        3_548,
        "2616857ee29ec61e33c8b52e1b50f4c93cb5339adbb13b73f0ae71a722427a43",
    ),
];

/// 这份内容是不是"我们 0.3.x 时代发布过的那个文件"（按 文件名+大小+哈希 精确判定）。
///
/// 先比大小再算哈希：老代理 28.6 MB、超分运行库 56 MB，算哈希不便宜，
/// 大小不符的文件在第一步就被挡掉了。
fn is_legacy_artifact(name: &str, bytes: &[u8]) -> bool {
    let short = name.rsplit(['\\', '/']).next().unwrap_or(name);
    for (n, size, sha) in LEGACY_ARTIFACTS {
        if *n == short && bytes.len() as u64 == *size && crate::util::sha256_hex(bytes) == *sha {
            return true;
        }
    }
    false
}

/// 这份文件的内容是不是「我们发布过的某一版」（0.3.x 或 0.4.0 的资产表里能逐字节对上）。
///
/// 用途：**没有部署记录**时判断归属。签名只能认代理 DLL（我们自己签的），
/// 而 NVIDIA 那两份运行库 / 那份 ini 跟游戏自带的无法靠签名区分，只能按内容认。
/// 对不上 = 我们**不能证明**是自己放的 —— 上层要按「不动它」处理。
fn is_known_ours(name: &str, bytes: &[u8]) -> bool {
    if is_legacy_artifact(name, bytes) {
        return true;
    }
    let short = name.rsplit(['\\', '/']).next().unwrap_or(name);
    ASSET_INVENTORY.iter().any(|(n, size, sha)| {
        n.rsplit(['\\', '/']).next() == Some(short)
            && bytes.len() as u64 == *size
            && crate::util::sha256_hex(bytes) == *sha
    })
}

/// 测试入口：按 文件名 + 内容 判断是不是 0.3.x 那一套里的文件。
/// （给自测用；生产代码走 is_our_artifact。）
pub fn is_legacy_artifact_like(name: &str, bytes: &[u8]) -> bool {
    is_legacy_artifact(name, bytes)
}

/// 这个游戏目录里部署的帧生成资产"是哪一套"。
///
/// 只做**廉价判定**（stat + 读 PE 证书表，不算哈希）：这个函数会被游戏库每次刷新时
/// 对每个游戏调用一次，30 MB 的哈希是不能放在这里的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetFamily {
    /// 0.4.0 那一套：代理由 dlssg_sm86\ 目录 + INI 组成，代理不到 1 MB
    V040,
    /// 0.4.1 那一套（布局相同，内核/后端/xefg/sr 全部更新）
    V041,
    /// 0.3.x 那一套：代理 28.6 MB，运行库铺在游戏根目录
    Legacy03x,
}

impl AssetFamily {
    pub fn version(self) -> &'static str {
        match self {
            AssetFamily::V040 => "0.4.0",
            AssetFamily::V041 => "0.4.1",
            AssetFamily::Legacy03x => "0.3.x",
        }
    }
    /// 这一套是不是"旧的那一套"（界面上要用黄色提醒重新部署）
    pub fn is_legacy(self) -> bool {
        matches!(self, AssetFamily::Legacy03x)
    }
}

/// 同一套布局下区分 0.4.0 / 0.4.1：**按内核文件哈希**判定。
///
/// 之前用文件大小（11_875_464）猜，上游哪天大小相同就会误判；哈希是唯一的。
fn modern_family(dir: &Path) -> AssetFamily {
    let k = dir.join(RUNTIME_DIR).join("kernels").join("fg_kernels.dll");
    let Ok(cur) = crate::util::sha256_file_cached(&k) else {
        return AssetFamily::V040;
    };
    let want = ASSET_INVENTORY
        .iter()
        .find(|(name, _, _)| name.ends_with("fg_kernels.dll"))
        .map(|(_, _, sha)| *sha);
    if let Some(w) = want {
        if cur.eq_ignore_ascii_case(w) {
            return AssetFamily::V041;
        }
    }
    AssetFamily::V040
}

/// 一个游戏目录里"帧生成资产"的样子 —— 徽章、批量重新部署、清理入口都用它。
#[derive(Debug, Clone)]
pub struct DeployedAssets {
    pub family: AssetFamily,
    /// 部署用的代理入口名（界面拿它去重新部署，不用去解析状态字符串）
    pub proxy: String,
    /// 随包运行库的版本号；读不出来就是空串
    pub runtime_version: String,
    /// 本工具**没有**这个目录的部署记录（用户自己手动装的）
    pub manual: bool,
    /// 还留在目录里的 0.3.x 文件（名字 + 字节数，只按 名字+大小 判，不算哈希）
    pub leftovers: Vec<(String, u64)>,
}

/// 随包运行库的字节数 -> 版本号。两份我们发布过的运行库大小完全不同，stat 就够。
const RUNTIME_BY_SIZE: [(u64, &str); 2] = [(7_506_544, "310.9.16"), (7_460_976, "310.9.1")];

/// 读出这个游戏目录里帧生成资产的版本信息。没有帧生成文件时返回 None。
///
/// 判据按可靠性排序，全部是廉价操作：
///   1. 部署记录（manifest）里有 dlssg_sm86\ 条目 -> 0.4.0；只有代理+INI -> 0.3.x
///   2. 目录里有没有 dlssg_sm86\ 子目录
///   3. 在位的代理（按签名认）的字节数：不到 1 MB -> 0.4.0，否则 0.3.x
pub fn deployed_assets(dir: &Path) -> Option<DeployedAssets> {
    let manifest = load_manifest(dir);
    // 在位的、属于本项目的代理入口
    let ours_proxy = PROXY_ENTRIES
        .iter()
        .find(|n| {
            let p = dir.join(n);
            p.is_file() && crate::scan::identify_dll(&p).is_ours()
        })
        .map(|n| (*n).to_owned());

    let proxy = manifest
        .as_ref()
        .map(|m| m.proxy_name.clone())
        .or_else(|| ours_proxy.clone());
    // 一份帧生成文件都没有 -> 不是"部署过的目录"
    let proxy = proxy?;

    let has_runtime_dir = dir.join(RUNTIME_DIR).is_dir();
    let from_manifest = manifest.as_ref().and_then(|m| {
        if m.files.iter().any(|e| e.rel_path.starts_with(RUNTIME_DIR)) {
            Some(modern_family(dir))
        } else if !m.files.is_empty() {
            Some(AssetFamily::Legacy03x)
        } else {
            None
        }
    });
    let family = from_manifest.unwrap_or({
        if has_runtime_dir {
            modern_family(dir)
        } else {
            // 没有记录、也没有新目录：看代理多大。0.4.0 的代理不到 1 MB，
            // 0.3.x 的是 28.6 MB —— 差两个数量级，不会认错。
            let small = ours_proxy
                .as_ref()
                .and_then(|n| std::fs::metadata(dir.join(n)).ok())
                .map(|m| m.len() < 1_000_000)
                .unwrap_or(false);
            if small {
                modern_family(dir)
            } else {
                AssetFamily::Legacy03x
            }
        }
    });

    // 运行库版本：先看新版位置，再看游戏根目录
    let runtime_version = [
        dir.join(RUNTIME_DIR)
            .join("runtime")
            .join("nvngx_dlssg.dll"),
        dir.join("nvngx_dlssg.dll"),
    ]
    .iter()
    .find_map(|p| {
        let len = std::fs::metadata(p).ok()?.len();
        RUNTIME_BY_SIZE
            .iter()
            .find(|(size, _)| *size == len)
            .map(|(_, v)| (*v).to_owned())
    })
    .unwrap_or_default();

    Some(DeployedAssets {
        family,
        proxy,
        runtime_version,
        manual: manifest.is_none(),
        leftovers: legacy_leftover_candidates(dir),
    })
}

/// 目录里还留着的 0.3.x 文件 —— **只按 名字 + 大小** 判（不算哈希，供每次刷新调用）。
///
/// 名字和大小两个条件同时命中才算：我们发布过的那几个文件大小都很特别
/// （28.6 MB 的代理、7.46 MB / 54.9 MB 的运行库），游戏自带的文件几乎不可能同时撞上。
/// 真正删除之前会再按哈希核一遍（见 clean_legacy_leftovers）。
pub fn legacy_leftover_candidates(dir: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for (name, size, _sha) in LEGACY_ARTIFACTS {
        // 游戏自带的 NVIDIA DLSS 运行库：名字/大小/哈希都可能和我们旧资产一致，
        // 但不能据此认定是我们的文件。永远不列入清理候选。
        if is_protected_game_lib(name) {
            continue;
        }
        let p = dir.join(name);
        if let Ok(m) = std::fs::metadata(&p) {
            if m.is_file() && m.len() == *size {
                out.push(((*name).to_owned(), m.len()));
            }
        }
    }
    out
}

/// 删掉目录里 0.3.x 的遗留文件，返回（删掉的, 没删掉的）。
///
/// 和上面那个"候选"函数的区别：这里**逐字节核对哈希**才删 —— 这是用户点按钮的
/// 一次性动作，慢一点没关系，但绝不能删错（游戏自带的运行库也不能碰）。
///
/// 这是给"我们清不掉"的情况兜底的：用户手动装过 0.3.x、之后又用新版部署过一次，
/// 于是那些运行库既不在新清单里、也无法从签名上认出来。
pub fn clean_legacy_leftovers(dir: &Path) -> (Vec<String>, Vec<String>) {
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    for (name, size) in legacy_leftover_candidates(dir) {
        if is_protected_game_lib(&name) {
            continue;
        }
        let p = dir.join(&name);
        let Ok(bytes) = std::fs::read(&p) else {
            failed.push(format!("{name}（读不出来）"));
            continue;
        };
        if bytes.len() as u64 != size || !is_legacy_artifact(&name, &bytes) {
            // 大小对上了但内容不是我们那份 -> 是别人的文件，绝不碰
            continue;
        }
        match std::fs::remove_file(&p) {
            Ok(_) => deleted.push(name),
            Err(e) => failed.push(format!("{name}（{e}）")),
        }
    }
    (deleted, failed)
}

/// 这个位置躺着的东西**是不是我们自己放进去的**（与有没有部署记录无关）。
///
/// 为什么必须能在没有记录时也认出来：真实场景是老用户**手动装过 0.3.x** —— 游戏目录里
/// 有我们的代理和运行库，但本工具没有任何记录。这种文件**绝不能当成"游戏原件"备份**：
/// 一旦记成原件，「还原」就会把我们自己的 mod 文件写回游戏目录，而且从此被当成游戏文件、
/// 再也清不掉（实测复现过的恶性 bug）。
///
/// 两条判据，按可靠性排序：
///   1. DLL：Authenticode 签名主体是本项目的自签证书 —— 任何版本、任何文件名都认得出，
///      而且绝不会误判（真正的 Windows version.dll 是微软签名的）；
///   2. 其它文件（INI 之类）：按内容指纹认。
///
/// name = 这个文件**在游戏目录里的名字**。
///
/// 不要用备份文件的文件名去比指纹：备份名带 .bak 后缀，比不出东西。
pub fn is_our_artifact(path: &Path, name: &str, bytes: &[u8]) -> bool {
    // NVIDIA 那两份运行库和游戏自带的是同一份文件：永远不要声称是我们的。
    // 否则部署时不会备份游戏原件，还原时会把游戏文件当我们的删掉。
    if is_protected_game_lib(name) {
        return false;
    }
    if crate::scan::identify_dll(path).is_ours() {
        return true;
    }
    // Smooth Motion 的解锁组件没有我们的签名，靠内容哈希认
    if crate::sm::is_our_asset_file(path) {
        return true;
    }
    /* dlssg_sm86\ 里的运行库、后端、内核是 NVIDIA 签名的，签名判定认不出"这是我们放的"；
    但内容就是内置资产里那一份。**重复部署时不能把它们当游戏原件备份** ——
    否则「还原」会把我们上次部署的旧运行库写回游戏目录（和 0.3.x 时代那个坑同源）。 */
    if is_our_runtime_file(name, bytes) {
        return true;
    }
    is_legacy_artifact(name, bytes)
}

/// 这个文件是不是「内置资产里 dlssg_sm86\ 目录下的那一份」（按内容哈希比）。
fn is_our_runtime_file(name: &str, bytes: &[u8]) -> bool {
    let prefix = format!("{RUNTIME_DIR}\\");
    let Some(rel) = name.strip_prefix(&prefix) else {
        return false;
    };
    if rel.is_empty() || rel.contains("..") {
        return false;
    }
    let Ok(dir) = crate::update::asset_path(RUNTIME_DIR) else {
        return false;
    };
    let p = dir.join(rel);
    if !p.is_file() {
        return false;
    }
    crate::util::sha256_file(&p)
        .map(|h| h == crate::util::sha256_hex(bytes))
        .unwrap_or(false)
}

/// 0.4.0 起运行库、后端、内核都进了 dlssg_sm86\\ 目录，不再铺在游戏根目录。
/// 留成常量是为了让"根目录那份旧运行库该不该清"这条判断有个唯一的开关。
const RUNTIME_IS_SEPARATE_DIR: bool = true;

/// 部署清单里的路径分隔符（Windows 上就是反斜杠，清单里统一按它写）。
const MAIN_SEP: char = std::path::MAIN_SEPARATOR;

/// 0.4.0 目录里三个**必须有**的文件：缺任何一个，游戏里就是"没生效"。
const BACKEND_REL: &str = "backend\\sm86_backend.dll";
const KERNELS_REL: &str = "kernels\\fg_kernels.dll";
const RUNTIME_LIB_REL: &str = "runtime\\nvngx_dlssg.dll";

/// 0.3.x 曾经铺在游戏根目录、0.4.0 不再需要的运行库。
///
/// 为什么必须在部署时清掉：新版把运行库搬进了 dlssg_sm86\runtime\，根目录再留一份旧版
/// **可能被游戏误加载**（同目录优先）。对老用户来说这就是"升级上来了却还是旧行为"。
/// 只在我们确认"这份还是当初我们写进去的、或它本来就不存在"时才动（见 plan_orphan）。
pub const LEGACY_ROOT_LIBS: [&str; 2] = ["nvngx_dlssg.dll", "nvngx_dlss.dll"];

/// Smooth Motion 与帧生成同时开着时给用户的那句提醒。
///
/// 两个都是插帧（一个在驱动层、一个在渲染管线里），同时开通常更糟。
/// 产品规定是**提醒而不是拦**：用户可以忽略这句继续部署。
pub const SM_CONFLICT_NOTE: &str =
    "该游戏疑似已开启Smooth Motion。不建议同时开启Smooth Motion和DLSS-FG。但你可以忽略这条提示继续部署。";

/// 一步到位：准备 + 部署（返回部署说明与备注）。
// 同 prepare_deploy：档位参数逐项传，调用方（sidecar / 自测）一眼能对上界面下拉框。
#[allow(clippy::too_many_arguments)]
pub fn deploy_game(
    dir: &Path,
    proxy: &str,
    fg_optimized: u8,
    fg_frames: u8,
    fg_preset: &str,
    fg_sr: bool,
    fg_engine: &str,
    fg_xess_flow: &str,
    fg_xess_hudless: &str,
) -> Result<(String, Vec<String>)> {
    let prep = prepare_deploy(
        dir,
        proxy,
        fg_optimized,
        fg_frames,
        fg_preset,
        fg_sr,
        fg_engine,
        fg_xess_flow,
        fg_xess_hudless,
    )?;
    let msg = deploy(dir, proxy, &prep.files, &[])?;
    Ok((msg, prep.notes))
}

/// 部署的帧生成资产是否旧于当前内置。
///
/// 两把尺子：
///   1) **逐文件哈希**：manifest 里记的 `deployed_sha256` != 当前内置文件哈希 → 有更新
///      （0.4.1 → 0.4.2 即使运行库版本号不变也能认出来）；
///   2) 兜底：0.3.x 旧布局、或 0.4.0 资产族，一律视为需要重新部署。
pub fn is_outdated(dir: &Path) -> bool {
    let Some(a) = deployed_assets(dir) else {
        return false;
    };
    if a.family.is_legacy() {
        return true;
    }
    let Some(m) = load_manifest(dir) else {
        return a.family == AssetFamily::V040;
    };
    for (rel, _, _) in ASSET_INVENTORY.iter().chain(SR_INVENTORY.iter()) {
        let Some(e) = m
            .files
            .iter()
            .find(|e| e.rel_path.eq_ignore_ascii_case(rel))
        else {
            continue;
        };
        if e.deployed_sha256.is_empty() {
            continue;
        }
        let Ok(bundled) = crate::update::asset_path(rel) else {
            continue;
        };
        if !bundled.is_file() {
            continue;
        }
        let Ok(cur) = crate::util::sha256_file_cached(&bundled) else {
            continue;
        };
        if !cur.eq_ignore_ascii_case(&e.deployed_sha256) {
            return true;
        }
    }
    a.family == AssetFamily::V040
}

#[cfg(test)]
mod proxy_inventory_tests {
    use super::*;

    /// T5：代理表必须和 `bundled-assets/` 里的真实文件一致（换了资产却忘了改表 → 测挂）。
    #[test]
    fn proxy_table_matches_bundled_assets() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/electron/bundled-assets");
        if !dir.is_dir() {
            return; // CI / 无资产环境：跳过（发版机上有资产，会真的核对）
        }
        for (name, size, sha) in PROXY_ASSET_SHA256.iter() {
            let f = dir.join(name);
            assert!(f.is_file(), "{name} 不存在于 bundled-assets");
            let meta = std::fs::metadata(&f).expect("metadata");
            assert_eq!(meta.len(), *size, "{name} 大小与表不符");
            let got = crate::util::sha256_file(&f).expect("sha256");
            assert!(got.eq_ignore_ascii_case(sha), "{name} 哈希与表不符");
        }
    }

    /// 游戏自带的 NVIDIA DLSS 运行库不能被当成 0.3.x 残留删掉。
    /// 回归：Cyberpunk 2077 的 bin\x64\nvngx_dlssg.dll 与旧资产逐字节相同，
    /// 旧逻辑在还原/自动清理时把它删了，游戏里的 DLSS 帧生成选项直接消失。
    #[test]
    fn protected_game_libs_are_never_legacy_leftovers() {
        let d = std::env::temp_dir().join(format!(
            "fgm-protected-lib-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("nvngx_dlssg.dll");
        let file = std::fs::File::create(&f).unwrap();
        file.set_len(7_460_976).unwrap();
        drop(file);

        assert!(legacy_leftover_candidates(&d).is_empty());
        let (deleted, failed) = clean_legacy_leftovers(&d);
        assert!(
            deleted.is_empty(),
            "protected lib must not be deleted: {deleted:?}"
        );
        assert!(failed.is_empty());
        assert!(f.is_file());

        assert!(is_protected_game_lib("C:\\Game\\bin\\x64\\NVNGX_DLSSG.DLL"));
        assert!(is_protected_game_lib("nvngx_dlss.dll"));
        assert!(!is_protected_game_lib("version.dll"));
        assert!(!is_our_artifact(&f, "nvngx_dlssg.dll", b"whatever"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// T5：ReShade 兜底表的形状（大小写无关比较依赖 64 位十六进制）。
    #[test]
    fn reshade_known_table_is_sane() {
        assert!(!RESHADE_KNOWN.is_empty());
        for (size, sha) in RESHADE_KNOWN.iter() {
            assert!(*size > 0);
            assert_eq!(sha.len(), 64);
        }
    }
}
