//! 显卡相关信息：驱动版本检测 + 显卡名称伪装（注册表）。
//!
//! 只做两件事：
//!   1. 读出 NVIDIA 驱动版本，和帧生成建议的最低版本比对；
//!   2. 在用户明确同意的前提下，只改 \`Enum\PCI\...\DeviceDesc\` 这一个值，
//!      把显卡名伪装成 50 系，并支持一键还原。
//!
//! 安全约定（动这段代码前请先读完）：
//!   * 只写 \`DeviceDesc\`。\`HardwareID\` / \`CompatibleIDs\` / \`Driver\` / \`Service\`
//!     / \`Mfg\` 一律不碰 —— 改这些会让驱动绑定失效。
//!   * 只处理 \`ClassGUID\` 是显示适配器、且 \`Service\` 是 nvlddmkm 的实例。
//!     \`VEN_10DE\` 下面还有 NVIDIA 高清音频控制器，不能按厂商号盲目匹配。
//!   * 类键下的 \`DriverDesc\` 也**故意不改**：本程序自己的 SM86 / SM75 路由判断
//!     读的就是它，改掉之后程序会把 RTX 30 系误判成「RTX 50 系，不需要本 Mod」。
//!   * 写之前必须备份并读回校验，校验不过就中止。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE};
use winreg::RegKey;

// ---------------------------------------------------------------- 常量

/// 帧生成建议的最低 NVIDIA 驱动版本。
///
/// 上游 0.3.1 的说明：建议 R580 以上，最低约 R555（更旧的驱动会自动改用 PTX，
/// 只在首次加载多一次 JIT），591.86 与 610.74 实测可用。这里按**最低**报，别吓人。
pub const MIN_FG_DRIVER: (u32, u32) = (555, 0);
pub const MIN_FG_DRIVER_TEXT: &str = "555.0（R555）";
/// 官方驱动下载页
pub const DRIVER_URL: &str = "https://www.nvidia.cn/geforce/drivers/";

const DISPLAY_CLASS: &str = "{4d36e968-e325-11ce-bfc1-08002be10318}";
pub const CLASS_KEY: &str =
    "SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}";
const ENUM_PCI: &str = "SYSTEM\\CurrentControlSet\\Enum\\PCI";
const NVIDIA_SERVICE: &str = "nvlddmkm";

/// 允许伪装成的型号。只放桌面版 50 系：名单固定，避免用户把型号填错。
pub const PRESETS: &[&str] = &[
    "NVIDIA GeForce RTX 5090",
    "NVIDIA GeForce RTX 5080",
    "NVIDIA GeForce RTX 5070 Ti",
    "NVIDIA GeForce RTX 5070",
    "NVIDIA GeForce RTX 5060 Ti",
    "NVIDIA GeForce RTX 5060",
];

// ---------------------------------------------------------------- 版本号

/// \`32.0.16.1692\` -> \`(616, 92)\`。
///
/// 规则：把版本号里的数字全部连起来取最后 5 位，前三后二。
/// 这是 NVIDIA Windows 驱动版本与市场版本号的固定对应关系，
/// 已用 nvidia-smi 在本机实测校验过（32.0.16.1692 <-> 616.92）。
pub fn parse_windows_version(v: &str) -> Option<(u32, u32)> {
    let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 5 {
        return None;
    }
    let tail = &digits[digits.len() - 5..];
    Some((tail[..3].parse().ok()?, tail[3..].parse().ok()?))
}

/// \`616.92\` -> \`(616, 92)\`
pub fn parse_marketing_version(v: &str) -> Option<(u32, u32)> {
    let (a, b) = v.trim().split_once('.')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

pub fn fmt_version(v: (u32, u32)) -> String {
    format!("{}.{:02}", v.0, v.1)
}

#[derive(Debug, Clone)]
pub struct DriverInfo {
    /// 市场版本号，如 "616.92"
    pub marketing: String,
    pub version: (u32, u32),
    /// 驱动安装时写的原始版本字符串，如 "32.0.16.1692"
    pub windows: Option<String>,
    /// 版本号是从哪读来的，界面上会标出来
    pub source: &'static str,
}

impl DriverInfo {
    /// 低于建议版本：帧生成可能不生效
    pub fn too_old(&self) -> bool {
        self.version < MIN_FG_DRIVER
    }
}

/// 调 nvidia-smi 问驱动自己。比读注册表权威，但要起一个进程（实测约 35ms）。
fn nvidia_smi_version() -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=driver_version", "--format=csv,noheader"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_owned)
}

