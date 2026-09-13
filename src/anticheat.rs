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

        let tier = if ty == SERVICE_TYPE_KERNEL_DRIVER {
            AcTier::Kernel
        } else {
            *default_tier
        };

        let kind = if ty == SERVICE_TYPE_KERNEL_DRIVER { "内核驱动" } else { "服务" };
        report.push(
            label,
            tier,
            format!("注册表服务 {} [{}] Type=0x{:X} Start={}", name, kind, ty, start),
        );
    }

    report
}

/// 游戏目录里出现的反作弊目录名
const DIR_MARKERS: &[(&str, &str)] = &[
    ("EasyAntiCheat", "Easy Anti-Cheat (EAC)"),
    ("EasyAntiCheat_EOS", "Easy Anti-Cheat (EOS)"),
    ("BattlEye", "BattlEye"),
    ("GameGuard", "nProtect GameGuard"),
    ("XignCode", "XignCode3"),
];

/// 游戏目录里出现的反作弊文件名
const FILE_MARKERS: &[(&str, &str)] = &[
    ("EasyAntiCheat.sys", "EAC 内核驱动"),
    ("EasyAntiCheat_EOS.sys", "EAC EOS 内核驱动"),
    ("BEDaisy.sys", "BattlEye 内核驱动"),
    ("BEService.exe", "BattlEye 服务"),
    ("start_protected_game.exe", "EAC 受保护启动器"),
    ("vgk.sys", "Riot Vanguard 内核驱动"),
    ("x3.xem", "XignCode3"),
];

fn check_flat(dir: &Path, report: &mut AcReport) {
    for (name, label) in DIR_MARKERS {
        let p = dir.join(name);
        if p.is_dir() {
            report.push(label, AcTier::Kernel, format!("目录存在: {}", p.display()));
        }
    }
    for (name, label) in FILE_MARKERS {
        let p = dir.join(name);
        if p.is_file() {
            report.push(label, AcTier::Kernel, format!("文件存在: {}", p.display()));
        }
    }
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

    let mut cur = dir.parent();
    for _ in 0..4 {
        let Some(p) = cur else { break };
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
    let mut visited = 0usize;
    for entry in walkdir::WalkDir::new(dir)
        .max_depth(4)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        visited += 1;
        if visited > 4000 {
            break;
        }
        if entry.file_type().is_dir() {
            check_flat(entry.path(), &mut report);
        }
    }
    report
}
