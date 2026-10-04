//! 反作弊检测：注册表服务/驱动 + 游戏目录特征。
//!
//! 全部只读，不加载、不卸载、不结束任何驱动或服务。

use serde::{Deserialize, Serialize};
use std::path::Path;
use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AcTier {
    None,
    UserMode,
    Kernel,
}

impl AcTier {
    pub fn label(self) -> &'static str {
        match self {
            AcTier::None => "未检出",
            AcTier::UserMode => "用户态反作弊",
            AcTier::Kernel => "内核级反作弊",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcHit {
    pub name: String,
    pub tier: AcTier,
    pub evidence: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AcReport {
    pub hits: Vec<AcHit>,
    /// 目录太多，这次没扫完。**界面必须如实说**：没检出 ≠ 干净。
    #[serde(default)]
    pub truncated: bool,
}

impl AcReport {
    pub fn verdict(&self) -> AcTier {
        if self.hits.iter().any(|h| h.tier == AcTier::Kernel) {
            AcTier::Kernel
        } else if self.hits.iter().any(|h| h.tier == AcTier::UserMode) {
            AcTier::UserMode
        } else {
            AcTier::None
        }
    }

    pub fn is_blocked(&self) -> bool {
        self.verdict() == AcTier::Kernel
    }

    fn push(&mut self, name: &str, tier: AcTier, evidence: String) {
        if self.hits.iter().any(|h| h.name == name) {
            return;
        }
        self.hits.push(AcHit {
            name: name.to_owned(),
            tier,
            evidence,
        });
    }

    pub fn merge(&mut self, other: AcReport) {
        self.truncated |= other.truncated;
        for h in other.hits {
            if !self.hits.iter().any(|x| x.name == h.name) {
                self.hits.push(h);
            }
        }
    }
}

/// 服务名匹配表：(小写子串, 展示名, 默认等级)
const SERVICE_PATTERNS: &[(&str, &str, AcTier)] = &[
    ("bedaisy", "BattlEye 内核驱动 (BEDaisy)", AcTier::Kernel),
    ("beservice", "BattlEye 服务 (BEService)", AcTier::UserMode),
    ("easyanticheat_eos", "Easy Anti-Cheat (EOS)", AcTier::Kernel),
    ("easyanticheat", "Easy Anti-Cheat", AcTier::Kernel),
    ("vgk", "Riot Vanguard 内核驱动 (vgk)", AcTier::Kernel),
    ("vgc", "Riot Vanguard 服务 (vgc)", AcTier::UserMode),
    ("npggnt", "nProtect GameGuard 内核驱动", AcTier::Kernel),
    ("gameguard", "nProtect GameGuard", AcTier::UserMode),
    ("xigncode", "XignCode3", AcTier::Kernel),
    ("x3.", "XignCode3 (x3)", AcTier::Kernel),
    ("pnkbstra", "PunkBuster A", AcTier::UserMode),
    ("pnkbstrab", "PunkBuster B", AcTier::UserMode),
    ("ricochet", "COD Ricochet", AcTier::Kernel),
];

/// SERVICE_KERNEL_DRIVER
const SERVICE_TYPE_KERNEL_DRIVER: u32 = 0x0000_0001;

pub fn scan_system() -> AcReport {
    let mut report = AcReport::default();

    let services = match RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey("SYSTEM\\CurrentControlSet\\Services") {
        Ok(k) => k,
        Err(_) => return report,
    };

    for name in services.enum_keys().flatten() {
        let lower = name.to_lowercase();
        let Some((_, label, default_tier)) = SERVICE_PATTERNS.iter().find(|(pat, _, _)| lower.contains(pat)) else {
            continue;
        };

        // 读 Type 判断它到底是不是内核驱动，比靠名字猜准。
        // Start: 0=Boot, 1=System, 2=Auto
        let (ty, start) = services
            .open_subkey(&name)
            .map(|k| {
                let ty: u32 = k.get_value("Type").unwrap_or(0);
                let st: u32 = k.get_value("Start").unwrap_or(0xFF);
                (ty, st)
            })
            .unwrap_or((0, 0xFF));

        // Start=4 = 已禁用：注册项还留着，但驱动不会再加载。卸载游戏后残留的注册项
        // 就是这种 —— 一直当成内核级会让用户以为自己机器上跑着反作弊（长期误报）。
        let disabled = start == 4;
        let tier = if disabled {
            AcTier::UserMode
        } else if ty == SERVICE_TYPE_KERNEL_DRIVER {
            AcTier::Kernel
        } else {
            *default_tier
        };

        let kind = if disabled {
            "已禁用的残留注册项"
        } else if ty == SERVICE_TYPE_KERNEL_DRIVER {
            "内核驱动"
        } else {
            "服务"
        };
        report.push(
            label,
            tier,
            format!("注册表服务 {} [{}] Type=0x{:X} Start={}", name, kind, ty, start),
        );
    }

    report
}

/// 游戏目录里出现的反作弊目录名。
///
/// 这些**一律按用户态**记：光有一个同名目录不能证明内核驱动装上了（那可能只是
/// 卸载残留、或者只是启动器）。真正算内核级的证据是下面的 .sys 文件。
/// 以前这里也按内核级记，于是只剩一个 `BEService.exe`（用户态服务）的游戏也会弹
/// 「内核级反作弊 / 封号风险」—— 同一个东西在服务表里我们标的是用户态，两边自相矛盾。
const DIR_MARKERS: &[(&str, &str, AcTier)] = &[
    ("EasyAntiCheat", "Easy Anti-Cheat (EAC)", AcTier::UserMode),
    ("EasyAntiCheat_EOS", "Easy Anti-Cheat (EOS)", AcTier::UserMode),
    ("BattlEye", "BattlEye", AcTier::UserMode),
    ("GameGuard", "nProtect GameGuard", AcTier::UserMode),
    ("XignCode", "XignCode3", AcTier::UserMode),
];

/// 游戏目录里出现的反作弊文件名，带各自的等级：`.sys` 才是内核驱动，其余是用户态组件。
const FILE_MARKERS: &[(&str, &str, AcTier)] = &[
    ("EasyAntiCheat.sys", "EAC 内核驱动", AcTier::Kernel),
    ("EasyAntiCheat_EOS.sys", "EAC EOS 内核驱动", AcTier::Kernel),
    ("BEDaisy.sys", "BattlEye 内核驱动", AcTier::Kernel),
    ("vgk.sys", "Riot Vanguard 内核驱动", AcTier::Kernel),
    ("BEService.exe", "BattlEye 服务", AcTier::UserMode),
    ("start_protected_game.exe", "EAC 受保护启动器", AcTier::UserMode),
    ("x3.xem", "XignCode3", AcTier::UserMode),
];

fn check_flat(dir: &Path, report: &mut AcReport) {
    for (name, label, tier) in DIR_MARKERS {
        let p = dir.join(name);
        if p.is_dir() {
            report.push(label, *tier, format!("目录存在: {}", p.display()));
        }
    }
    for (name, label, tier) in FILE_MARKERS {
        let p = dir.join(name);
        if p.is_file() {
            report.push(label, *tier, format!("文件存在: {}", p.display()));
        }
    }
}

/// 「大家一起用的」目录 —— 它不是某个游戏的安装根，反作弊的祖先扫描到这些地方就停。
fn is_shared_root(p: &Path) -> bool {
    // 盘根
    if p.parent().is_none() {
        return true;
    }
    let name = p
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "common"
            | "steamapps"
            | "program files"
            | "program files (x86)"
            | "windows"
            | "users"
            | "programdata"
            | "appdata"
            | "games"
    )
}

/// 扫描部署目标目录：自身 + 最多 4 级祖先（EAC/BattlEye 通常装在游戏根目录，
/// 而部署目标可能是 ...\Binaries\Win64）。
pub fn scan_game_dir(dir: &Path) -> AcReport {
    let mut report = AcReport::default();

    check_flat(dir, &mut report);

    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                check_flat(&entry.path(), &mut report);
            }
        }
    }

    // 往上看最多 4 级（EAC / BattlEye 常装在游戏根目录，而部署目标可能是
    // ...\Binaries\Win64）。**扫到「大家一起用的」目录就停**：
    // C:\Program Files (x86)\EasyAntiCheat 这种机器级目录会让每一个游戏都被判成
    // 内核级反作弊 —— 那是误报，而且反复误报会让用户对真警告一起脱敏。
    let mut cur = dir.parent();
    for _ in 0..4 {
        let Some(p) = cur else { break };
        if is_shared_root(p) {
            break;
        }
        check_flat(p, &mut report);
        cur = p.parent();
    }

    report
}

/// 更彻底的扫描：额外做一次有上限的目录遍历。
/// 用于真正要写入的目标目录（部署闸门），因为 BattlEye 之类可能埋在
/// ...BinariesWin64BattlEye 这种三四层深的位置。
/// 有 4000 个目录的上限，避免在超大游戏目录上卡住。
pub fn scan_deep(dir: &Path) -> AcReport {
    let mut report = scan_game_dir(dir);
    // 上限按**目录数**算。以前把文件也算进去，于是 Asset/Content 很多的大游戏目录
    // 会在看到 BattlEye/EasyAntiCheat 之前就截断 —— 闸门静默放行，偏偏那正是最该拦的
    // 场景。截断时要打标记，让界面说「没扫完」而不是「没检出」。
    let mut dirs = 0usize;
    for entry in walkdir::WalkDir::new(dir)
        .max_depth(4)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_dir() {
            continue;
        }
        dirs += 1;
        if dirs > MAX_SCAN_DIRS {
            report.truncated = true;
            break;
        }
        check_flat(entry.path(), &mut report);
    }
    report
}

/// 一次深度扫描最多看多少个目录。只数目录，不数文件。
pub const MAX_SCAN_DIRS: usize = 4000;