/// 问 nvidia-smi 要**型号名**。
///
/// 这个比读注册表权威得多：注册表里可能同时存在好几条 NVIDIA 名字的条目
/// （包括旧显卡留下的幽灵条目、或者被别的工具改过的值），而 nvidia-smi 是驱动
/// 自己在跑的那块卡上直接报的。显卡识别的第一优先级就是它。
pub fn nvidia_smi_gpu_name() -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_owned)
}

// ---------------------------------------------------------------- 适配器枚举

#[derive(Debug, Clone)]
pub struct GpuAdapter {
    /// 类键下的实例名，如 "0000"
    pub class_sub: String,
    /// 驱动记录的名称，如 "NVIDIA GeForce RTX 3070"。本程序的路由判断依据。
    pub driver_name: String,
    /// 驱动版本字符串，如 "32.0.16.1692"
    pub driver_version: String,
    /// Enum 实例键（相对 HKLM），唯一确定要改哪一个值
    pub enum_key: String,
    /// 硬件 ID，如 PCI\VEN_10DE&DEV_2484&SUBSYS_...
    pub hardware_id: String,
    /// Enum 键当前的 DeviceDesc
    pub device_desc: Option<String>,
}

/// DeviceDesc 可能是 \`@oem19.inf,%nvidia_dev.2484%;NVIDIA GeForce RTX 3070\`
/// 这种「间接字符串」，真正显示的是最后一个分号后面的部分。
pub fn display_name(raw: &str) -> &str {
    raw.rsplit_once(';').map(|(_, b)| b).unwrap_or(raw).trim()
}

impl GpuAdapter {
    /// 当前实际显示出来的显卡名
    pub fn current_name(&self) -> Option<String> {
        self.device_desc
            .as_deref()
            .map(display_name)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    /// 名字是不是被改过（和驱动记录的不一致）
    pub fn spoofed(&self) -> bool {
        match self.current_name() {
            Some(n) => !n.eq_ignore_ascii_case(&self.driver_name),
            None => false,
        }
    }
}

/// 枚举所有「NVIDIA PCI 显示适配器」。全程只读。
pub fn enumerate() -> Vec<GpuAdapter> {
    let mut out = Vec::new();
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);

    // 1) 类键下的实例：拿驱动记录的名称和版本，并记住它是哪个实例名
    let Ok(class) = hklm.open_subkey(CLASS_KEY) else {
        return out;
    };
    let mut class_map: Vec<(String, String, String)> = Vec::new();
    for sub in class.enum_keys().flatten() {
        if sub.len() != 4 || !sub.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(k) = class.open_subkey(&sub) else {
            continue;
        };
        let desc: String = k.get_value("DriverDesc").unwrap_or_default();
        // 只认 NVIDIA：本工具对它没有意义
        if !desc.to_ascii_uppercase().contains("NVIDIA") {
            continue;
        }
        let mid: String = k.get_value("MatchingDeviceId").unwrap_or_default();
        // 虚拟显示器（Root\...）不是 PCI 设备，改它没有意义
        if !mid.to_ascii_lowercase().starts_with("pci\\") {
            continue;
        }
        let ver: String = k.get_value("DriverVersion").unwrap_or_default();
        class_map.push((sub, desc.trim().to_owned(), ver.trim().to_owned()));
    }
    if class_map.is_empty() {
        return out;
    }

    // 2) 到 Enum\PCI 里找对应实例。用实例的 Driver 值反查类键实例名，
    //    再用 ClassGUID + Service 双重确认这是 NVIDIA 显示适配器。
    let Ok(pci) = hklm.open_subkey(ENUM_PCI) else {
        return out;
    };
    for dev in pci.enum_keys().flatten() {
        if !dev.to_ascii_uppercase().starts_with("VEN_10DE") {
            continue;
        }
        let Ok(devk) = pci.open_subkey(&dev) else {
            continue;
        };
        for inst in devk.enum_keys().flatten() {
            let Ok(ik) = devk.open_subkey(&inst) else {
                continue;
            };
            // 音频控制器等会被这一条挡掉
            let guid: String = ik.get_value("ClassGUID").unwrap_or_default();
            if !guid.eq_ignore_ascii_case(DISPLAY_CLASS) {
                continue;
            }
            let svc: String = ik.get_value("Service").unwrap_or_default();
            if !svc.eq_ignore_ascii_case(NVIDIA_SERVICE) {
                continue;
            }
            // Driver 形如 {4d36e968-...}\0000，最后一段就是类键实例名
            let drv: String = ik.get_value("Driver").unwrap_or_default();
            let Some(sub) = drv.rsplit('\\').next().filter(|s| !s.is_empty()) else {
                continue;
            };
            let Some((_, desc, ver)) = class_map.iter().find(|(s, _, _)| s == sub) else {
                continue;
            };
            let hwids: Vec<String> = ik.get_value("HardwareID").unwrap_or_default();
            out.push(GpuAdapter {
                class_sub: sub.to_owned(),
                driver_name: desc.clone(),
                driver_version: ver.clone(),
                enum_key: format!("{ENUM_PCI}\\{dev}\\{inst}"),
                hardware_id: hwids.first().cloned().unwrap_or_default(),
                device_desc: ik.get_value("DeviceDesc").ok(),
            });
        }
    }
    out
}

/// 挑出要展示/操作的那块显卡：优先 NVIDIA 独显。
pub fn primary(adapters: &[GpuAdapter]) -> Option<&GpuAdapter> {
    adapters
        .iter()
        .find(|a| a.driver_name.to_ascii_uppercase().contains("NVIDIA"))
        .or_else(|| adapters.first())
}

/// 读驱动版本。nvidia-smi 优先（驱动自报，最权威），注册表兜底（零耗时）。
pub fn detect_driver(adapters: &[GpuAdapter]) -> Option<DriverInfo> {
    let windows: Option<String> = primary(adapters)
        .map(|a| a.driver_version.clone())
        .filter(|s| !s.is_empty());

    if let Some(raw) = nvidia_smi_version() {
        if let Some(v) = parse_marketing_version(&raw) {
            return Some(DriverInfo {
                marketing: fmt_version(v),
                version: v,
                windows,
                source: "nvidia-smi",
            });
        }
    }
    let v = parse_windows_version(windows.as_deref()?)?;
    Some(DriverInfo {
        marketing: fmt_version(v),
        version: v,
        windows,
        source: "注册表",
    })
}

// ------------------------------------------------- 「硬件加速 GPU 计划」

/// 「硬件加速 GPU 计划」的当前状态。
///
/// 注册表里**只有一个地方**记它：`HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers`
/// 的 `HwSchMode`（2 = 开，1 = 关）。但这个值**可能根本不存在** —— Windows 11 默认
/// 就是开启，而系统不一定往注册表里写这一项（实测本机 Win11 26300 实际开着、值却不存在，
/// 全注册表扫了一遍也没有第二个地方记状态）。所以读不到时绝不能当成「关着」，
/// 只能老实报「未知」，让用户点「去设置」自己看。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HagsState {
    Enabled,
    Disabled,
    Unknown,
}

impl HagsState {
    pub fn label(self) -> &'static str {
        match self {
            HagsState::Enabled => "已开启",
            HagsState::Disabled => "已关闭",
            HagsState::Unknown => "未知",
        }
    }
}

/// 把注册表里读到的值翻译成状态。抽成纯函数是为了能单独测。
pub fn hags_from_value(v: Option<u32>) -> HagsState {
    match v {
        Some(2) => HagsState::Enabled,
        Some(1) => HagsState::Disabled,
        // 0、3 之类的值没见过 —— 按未知处理，别瞎猜
        _ => HagsState::Unknown,
    }
}

const HAGS_KEY: &str = "SYSTEM\\CurrentControlSet\\Control\\GraphicsDrivers";

/// 读当前状态。**不需要管理员权限。**
pub fn hags_state() -> HagsState {
    let v = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(HAGS_KEY)
        .ok()
        .and_then(|k| k.get_value::<u32, _>("HwSchMode").ok());
    hags_from_value(v)
}

/// 当前系统构建号。读不到返回 None（那就按 Win10 处理）。
///
/// 注意别和 `DriverInfo::windows` 搞混 —— 那个是**驱动**的 Windows 格式版本号
/// （32.0.16.1692 这种），不是系统版本。
pub fn windows_build() -> Option<u32> {
    let k = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion")
        .ok()?;
    let s: String = k.get_value("CurrentBuildNumber").ok()?;
    s.trim().parse().ok()
}

/// Win11 是 22000 起步。
pub fn is_windows_11() -> bool {
    windows_build().map(|b| b >= 22000).unwrap_or(false)
}

/// 「硬件加速 GPU 计划」那一页在设置里的 URI。见微软官方 ms-settings 列表：
///   Win11 = 系统 > 显示 > 图形 > **更改默认图形设置**（开关在这一页上）
///   Win10 = 系统 > 显示 > **图形设置**，开关直接就在页面上
pub fn hags_settings_uri() -> &'static str {
    if is_windows_11() {
        "ms-settings:display-advancedgraphics-default"
    } else {
        "ms-settings:display-advancedgraphics"
    }
}

/// 打开系统设置里的那一页，让用户自己看/改。
/// 走 ShellExecute（和打开网址同一套），ms-settings 协议由它处理；不写任何注册表。
pub fn open_hags_settings() -> Result<()> {
    crate::util::open_url(hags_settings_uri())
}

// ---------------------------------------------------------------- 备份

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupEntry {
    /// Enum 实例键（相对 HKLM）
    pub key: String,
    /// 备份时这个值是否存在
    pub existed_before: bool,
    /// 原始值。existed_before 为 false 时是 None。
    pub original: Option<String>,
    /// 本工具写进去的值
    pub applied: String,
    /// 驱动记录的名称，用作兜底还原
    pub driver_name: String,
    pub saved_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backup {
    pub saved_at: String,
    pub entries: Vec<BackupEntry>,
}

/// 备份放在 %APPDATA%，不放在 exe 同级 —— 免得用户换个体积包里就找不到还原依据。
pub fn backup_path() -> Result<PathBuf> {
    let d = crate::util::backups_dir()?.join("gpu-name");
    std::fs::create_dir_all(&d)?;
    Ok(d.join("manifest.json"))
}

pub fn load_backup() -> Option<Backup> {
    let p = backup_path().ok()?;
    let t = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&t).ok()
}

fn write_backup(b: &Backup) -> Result<()> {
    let p = backup_path()?;
    std::fs::write(&p, serde_json::to_string_pretty(b)?)
        .with_context(|| format!("写入备份失败: {}", p.display()))?;
    Ok(())
}

/// 备份里有没有这块显卡的原始值
pub fn has_backup_for(a: &GpuAdapter) -> bool {
    load_backup()
        .map(|b| b.entries.iter().any(|e| e.key == a.enum_key))
        .unwrap_or(false)
}

// ---------------------------------------------------------------- 写入 / 还原

fn open_rw(key: &str) -> Result<RegKey> {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(key, KEY_READ | KEY_WRITE)
        .with_context(|| {
            format!("打开注册表键失败（这一步需要管理员权限）: HKLM\\{key}")
        })
}

/// 把显卡名改成 \`new_name\`。调用方必须已经拿到用户对副作用说明的确认。
pub fn apply(a: &GpuAdapter, new_name: &str) -> Result<String> {
    if !PRESETS.contains(&new_name) {
        bail!("型号「{new_name}」不在允许的名单里，已拒绝写入（防止填错型号）");
    }

    let key = open_rw(&a.enum_key)?;
    let original: Option<String> = key.get_value("DeviceDesc").ok();

    // ---- 备份。已经备份过就保留最早的原始值，绝不用当前值覆盖它。
    let mut bk = load_backup().unwrap_or(Backup {
        saved_at: crate::util::now_utc(),
        entries: Vec::new(),
    });
    let now = crate::util::now_utc();
    match bk.entries.iter_mut().find(|e| e.key == a.enum_key) {
        Some(e) => e.applied = new_name.to_owned(),
        None => bk.entries.push(BackupEntry {
            key: a.enum_key.clone(),
            existed_before: original.is_some(),
            original: original.clone(),
            applied: new_name.to_owned(),
            driver_name: a.driver_name.clone(),
            saved_at: now.clone(),
        }),
    }
    bk.saved_at = now;
    write_backup(&bk)?;

    // 读回校验：备份读不回来就绝不往下写
    if !has_backup_for(a) {
        bail!("备份文件读回校验失败，已中止（没有改动任何注册表值）");
    }

    // ---- 写入
    key.set_value("DeviceDesc", &new_name.to_owned())
        .context("写入 DeviceDesc 失败")?;

    // 读回校验。不一致就把原值写回去。
    let after: String = key.get_value("DeviceDesc").unwrap_or_default();
    if after != new_name {
        if let Some(o) = &original {
            let _ = key.set_value("DeviceDesc", o);
        }
        bail!("写入后读回不一致（实际为「{after}」），已尝试回滚");
    }

    let from = a
        .current_name()
        .unwrap_or_else(|| a.driver_name.clone());
    Ok(format!(
        "已把显卡名从「{from}」改为「{new_name}」。重启后生效。"
    ))
}

/// 还原到哪个值。两种目标差别很大，界面上必须让用户自己选。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreTo {
    /// 驱动自己记录的名称。选这个等于彻底去掉伪装。
    DriverName,
    /// 本工具第一次改动之前的值。可能本身就是别的工具改过的值。
    BackupOriginal,
}

/// 备份里记录的「改动前的值」。没有备份、或原本该值不存在时返回 None。
pub fn backup_original_of(a: &GpuAdapter) -> Option<String> {
    let b = load_backup()?;
    let e = b.entries.iter().find(|x| x.key == a.enum_key)?;
    if e.existed_before {
        e.original.clone()
    } else {
        None
    }
}

fn write_desc(a: &GpuAdapter, target: &str) -> Result<()> {
    let key = open_rw(&a.enum_key)?;
    key.set_value("DeviceDesc", &target.to_owned())
        .context("写入 DeviceDesc 失败")?;
    let after: String = key.get_value("DeviceDesc").unwrap_or_default();
    if after != target {
        bail!("写入后读回不一致（实际为「{after}」）");
    }
    Ok(())
}

/// 还原。
pub fn restore(a: &GpuAdapter, to: RestoreTo) -> Result<String> {
    let target = match to {
        RestoreTo::DriverName => a.driver_name.clone(),
        RestoreTo::BackupOriginal => match backup_original_of(a) {
            Some(v) => v,
            None => bail!(
                "没有可用的备份原始值（记录显示该值原本不存在）。\
                 可以改用「还原为驱动记录的名称」，或在设备管理器里卸载显卡后重新扫描硬件。"
            ),
        },
    };
    if target.trim().is_empty() {
        bail!("目标值是空的，已中止");
    }
    write_desc(a, &target)?;
    Ok(format!(
        "已把显卡名还原为「{}」。重启后生效。",
        display_name(&target)
    ))
}

// ---------------------------------------------------------------- 提权执行

#[derive(Debug, Clone)]
pub enum Op {
    Apply(String),
    Restore(RestoreTo),
}

/// 结果文件专门带进程号和时间戳，避免和上次残留的混在一起。
fn result_file() -> PathBuf {
    let pid = std::process::id();
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("framegen-gpuop-{pid}-{ms}.json"))
}

/// 用 ShellExecuteW("runas") 重新拉起自己，这时候会弹一次 UAC。
/// 子进程干完活把 JSON 结果写进临时文件后直接退出，主程序轮询读它。
pub fn run_elevated(exe: &Path, op: &Op) -> Result<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let out = result_file();
    let _ = std::fs::remove_file(&out);

    let args = match op {
        Op::Apply(name) => format!("--gpuspoof-apply \"{name}\" \"{}\"", out.display()),
        Op::Restore(to) => format!(
            "--gpuspoof-restore {} \"{}\"",
            match to {
                RestoreTo::DriverName => "driver",
                RestoreTo::BackupOriginal => "backup",
            },
            out.display()
        ),
    };

    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let file: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let verb = wide("runas");
    let params = wide(&args);

    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 的返回值 <= 32 表示失败（这是历史遗留的约定）
    if r as isize <= 32 {
        bail!(
            "没能启动管理员进程（ShellExecuteW 返回 {}）。\
             如果在 UAC 弹窗上点了「否」，就属于这种情况。",
            r as isize
        );
    }

    // 等子进程把结果写出来。用户可能正在看 UAC 弹窗，给足时间。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    while std::time::Instant::now() < deadline {
        if let Ok(t) = std::fs::read_to_string(&out) {
            let _ = std::fs::remove_file(&out);
            let v: serde_json::Value =
                serde_json::from_str(&t).context("子进程返回的内容无法解析")?;
            let ok = v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false);
            let msg = v
                .get("msg")
                .and_then(|s| s.as_str())
                .unwrap_or("(无消息)")
                .to_owned();
            return if ok { Ok(msg) } else { bail!("{msg}") };
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    bail!("等待管理员操作超时，没有收到结果文件。")
}
