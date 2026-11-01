//! Smooth Motion（30 系，实验性）—— 适用性判定与只读探测。
//!
//! 产品规定：**驱动 >= 616.64 才允许部署**，低于该版本一律不允许
//! （见 docs/SM-事实来源与策略.md；该门槛是本项目的决定，不是上游/NVIDIA 的要求）。
//!
//! 写入路径只动**绑定到某个游戏 EXE 的 profile**（NVIDIA 预置的那一个，或者
//! 没找到时我们自己建的 "FrameGen Manager - 游戏名"），绝不写全局/基础 profile；
//! 写前把旧值记进备份记录，关掉/还原时按记录写回。

use crate::gpu;

/// 允许部署 SM 的最低驱动版本（硬门槛，产品规定）。
pub const MIN_SM_DRIVER: (u32, u32) = (616, 64);
pub const MIN_SM_DRIVER_TEXT: &str = "616.64";

/// 适用性判定结果（界面直接显示 reason）。
#[derive(Debug, Clone)]
pub struct Gate {
    /// 30 系 / SM86？
    pub is_sm86: bool,
    /// 20 系 / SM75？（本工具新增的支持范围）
    pub is_sm75: bool,
    pub gpu_name: String,
    /// 市场版本号，如 "617.42"；读不到则空串
    pub driver: String,
    pub version: Option<(u32, u32)>,
    /// 版本号是从哪读来的
    pub source: &'static str,
    /// 是否允许部署
    pub allowed: bool,
    /// 不允许的原因（允许时为空）
    pub reason: String,
}

/// 按产品规定判定能不能部署 SM（读本机实际信息）。
pub fn gate() -> Gate {
    let gpu_name = crate::scan::detect_gpu().unwrap_or_default();
    let route = crate::scan::classify_gpu(&gpu_name);
    let is_sm86 = matches!(route, crate::scan::GpuRoute::Sm86);
    let is_sm75 = matches!(route, crate::scan::GpuRoute::Sm75);
    let raw = gpu::nvidia_smi_version();
    gate_for(raw.as_deref(), is_sm86, is_sm75, &gpu_name)
}

/// 纯判定版本（不碰系统），自测与界面预览都用它。
pub fn gate_for(driver_raw: Option<&str>, is_sm86: bool, is_sm75: bool, gpu_name: &str) -> Gate {
    let (driver, version) = match driver_raw {
        Some(s) => (s.trim().to_owned(), gpu::parse_marketing_version(s)),
        None => (String::new(), None),
    };
    let mut allowed = true;
    let mut reason = String::new();
    if !is_sm86 && !is_sm75 {
        allowed = false;
        reason = format!(
            "当前显卡「{}」不是 20/30 系 —— 这个解锁只对 20/30 系有效。",
            if gpu_name.is_empty() {
                "未知"
            } else {
                gpu_name
            }
        );
    } else if version.is_none() {
        allowed = false;
        reason = "读不到 NVIDIA 驱动版本（需要已安装的 NVIDIA 驱动与 nvidia-smi）。".to_owned();
    } else if version.unwrap() < MIN_SM_DRIVER {
        allowed = false;
        reason = format!(
            "驱动 {} 低于本工具要求的最低版本 {} —— 按产品规定不允许部署。",
            driver, MIN_SM_DRIVER_TEXT
        );
    }
    Gate {
        is_sm86,
        is_sm75,
        gpu_name: gpu_name.to_owned(),
        driver,
        version,
        source: "nvidia-smi --query-gpu=driver_version",
        allowed,
        reason,
    }
}

/// 一个 NvPresent 文件（驱动里真正实现呈现层插帧的组件；解锁是否生效由它的构建决定）。
#[derive(Debug, Clone)]
pub struct NvPresent {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    /// 是否在本工具的"已识别构建"表里
    pub recognized: bool,
    pub note: String,
}

/// 上游报告的"已识别构建"：(驱动版本, 字节数, sha256, 说明)。
///
/// 只有列在这里的构建才会被标成"已验证/实验"；其余标为"未被识别"。
/// **注意**：这张表**不拦部署** —— 是否允许部署由上面的驱动门槛决定；
/// 这里只影响界面的如实标注与"未被识别 → 二次确认"。
pub const KNOWN_BUILDS: &[(&str, u64, &str, &str)] = &[(
    "616.64",
    8_417_512,
    "f36a5f10458a9b67833df97ea2a4634d8c03b634d8edcbfbd9e49bc47ad8b54e",
    "上游报告的黄金构建（PROJECT_VALIDATED；我方尚未实机核对）",
)];

/// 探测本机所有 NvPresent 构建（只读；按字节数倒序，最大的通常是当前驱动那份）。
pub fn nvpresent_all() -> Vec<NvPresent> {
    let mut out: Vec<NvPresent> = Vec::new();
    let Some(root) = std::env::var_os("SystemRoot") else {
        return out;
    };
    let store = std::path::Path::new(&root)
        .join("System32")
        .join("DriverStore")
        .join("FileRepository");
    let Ok(rd) = std::fs::read_dir(&store) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        if !name.starts_with("nv_dispi.inf_") {
            continue;
        }
        for file in ["NvPresent64.dll", "NvPresent.dll"] {
            let p = e.path().join(file);
            if !p.is_file() {
                continue;
            }
            let bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            let sha256 = crate::util::sha256_file(&p).unwrap_or_default();
            let hit = KNOWN_BUILDS
                .iter()
                .find(|(_, b, h, _)| *b == bytes && h.eq_ignore_ascii_case(&sha256));
            out.push(NvPresent {
                path: p.display().to_string(),
                bytes,
                sha256,
                recognized: hit.is_some(),
                note: hit
                    .map(|(d, _, _, n)| format!("对应驱动 {d}：{n}"))
                    .unwrap_or_else(|| {
                        "这个构建不在本工具的已识别清单里（上游可能尚未适配该驱动）".to_owned()
                    }),
            });
        }
    }
    out.sort_by_key(|x| std::cmp::Reverse(x.bytes));
    out
}

// ---------------------------------------------------------------- NVAPI（只读）

/// 驱动 profile 里某个设置项的读取结果。
#[derive(Debug, Clone)]
pub struct SettingState {
    /// 未设置 / 已开启 / 已关闭 / 读取失败
    pub state: &'static str,
    pub value: Option<u32>,
    /// 出错时的原始信息（只进日志与诊断，不上界面）
    pub detail: String,
}

/// Smooth Motion 开关这一项在驱动里的设置 ID（来源：Profile Inspector 的
/// CustomSettingNames.xml，0=Off / 1=On）。见工作区里的 SM 事实来源记录。
pub const SM_SETTING_ID: u32 = 0xB0D3_84C0;

/// 读**基础/全局 profile**里的 SM 开关（只读；本工具**绝不写**这一层）。
pub fn base_sm_state() -> SettingState {
    let sess = match nv::Session::open() {
        Ok(s) => s,
        Err(e) => {
            return SettingState {
                state: "读取失败",
                value: None,
                detail: e,
            }
        }
    };
    let prof = match sess.base() {
        Ok(p) => p,
        Err(e) => {
            return SettingState {
                state: "读取失败",
                value: None,
                detail: e,
            }
        }
    };
    state_from_read(sess.read(prof, SM_SETTING_ID))
}

fn state_from_read(r: Result<u32, String>) -> SettingState {
    match r {
        Ok(1) => SettingState {
            state: "已开启",
            value: Some(1),
            detail: String::new(),
        },
        Ok(0) => SettingState {
            state: "已关闭",
            value: Some(0),
            detail: String::new(),
        },
        Ok(v) => SettingState {
            state: "未设置",
            value: Some(v),
            detail: String::new(),
        },
        Err(e) if e.starts_with("rc=-160") => SettingState {
            state: "未设置",
            value: None,
            detail: e,
        },
        Err(e) => SettingState {
            state: "读取失败",
            value: None,
            detail: e,
        },
    }
}

/// 某个游戏 EXE 在驱动里对应的 SM 状态（只读）。
#[derive(Debug, Clone)]
pub struct ExeProfileState {
    /// 驱动里有没有挂这个 EXE 的 profile
    pub found: bool,
    /// 命中的 profile 里登记的应用名（诊断用）
    pub matched: Vec<String>,
    pub state: SettingState,
    pub detail: String,
    /// 诊断：枚举到多少个 profile、第一个 profile 的应用名样例、各步返回码
    pub profile_count: usize,
    pub sample_apps: Vec<String>,
    pub diag: Vec<String>,
}

/// 按 EXE 读 SM 开关。匹配规则：profile 登记的应用名与 EXE 的**文件名**一致
/// （忽略大小写）—— NVIDIA 的 profile 里有的存完整路径、有的只存文件名。
pub fn exe_sm_state(exe: &std::path::Path) -> ExeProfileState {
    let want = exe
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let mut out = ExeProfileState {
        found: false,
        matched: Vec::new(),
        state: SettingState {
            state: "未设置",
            value: None,
            detail: String::new(),
        },
        detail: String::new(),
        profile_count: 0,
        sample_apps: Vec::new(),
        diag: Vec::new(),
    };
    if want.is_empty() {
        out.state = SettingState {
            state: "读取失败",
            value: None,
            detail: "没有给 EXE 路径".to_owned(),
        };
        out.detail = "没有给 EXE 路径".to_owned();
        return out;
    }
    let sess = match nv::Session::open() {
        Ok(s) => s,
        Err(e) => {
            out.state = SettingState {
                state: "读取失败",
                value: None,
                detail: e.clone(),
            };
            out.detail = e;
            return out;
        }
    };
    let profs = sess.profiles();
    out.profile_count = profs.len();
    out.diag.push(format!("profiles={}", profs.len()));
    for (idx, prof) in profs.into_iter().enumerate() {
        let apps = sess.apps(prof);
        if idx == 0 {
            out.sample_apps = apps.clone();
            out.diag.push(format!("apps(first profile)={}", apps.len()));
        }
        let hit = apps.iter().any(|a| {
            std::path::Path::new(a)
                .file_name()
                .map(|f| f.to_string_lossy().to_ascii_lowercase() == want)
                .unwrap_or(false)
        });
        if !hit {
            continue;
        }
        out.found = true;
        out.matched = apps;
        out.state = state_from_read(sess.read(prof, SM_SETTING_ID));
        break;
    }
    if !out.found {
        out.detail = "驱动里没有挂这个 EXE 的 profile（还没为它建过驱动配置）".to_owned();
        out.diag.push(format!(
            "scanned={} profiles, exe={}",
            out.profile_count, want
        ));
    }
    out
}

/// NVAPI 动态调用（只读）。不链接 import 库 —— nvapi64.dll 随 NVIDIA 驱动安装。
#[cfg(windows)]
mod nv {
    use std::ffi::c_void;

    const ID_INIT: u32 = 0x0150_E828;
    const ID_CREATE_SESSION: u32 = 0x0694_D52E;
    const ID_LOAD_SETTINGS: u32 = 0x375D_BD6B;
    const ID_GET_BASE_PROFILE: u32 = 0xDA84_66A0;
    const ID_FIND_PROFILE: u32 = 0x7E4A_9A0B;
    const ID_CREATE_PROFILE: u32 = 0xCC17_6068;
    const ID_CREATE_APPLICATION: u32 = 0x4347_A9DE;
    /// 写设置：**必须用首选 ID**。实测回退 ID 0x577DD202 对 Smooth Motion 返回 -160，
    /// 而首选 ID 返回 0 —— 这就是之前那个误导性 -160 的真凶。
    const ID_SET_SETTING: u32 = 0x8A2C_F5F5;
    const ID_SAVE_SETTINGS: u32 = 0xFCBC_7E14;
    const ID_DELETE_PROFILE: u32 = 0x1709_3206;
    /// 问驱动：这个 exe 属于哪个 profile（PI 也是用它定位游戏配置的）。
    const ID_FIND_APPLICATION_BY_NAME: u32 = 0xEEE5_66B2;
    /// 删掉某个 profile 里的单项设置（PI 的"恢复默认"就是用它，而不是写 0）。
    /// 和写设置一样，用首选 ID（与 PI 的 GetDelegate 调用顺序一致）。
    const ID_DELETE_PROFILE_SETTING: u32 = 0xD20D_29DF;
    /// 读设置：优先用 PI 的首选 ID（它和回退 ID 的参数个数不同，这里必须用首选）
    const ID_GET_SETTING: u32 = 0xEA99_498D;

    /// NVDRS_SETTING：**12320 字节**，与 Profile Inspector 的声明逐字段对齐
    /// （version 低位放结构体大小、高位放版本号：size | (1 << 16)）。
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NvSetting {
        version: u32,
        /// 设置名（2048 个 wchar，内联；这里用不到但必须占位，否则偏移全错）
        setting_name: [u16; 2048],
        setting_id: u32,
        setting_type: u32,
        setting_location: u32,
        is_current_predefined: u32,
        is_predefined_valid: u32,
        predefined: [u8; 4100],
        current: [u8; 4100],
    }

    impl NvSetting {
        fn new(id: u32) -> NvSetting {
            let mut st: NvSetting = unsafe { std::mem::zeroed() };
            st.version = (std::mem::size_of::<NvSetting>() as u32) | (1 << 16);
            st.setting_id = id;
            // 名字必须填。Profile Inspector 写进驱动的就是这串固定占位串
            // （从 PI 写好的 CS2 profile 里读出来的原文正是 PLACEHOLDER）。
            for (i, c) in "PLACEHOLDER".encode_utf16().enumerate() {
                st.setting_name[i] = c;
            }
            st
        }

        fn value(&self) -> u32 {
            u32::from_le_bytes([
                self.current[0],
                self.current[1],
                self.current[2],
                self.current[3],
            ])
        }
    }

    type FnQuery = unsafe extern "C" fn(u32) -> *mut c_void;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    // ---- 按 EXE 定位 profile（只读）----

    const ID_ENUM_PROFILES: u32 = 0xBC37_1EE0;
    const ID_ENUM_APPLICATIONS: u32 = 0x7FA2_173A;

    fn wide_to_string(buf: &[u16]) -> String {
        let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// 一次 NVAPI 会话（只读使用）。
    pub struct Session {
        sess: *mut c_void,
        query: FnQuery,
    }

    impl Session {
        pub fn open() -> Result<Session, String> {
            use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
            unsafe {
                let lib = LoadLibraryW(wide("nvapi64.dll").as_ptr());
                if lib.is_null() {
                    return Err("加载 nvapi64.dll 失败（没装 NVIDIA 驱动？）".to_owned());
                }
                let Some(p) = GetProcAddress(lib, c"nvapi_QueryInterface".as_ptr() as *const u8)
                else {
                    return Err("nvapi64.dll 里没有 nvapi_QueryInterface".to_owned());
                };
                let query: FnQuery = std::mem::transmute(p);
                let init: unsafe extern "C" fn() -> i32 = std::mem::transmute(query(ID_INIT));
                let rc = init();
                if rc != 0 {
                    return Err(format!("NvAPI_Initialize rc={rc}"));
                }
                let mut sess: *mut c_void = std::ptr::null_mut();
                let create: unsafe extern "C" fn(*mut *mut c_void) -> i32 =
                    std::mem::transmute(query(ID_CREATE_SESSION));
                let rc = create(&mut sess);
                if rc != 0 {
                    return Err(format!("DRS_CreateSession rc={rc}"));
                }
                let load: unsafe extern "C" fn(*mut c_void) -> i32 =
                    std::mem::transmute(query(ID_LOAD_SETTINGS));
                let rc = load(sess);
                if rc != 0 {
                    return Err(format!("DRS_LoadSettings rc={rc}"));
                }
                Ok(Session { sess, query })
            }
        }

        pub(crate) fn base(&self) -> Result<*mut c_void, String> {
            unsafe {
                let mut prof: *mut c_void = std::ptr::null_mut();
                let f: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> i32 =
                    std::mem::transmute((self.query)(ID_GET_BASE_PROFILE));
                let rc = f(self.sess, &mut prof);
                if rc != 0 {
                    return Err(format!("DRS_GetBaseProfile rc={rc}"));
                }
                Ok(prof)
            }
        }

        /// 枚举所有 profile 的句柄。
        ///
        /// 上限给得很宽（20000）：NVIDIA 自带的 profile 有一千多个，默认驱动装完就容易
        /// 超过 512 —— 卡在 512 会让排在后面的自建 profile 直接"不存在"（枚举不到就
        /// 清不掉，也查不到）。真正的结束条件是驱动返回错误码，或在驱动不报结束时
        /// 吐出重复句柄（见下面的去重）。
        pub fn profiles(&self) -> Vec<*mut c_void> {
            let mut out = Vec::new();
            unsafe {
                let raw = (self.query)(ID_ENUM_PROFILES);
                if raw.is_null() {
                    crate::log::line("[sm] nvapi_QueryInterface(EnumProfiles) 返回空");
                    return out;
                }
                let f: unsafe extern "C" fn(*mut c_void, u32, *mut *mut c_void) -> i32 =
                    std::mem::transmute(raw);
                let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
                for i in 0..20_000u32 {
                    let mut prof: *mut c_void = std::ptr::null_mut();
                    let rc = f(self.sess, i, &mut prof);
                    if rc != 0 || prof.is_null() {
                        if i == 0 {
                            crate::log::line(&format!("[sm] EnumProfiles(0) rc={rc}"));
                        }
                        break;
                    }
                    // 有的驱动/接口版本不会返回 END_ENUMERATION，而是把同一个句柄一直吐出来：
                    // 句柄重复即视为枚举结束（否则会白跑几千次 API 调用）。
                    if !seen.insert(prof as usize) {
                        break;
                    }
                    out.push(prof);
                }
            }
            out
        }

        /// 取某个 profile 下挂的所有应用程序名（通常是 exe 的完整路径）。
        ///
        /// 注意签名：NVAPI 的 DRS_EnumApplications 是**5 个参数**
        /// （hSession, hProfile, startIndex, **ref appCount**, pApps）——
        /// 少传那个 count 会读进垃圾参数并让本进程崩溃（我们踩过）。
        /// 结构体用 NVDRS_APPLICATION_V4 的布局（version=0x500C0004 即 size<<16|4），
        /// 并用一整块足够大的缓冲区，避免驱动写越界。
        pub fn apps(&self, prof: *mut c_void) -> Vec<String> {
            const SLOTS: usize = 32;
            const APP_SIZE: usize = 20492; // V4: 4+4+5*4096+4
            let mut buf = vec![0u8; SLOTS * APP_SIZE];
            let ver: u32 = ((APP_SIZE as u32) << 16) | 4;
            for i in 0..SLOTS {
                let off = i * APP_SIZE;
                buf[off..off + 4].copy_from_slice(&ver.to_le_bytes());
            }
            let mut count: u32 = SLOTS as u32;
            unsafe {
                let raw = (self.query)(ID_ENUM_APPLICATIONS);
                if raw.is_null() {
                    return Vec::new();
                }
                let f: unsafe extern "C" fn(
                    *mut c_void,
                    *mut c_void,
                    u32,
                    *mut u32,
                    *mut c_void,
                ) -> i32 = std::mem::transmute(raw);
                let rc = f(
                    self.sess,
                    prof,
                    0,
                    &mut count,
                    buf.as_mut_ptr() as *mut c_void,
                );
                if rc != 0 {
                    crate::log::line(&format!("[sm] EnumApplications rc={rc} count={count}"));
                    return Vec::new();
                }
            }
            let n = (count as usize).min(SLOTS);
            let mut out: Vec<String> = Vec::new();
            // 一个应用条目里 4 个名字字段都可能用来标识游戏：
            //   appName(偏移 8) / userFriendlyName(4104) / launcher(8200) / fileInFolder(12296)
            // NVIDIA 的预置 profile 有时只填 launcher 或 fileInFolder，所以四个都要看。
            const NAME_OFFSETS: [usize; 4] = [8, 4104, 8200, 12296];
            for i in 0..n {
                let base = i * APP_SIZE;
                for off in NAME_OFFSETS {
                    let start = base + off;
                    let mut name_buf = [0u16; 2048];
                    for (k, slot) in name_buf.iter_mut().enumerate() {
                        let p = start + k * 2;
                        if p + 1 >= buf.len() {
                            break;
                        }
                        *slot = u16::from_le_bytes([buf[p], buf[p + 1]]);
                    }
                    let name = wide_to_string(&name_buf);
                    if !name.is_empty() {
                        out.push(name);
                    }
                }
            }
            out
        }

        /// 问驱动：这个 exe 属于哪个 profile？找不到返回 Ok(None)。
        ///
        /// 为什么必须用它：真实游戏几乎都已经属于某个 NVIDIA 预置 profile，
        /// 此时 CreateApplication 会返回 -167（exe 已被占用），我们自建的 profile
        /// 根本挂不上游戏 —— 设置写进去也不会生效。
        pub fn find_profile_for_exe(
            &self,
            exe: &std::path::Path,
        ) -> Result<Option<*mut c_void>, String> {
            unsafe {
                let raw = (self.query)(ID_FIND_APPLICATION_BY_NAME);
                if raw.is_null() {
                    return Err("驱动没有提供 DRS_FindApplicationByName".to_owned());
                }
                let f: unsafe extern "C" fn(
                    *mut c_void,
                    *const u16,
                    *mut *mut c_void,
                    *mut c_void,
                ) -> i32 = std::mem::transmute(raw);
                let w: Vec<u16> = exe
                    .display()
                    .to_string()
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let mut app = vec![0u8; 20492];
                app[0..4].copy_from_slice(&(20492u32 | (4u32 << 16)).to_le_bytes());
                let mut prof: *mut c_void = std::ptr::null_mut();
                let rc = f(
                    self.sess,
                    w.as_ptr(),
                    &mut prof,
                    app.as_mut_ptr() as *mut c_void,
                );
                if rc == 0 && !prof.is_null() {
                    Ok(Some(prof))
                } else {
                    Ok(None)
                }
            }
        }

        /// 读一个 profile 自己的名字（诊断/日志用）。
        pub fn profile_name(&self, prof: *mut c_void) -> String {
            unsafe {
                const ID_PROFILE_INFO: u32 = 0x61CD_6FD6;
                let raw = (self.query)(ID_PROFILE_INFO);
                if raw.is_null() {
                    return String::new();
                }
                let f: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32 =
                    std::mem::transmute(raw);
                let mut info = vec![0u8; 4116];
                info[0..4].copy_from_slice(&(4116u32 | (1u32 << 16)).to_le_bytes());
                let rc = f(self.sess, prof, info.as_mut_ptr() as *mut c_void);
                if rc != 0 {
                    return String::new();
                }
                let mut name = String::new();
                for i in 0..2048 {
                    let o = 4 + i * 2;
                    let c = u16::from_le_bytes([info[o], info[o + 1]]);
                    if c == 0 {
                        break;
                    }
                    name.push(char::from_u32(c as u32).unwrap_or('?'));
                }
                name
            }
        }

        /// 读任意 profile 里的一个 u32 设置项。
        pub fn read(&self, prof: *mut c_void, id: u32) -> Result<u32, String> {
            unsafe {
                let mut st = NvSetting::new(id);
                let raw = (self.query)(ID_GET_SETTING);
                if raw.is_null() {
                    return Err("驱动没有提供 DRS_GetSetting".to_owned());
                }
                // 注意：DRS_GetSetting 实际是【5 个参数】（最后还有一个 ref uint）。
                // 少传会让被调函数读到垃圾参数，表现为「什么都读不到」——我们踩过。
                let f: unsafe extern "C" fn(
                    *mut c_void,
                    *mut c_void,
                    u32,
                    *mut NvSetting,
                    *mut u32,
                ) -> i32 = std::mem::transmute(raw);
                let mut extra: u32 = 0;
                let rc = f(self.sess, prof, id, &mut st, &mut extra);
                if rc != 0 {
                    return Err(format!("rc={rc}"));
                }
                Ok(st.value())
            }
        }

        /// 按名字找 profile。返回 Ok(None) 表示"没有这个 profile"（rc = -163）。
        pub fn find(&self, name: &str) -> Result<Option<*mut c_void>, String> {
            unsafe {
                let raw = (self.query)(ID_FIND_PROFILE);
                if raw.is_null() {
                    return Err("驱动没有提供 DRS_FindProfileByName".to_owned());
                }
                let f: unsafe extern "C" fn(*mut c_void, *const u16, *mut *mut c_void) -> i32 =
                    std::mem::transmute(raw);
                let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
                let mut prof: *mut c_void = std::ptr::null_mut();
                let rc = f(self.sess, w.as_ptr(), &mut prof);
                if rc == 0 {
                    Ok(Some(prof))
                } else if rc == -163 {
                    Ok(None)
                } else {
                    Err(format!("DRS_FindProfileByName rc={rc}"))
                }
            }
        }

        /// 建一个**我们自己的** profile（NVDRS_PROFILE = 4116 字节）。
        pub fn create_profile(&self, name: &str) -> Result<*mut c_void, String> {
            unsafe {
                let raw = (self.query)(ID_CREATE_PROFILE);
                if raw.is_null() {
                    return Err("驱动没有提供 DRS_CreateProfile".to_owned());
                }
                let f: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> i32 =
                    std::mem::transmute(raw);
                let mut buf = vec![0u8; 4116];
                buf[0..4].copy_from_slice(&(4116u32 | (1u32 << 16)).to_le_bytes());
                let w: Vec<u16> = name.encode_utf16().collect();
                for (i, c) in w.iter().enumerate() {
                    let o = 4 + i * 2;
                    if o + 2 > buf.len() {
                        break;
                    }
                    buf[o..o + 2].copy_from_slice(&c.to_le_bytes());
                }
                let mut prof: *mut c_void = std::ptr::null_mut();
                let rc = f(self.sess, buf.as_mut_ptr() as *mut c_void, &mut prof);
                if rc != 0 {
                    return Err(format!("DRS_CreateProfile rc={rc}"));
                }
                Ok(prof)
            }
        }

        /// 把某个 exe 挂到我们的 profile 上（NVDRS_APPLICATION_V4 = 20492 字节）。
        /// rc = -167 表示这个 exe 已经被别的 profile 占了。
        pub fn create_application(
            &self,
            prof: *mut c_void,
            exe: &std::path::Path,
        ) -> Result<(), String> {
            unsafe {
                let raw = (self.query)(ID_CREATE_APPLICATION);
                if raw.is_null() {
                    return Err("驱动没有提供 DRS_CreateApplication".to_owned());
                }
                let f: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32 =
                    std::mem::transmute(raw);
                let mut buf = vec![0u8; 20492];
                buf[0..4].copy_from_slice(&(20492u32 | (4u32 << 16)).to_le_bytes());
                let text = exe.display().to_string();
                let w: Vec<u16> = text.encode_utf16().collect();
                for (i, c) in w.iter().enumerate() {
                    let o = 8 + i * 2;
                    if o + 2 > buf.len() {
                        break;
                    }
                    buf[o..o + 2].copy_from_slice(&c.to_le_bytes());
                }
                let rc = f(self.sess, prof, buf.as_mut_ptr() as *mut c_void);
                if rc != 0 {
                    return Err(format!(
                        "DRS_CreateApplication rc={rc}（-167 = 该 exe 已被别的配置占用）"
                    ));
                }
                Ok(())
            }
        }

        /// 写一个 DWORD 设置。返回驱动的原始返回码 —— **它不可信**
        /// （实测：SM 这一项返回 -160 但值其实写进去了），判成败必须靠读回复核。
        pub fn set_dword(&self, prof: *mut c_void, id: u32, value: u32) -> Result<i32, String> {
            // 最后一道护栏：全局/基础 profile 一律不许写（连句柄都不接）。
            if self.is_base_profile(prof) {
                return Err("拒绝：不允许写全局/基础 profile。".to_owned());
            }
            unsafe {
                let raw = (self.query)(ID_SET_SETTING);
                if raw.is_null() {
                    return Err("驱动没有提供 DRS_SetSetting".to_owned());
                }
                // 同样是 5 个参数（结构体 + 两个 uint）
                let f: unsafe extern "C" fn(
                    *mut c_void,
                    *mut c_void,
                    *mut NvSetting,
                    u32,
                    u32,
                ) -> i32 = std::mem::transmute(raw);
                let mut st = NvSetting::new(id);
                st.current[0..4].copy_from_slice(&value.to_le_bytes());
                Ok(f(self.sess, prof, &mut st, 0, 0))
            }
        }

        pub fn save(&self) -> Result<(), String> {
            unsafe {
                let raw = (self.query)(ID_SAVE_SETTINGS);
                if raw.is_null() {
                    return Err("驱动没有提供 DRS_SaveSettings".to_owned());
                }
                let f: unsafe extern "C" fn(*mut c_void) -> i32 = std::mem::transmute(raw);
                let rc = f(self.sess);
                if rc != 0 {
                    return Err(format!("DRS_SaveSettings rc={rc}"));
                }
                Ok(())
            }
        }

        pub fn delete_profile(&self, prof: *mut c_void) -> Result<(), String> {
            unsafe {
                let raw = (self.query)(ID_DELETE_PROFILE);
                if raw.is_null() {
                    return Err("驱动没有提供 DRS_DeleteProfile".to_owned());
                }
                let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 =
                    std::mem::transmute(raw);
                let rc = f(self.sess, prof);
                if rc != 0 {
                    return Err(format!("DRS_DeleteProfile rc={rc}"));
                }
                Ok(())
            }
        }

        /// 删掉某个 profile 里的一个设置项（= 恢复成"没有覆盖、跟随全局"）。
        pub fn delete_setting(&self, prof: *mut c_void, id: u32) -> Result<i32, String> {
            unsafe {
                let raw = (self.query)(ID_DELETE_PROFILE_SETTING);
                if raw.is_null() {
                    return Err("驱动没有提供 DRS_DeleteProfileSetting".to_owned());
                }
                let f: unsafe extern "C" fn(*mut c_void, *mut c_void, u32) -> i32 =
                    std::mem::transmute(raw);
                Ok(f(self.sess, prof, id))
            }
        }

        /// 这个句柄是不是**全局/基础 profile**？是的话一律拒绝写入（硬护栏）。
        pub fn is_base_profile(&self, prof: *mut c_void) -> bool {
            self.base().is_ok_and(|b| b == prof)
        }
    }
}

// ---------------------------------------------------------------- 写入路径（游戏自己的 profile）

/// 我们自己创建的 profile 名前缀 —— 用来区分「我们的」与 NVIDIA 预置的配置。
pub const PROFILE_PREFIX: &str = "FrameGen Manager - ";

/// 某个游戏 EXE 在驱动里的归属与当前 SM 值（界面状态用它）。
#[derive(Debug, Clone)]
pub struct SmGameState {
    pub exe: String,
    /// 驱动里挂了这个 EXE 的 profile 名字；空串 = 没有
    pub profile: String,
    /// 命中的是**我们建的** profile 吗（只有这种才允许被删掉）
    pub ours: bool,
    /// 该 profile 里 SM 的值；None = 没设置（跟随全局）
    pub value: Option<u32>,
    pub detail: String,
}

/// 一次写入的结果。ok 只取决于**读回复核**，不取决于驱动返回码。
#[derive(Debug, Clone)]
pub struct SmWriteResult {
    pub ok: bool,
    pub readback: Option<u32>,
    /// 到底写进了哪个 profile（NVIDIA 预置的，还是我们自建的）
    pub profile: String,
    pub message: String,
}

pub fn game_profile_name(game: &str) -> String {
    format!("{}{}", PROFILE_PREFIX, game)
}

// ---- 备份记录：这个游戏上一次的 SM 值是什么 ----

/// 一条"我们改过这个游戏"的记录。
///
/// 为什么要记：真实游戏都归属 NVIDIA 预置 profile，我们不能再像以前那样把配置
/// 写进自建 profile 就完事 —— 关闭时得把**旧值**写回去，还得知道那个 profile
/// 是不是我们建的（不是就绝不能删）。
#[derive(Debug, Clone, PartialEq)]
struct SmRecord {
    game: String,
    exe: String,
    profile: String,
    /// profile 是我们自建的吗（是的话还原时可以连 profile 一起删）
    ours: bool,
    /// 改之前这一项存在吗
    had: bool,
    had_value: Option<u32>,
    /// 游戏目录（记录名是它的哈希；这里存原值，好让"全部还原"找回同一个记录）
    dir: String,
}

fn sm_records_dir() -> Result<std::path::PathBuf, String> {
    let dir = crate::util::backups_dir()
        .map_err(|e| format!("{e:#}"))?
        .join("sm-profiles");
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录 {} 失败：{e}", dir.display()))?;
    Ok(dir)
}

fn record_path(game_dir: &std::path::Path) -> Result<std::path::PathBuf, String> {
    // 目录为空时退回"按游戏名算键"：这种记录只可能来自"调用方没给目录"，还原时会用
    // find_record 再按 EXE / 游戏名找回来，所以键本身是什么不重要。
    let key = if game_dir.as_os_str().is_empty() {
        crate::util::dir_key(std::path::Path::new(""))
    } else {
        crate::util::dir_key(game_dir)
    };
    Ok(sm_records_dir()?.join(format!("{key}.json")))
}

/// 备份记录里的每一条（还原时按 EXE / 游戏名兜底查找用）。
fn all_records() -> Vec<(std::path::PathBuf, SmRecord)> {
    let Ok(dir) = sm_records_dir() else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if !p.extension().map(|x| x == "json").unwrap_or(false) {
            continue;
        }
        if let Some(r) = record_read(&p) {
            out.push((p, r));
        }
    }
    out
}

/// 按目录找不到记录时，按 EXE 路径（忽略大小写）或游戏名再找一次。
fn find_record(
    recs: &[(std::path::PathBuf, SmRecord)],
    exe: &std::path::Path,
    game: &str,
) -> Option<(std::path::PathBuf, SmRecord)> {
    let exe_s = exe.display().to_string();
    let hit = recs.iter().find(|(_, r)| {
        (!exe_s.is_empty() && !r.exe.is_empty() && r.exe.eq_ignore_ascii_case(&exe_s))
            || (!game.is_empty() && !r.game.is_empty() && r.game == game)
    });
    hit.map(|(p, r)| (p.clone(), r.clone()))
}

/// JSON 字符串转义。记录是手写的（不引 serde），字段值一律走这里。
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn record_write(path: &std::path::Path, r: &SmRecord) -> Result<(), String> {
    let had_value = match r.had_value {
        Some(v) => v.to_string(),
        None => "null".to_owned(),
    };
    let body = format!(
        "{{\"game\":\"{}\",\"exe\":\"{}\",\"profile\":\"{}\",\"ours\":{},\"had\":{},\"had_value\":{},\"dir\":\"{}\"}}",
        json_escape(&r.game),
        json_escape(&r.exe),
        json_escape(&r.profile),
        r.ours,
        r.had,
        had_value,
        json_escape(&r.dir)
    );
    // 原子写（原来是自己写 tmp + rename，现在和其他几处共用同一条实现）
    crate::util::write_text_atomic(path, &body).map_err(|e| format!("落盘记录失败：{e}"))?;
    Ok(())
}

fn record_read(path: &std::path::Path) -> Option<SmRecord> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(SmRecord {
        game: record_str(&text, "game").unwrap_or_default(),
        exe: record_str(&text, "exe").unwrap_or_default(),
        profile: record_str(&text, "profile").unwrap_or_default(),
        ours: record_bool(&text, "ours").unwrap_or(false),
        had: record_bool(&text, "had").unwrap_or(false),
        had_value: record_u32(&text, "had_value"),
        dir: record_str(&text, "dir").unwrap_or_default(),
    })
}

/// 在记录里找 "key":"值"（值由 json_escape 写出）。找不到返回 None。
fn record_str(text: &str, key: &str) -> Option<String> {
    let marker = format!("\"{key}\":\"");
    let start = text.find(&marker)? + marker.len();
    let rest = text.get(start..)?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// 在记录里找 "key":true|false。找不到返回 None。
fn record_bool(text: &str, key: &str) -> Option<bool> {
    for want in ["true", "false"] {
        if text.contains(&format!("\"{key}\":{want}")) {
            return Some(want == "true");
        }
    }
    None
}

/// 在记录里找 "key":123 或 "key":null。找不到/null 都返回 None。
fn record_u32(text: &str, key: &str) -> Option<u32> {
    let marker = format!("\"{key}\":");
    let start = text.find(&marker)? + marker.len();
    let rest = text.get(start..)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// 记录里的 profile 句柄。None = 名字对不上了（被驱动改名/删掉），交给调用方退回自建。
fn handle_by_name(sess: &nv::Session, name: &str) -> Result<Option<*mut std::ffi::c_void>, String> {
    match sess.find(name)? {
        Some(p) if !sess.is_base_profile(p) => Ok(Some(p)),
        _ => Ok(None),
    }
}

/// 读某个 profile 里的 SM 值。rc=-160 = 这一项没设置（跟随全局）→ None。
fn read_current(sess: &nv::Session, prof: *mut std::ffi::c_void) -> Result<Option<u32>, String> {
    match sess.read(prof, SM_SETTING_ID) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.starts_with("rc=-160") => Ok(None),
        Err(e) => Err(e),
    }
}

/// 还原之后的值是不是我们期望的"改之前那样"。
fn restored_matches(sess: &nv::Session, prof: *mut std::ffi::c_void, want: Option<u32>) -> bool {
    match read_current(sess, prof) {
        Ok(v) => v == want,
        Err(_) => false,
    }
}

/// 算出某个游戏 EXE 现在的 SM 状态：它在哪个 profile 里、那个 profile 是不是我们建的、
/// 值是多少。界面上的状态列表用它（自己开会话）。
pub fn game_state(exe: &std::path::Path) -> SmGameState {
    match nv::Session::open() {
        Ok(sess) => game_state_with(&sess, exe),
        Err(e) => SmGameState {
            exe: exe.display().to_string(),
            profile: String::new(),
            ours: false,
            value: None,
            detail: e,
        },
    }
}

/// 同上，但复用调用方已经开好的会话（一次查询一批游戏时省掉反复开会话）。
pub fn game_state_with(sess: &nv::Session, exe: &std::path::Path) -> SmGameState {
    let mut out = SmGameState {
        exe: exe.display().to_string(),
        profile: String::new(),
        ours: false,
        value: None,
        detail: String::new(),
    };
    let prof = match sess.find_profile_for_exe(exe) {
        Ok(Some(p)) => p,
        Ok(None) => {
            out.detail = "驱动里没有挂这个 EXE 的 profile".to_owned();
            return out;
        }
        Err(e) => {
            out.detail = e;
            return out;
        }
    };
    if sess.is_base_profile(prof) {
        out.detail = "拒绝：按 EXE 找到的竟然是全局 profile".to_owned();
        return out;
    }
    out.profile = sess.profile_name(prof);
    out.ours = !out.profile.is_empty() && out.profile.starts_with(PROFILE_PREFIX);
    out.value = read_current(sess, prof).unwrap_or_else(|e| {
        out.detail = e;
        None
    });
    out
}

/// 按 profile 名字读 SM 值（老记录只存了游戏名、没有 EXE 路径时用它兜底）。
/// None = 没有这个 profile，或者里面没设置这一项。
pub fn game_sm_value_in(profile: &str) -> Result<Option<u32>, String> {
    if profile.is_empty() {
        return Ok(None);
    }
    let sess = nv::Session::open()?;
    match handle_by_name(&sess, profile)? {
        None => Ok(None),
        Some(p) => read_current(&sess, p),
    }
}

/// 开一个会话给批量查询用（界面的状态列表）。
pub fn open_session() -> Result<nv::Session, String> {
    nv::Session::open()
}

/// 开启 / 关闭某个游戏的 Smooth Motion。
///
/// 目标 profile 的选择顺序：
///   1. 问驱动这个 EXE 归属哪个 profile（真实游戏几乎都已经属于 NVIDIA 预置的那一个）；
///   2. 找不到才自建 "FrameGen Manager - 游戏名" 并把这个 EXE 挂上去 —— 挂不上（被占用）
///      就直接报错，不留一个挂不上游戏的空 profile。
///
/// 写之前先把旧值记进备份目录（game_dir 决定记录名）。关闭时按你要求走
/// restore_game_sm —— 把旧值写回去（本来没设置就删掉这一项）。
pub fn set_game_sm(
    game: &str,
    game_dir: &std::path::Path,
    exe: &std::path::Path,
    on: bool,
) -> Result<SmWriteResult, String> {
    if !game.is_empty() && game_dir.as_os_str().is_empty() {
        return Err("缺少游戏目录（备份记录要放在那里）".to_owned());
    }
    let dir = if game_dir.as_os_str().is_empty() {
        std::path::Path::new(game)
    } else {
        game_dir
    };
    let want = record_path(dir)?;
    let rec = record_read(&want);
    let sess = nv::Session::open()?;

    // 驱动里现在的归属。这一步必须在写之前 —— 写完句柄可能就变了。
    let target = sess.find_profile_for_exe(exe)?;
    let (prof, profile_name) = match target {
        Some(p) => {
            if sess.is_base_profile(p) {
                return Err("拒绝：不允许写全局/基础 profile。".to_owned());
            }
            let name = sess.profile_name(p);
            if name.is_empty() {
                return Err(
                    "读不到这个 EXE 归属的 profile 的名字，为安全起见中止（不写）。".to_owned(),
                );
            }
            (p, name)
        }
        None => {
            let name = game_profile_name(game);
            let p = match sess.find(&name)? {
                Some(p) if !sess.is_base_profile(p) => p,
                _ => sess.create_profile(&name)?,
            };
            // 自建的 profile 必须真的把 EXE 挂上，否则设置永远不会生效 ——
            // 挂不上就撤掉这个 profile，不留孤儿。
            if let Err(e) = sess.create_application(p, exe) {
                let _ = sess.delete_profile(p);
                let _ = sess.save();
                return Err(format!(
                    "这个游戏还不属于任何驱动配置，而新建的配置也挂不上它：{e}。请确认选的是游戏真正运行的那个 EXE。"
                ));
            }
            (p, name)
        }
    };

    // 只在"这次是第一次改这个游戏"或者记录指向的 profile 变了的时候才重新记旧值。
    let need_record = rec
        .as_ref()
        .map(|r| r.profile != profile_name)
        .unwrap_or(true);
    if need_record {
        let had = read_current(&sess, prof)?;
        let r = SmRecord {
            game: game.to_owned(),
            exe: exe.display().to_string(),
            profile: profile_name.clone(),
            ours: profile_name.starts_with(PROFILE_PREFIX),
            had: had.is_some(),
            had_value: had,
            dir: dir.display().to_string(),
        };
        record_write(&want, &r)?;
    }

    let value = if on { 1u32 } else { 0u32 };
    let rc_set = sess.set_dword(prof, SM_SETTING_ID, value)?;
    sess.save()?;

    // 复核必须换一个**全新的会话**：同一个会话里读回来的可能只是内存里那份。
    let readback = nv::Session::open()
        .ok()
        .and_then(|s| s.read(prof, SM_SETTING_ID).ok());
    let ok = readback == Some(value);
    crate::log::line(&format!(
        "[sm] set_game_sm game={game} on={on} profile={profile_name} rc={rc_set} readback={readback:?}"
    ));
    Ok(SmWriteResult {
        ok,
        readback,
        profile: profile_name.clone(),
        message: if ok {
            format!(
                "已{}（写进驱动配置「{profile_name}」，读回复核 = {value}，驱动返回码 {rc_set}）",
                if on { "开启" } else { "关闭" }
            )
        } else {
            format!("写入后复核不符：期望 {value}，实际读到 {readback:?}（配置「{profile_name}」）")
        },
    })
}

/// 还原某个游戏：把 SM 这一项恢复成本工具改它之前的样子。
///
///   * 改之前本来就有值 → 写回那个值；
///   * 改之前没这一项 → 删掉这一项（跟随全局）；
///   * 只有 profile 是**我们自建的**才把它删掉 —— NVIDIA 预置的 profile 绝不删。
///
/// 返回一句给用户看的结果说明。
pub fn restore_game_sm(
    game: &str,
    game_dir: &std::path::Path,
    exe: &std::path::Path,
) -> Result<String, String> {
    let dir = if game_dir.as_os_str().is_empty() {
        std::path::Path::new(game)
    } else {
        game_dir
    };
    // 记录名是目录的哈希：目录对不上时（调用方只给了 EXE、或者库里的目录和记记录时的不一样），
    // 按 EXE / 游戏名把记录找回来 —— 否则这里会以为"没改过"，把驱动里开着的开关留在原地。
    let (rec_path, rec) = match record_read(&record_path(dir)?) {
        Some(r) => (record_path(dir)?, Some(r)),
        None => match find_record(&all_records(), exe, game) {
            Some((p, r)) => (p, Some(r)),
            None => (record_path(dir)?, None),
        },
    };
    let sess = nv::Session::open()?;

    // 找目标 profile：优先用记录里的名字；记录没了就再问一次驱动。
    let mut target: Option<(*mut std::ffi::c_void, String)> = None;
    if let Some(r) = &rec {
        if !r.profile.is_empty() {
            if let Some(p) = handle_by_name(&sess, &r.profile)? {
                target = Some((p, r.profile.clone()));
            }
        }
        if target.is_none() && !r.exe.is_empty() {
            if let Some(p) = sess.find_profile_for_exe(std::path::Path::new(&r.exe))? {
                if !sess.is_base_profile(p) {
                    target = Some((p, sess.profile_name(p)));
                }
            }
        }
    }

    let Some((prof, profile_name)) = target else {
        // 没有记录、驱动里也没有挂这个 EXE 的配置 = 本来就没改过。
        let _ = std::fs::remove_file(&rec_path);
        return Ok("驱动里没有这个游戏的配置，也没有本工具的记录 —— 无需还原。".to_owned());
    };

    let want = rec
        .as_ref()
        .map(|r| if r.had { r.had_value } else { None })
        .unwrap_or(None);
    let ours = rec
        .as_ref()
        .map(|r| r.ours)
        .unwrap_or_else(|| profile_name.starts_with(PROFILE_PREFIX));

    if let Some(v) = want {
        sess.set_dword(prof, SM_SETTING_ID, v)?;
    } else {
        sess.delete_setting(prof, SM_SETTING_ID)?;
    }
    sess.save()?;

    // 复核同样换新会话。
    let mut back_ok = false;
    let mut back_seen: Option<u32> = None;
    if let Ok(s) = nv::Session::open() {
        back_seen = read_current(&s, prof).ok().flatten();
        back_ok = restored_matches(&s, prof, want);
    }
    if !back_ok {
        return Err(format!(
            "写回旧值时复核不符：期望 {want:?}，实际读到 {back_seen:?}（配置「{profile_name}」）"
        ));
    }

    // 我们自建的 profile 现在没有别的东西了：删掉它，驱动里就当从没配过。
    let mut deleted_profile = false;
    if ours && !profile_name.is_empty() && profile_name.starts_with(PROFILE_PREFIX) {
        if let Some(p) = handle_by_name(&sess, &profile_name)? {
            if sess.apps(p).is_empty() {
                sess.delete_profile(p)?;
                sess.save()?;
                deleted_profile = true;
            }
        }
    }
    let _ = std::fs::remove_file(&rec_path);
    crate::log::line(&format!(
        "[sm] restore_game_sm game={game} profile={profile_name} ours={ours} want={want:?} deleted_profile={deleted_profile}"
    ));
    Ok(if deleted_profile {
        format!("已还原并删掉本工具建的配置「{profile_name}」")
    } else if let Some(v) = want {
        format!("已还原成原来的设置（配置「{profile_name}」里的旧值 {v}）")
    } else {
        format!("已还原（配置「{profile_name}」里这一项已删除，跟随全局）")
    })
}

/// 【关闭】把 SM 这一项**写成 0**（本游戏强制关闭）。
///
/// 和 restore_game_sm 的区别：restore 是"把我们的改动收回去"（旧值是开就回开），
/// 这个是"关掉"。界面按钮上写的是「关闭 Smooth Motion」，那它就必须真的关掉 ——
/// 无论之前有没有我们的记录、旧值是几。关掉后这次操作不再是"待还原"，记录一并清掉。
pub fn turn_off_game_sm(
    game: &str,
    game_dir: &std::path::Path,
    exe: &std::path::Path,
) -> Result<String, String> {
    let before;
    let profile_name;
    {
        // 句柄只在它自己那次会话里有效，所以查找 / 读旧值 / 写 / 保存都在这一个会话里做完。
        let sess = nv::Session::open()?;
        let prof = match sess.find_profile_for_exe(exe)? {
            Some(p) => p,
            None => {
                // 没挂这个 EXE 就没什么可关的；直说，别报"成功关闭"让人误会。
                return Ok(format!(
                    "驱动里没有挂这个 EXE 的配置，Smooth Motion 本来就没为它开过（{game}）"
                ));
            }
        };
        if sess.is_base_profile(prof) {
            return Err("拒绝：不允许写全局/基础 profile。".to_owned());
        }
        profile_name = sess.profile_name(prof);
        before = read_current(&sess, prof)?;
        sess.set_dword(prof, SM_SETTING_ID, 0)?;
        sess.save()?;
    }
    // 复核换一个**全新会话**：同一个会话里读回来的可能只是内存那份。复核按 profile 名字找。
    let after = match nv::Session::open() {
        Ok(s) => match handle_by_name(&s, &profile_name) {
            Ok(Some(h)) => s.read(h, SM_SETTING_ID).ok(),
            _ => None,
        },
        Err(_) => None,
    };
    if after != Some(0) {
        return Err(format!(
            "关闭后复核不符：期望 0，实际读到 {after:?}（配置「{profile_name}」）"
        ));
    }
    drop_game_sm_record(game, game_dir, exe);
    crate::log::line(&format!(
        "[sm] turn_off_game_sm game={game} profile={profile_name} before={before:?} after={after:?}"
    ));
    Ok(match before {
        Some(1) => format!("已关闭（配置「{profile_name}」里的 Smooth Motion 由开改为关）"),
        Some(v) => format!("已关闭（配置「{profile_name}」里原本是 {v}，现在写成 0）"),
        None => format!("已关闭（在配置「{profile_name}」里写入 0，不再跟随全局）"),
    })
}

/// 删掉某个游戏的那条备份记录（按目录 / EXE / 游戏名找）。返回是否删了东西。
pub fn drop_game_sm_record(game: &str, game_dir: &std::path::Path, exe: &std::path::Path) -> bool {
    let mut dropped = false;
    // 目录对得上就直接删
    if !game_dir.as_os_str().is_empty() {
        if let Ok(p) = record_path(game_dir) {
            if std::fs::remove_file(&p).is_ok() {
                dropped = true;
            }
        }
    }
    // 再按内容兜底（记录名是目录哈希，只有内容能可靠匹配）
    let exe_s = exe.display().to_string();
    for (f, r) in all_records() {
        let hit = (!exe_s.is_empty() && !r.exe.is_empty() && r.exe.eq_ignore_ascii_case(&exe_s))
            || (!game.is_empty() && !r.game.is_empty() && r.game == game);
        if hit && std::fs::remove_file(&f).is_ok() {
            dropped = true;
        }
    }
    dropped
}

// ---------------------------------------------------------------- 自测用的小接口

/// 自测用的一条记录（形如 "备份记录里的一行"）。
#[derive(Debug, Clone, PartialEq)]
pub struct TestRecord {
    pub game: String,
    pub exe: String,
    pub profile: String,
    pub ours: bool,
    pub had: bool,
    pub had_value: Option<u32>,
    pub dir: String,
}

fn to_test_record(r: &SmRecord) -> TestRecord {
    TestRecord {
        game: r.game.clone(),
        exe: r.exe.clone(),
        profile: r.profile.clone(),
        ours: r.ours,
        had: r.had,
        had_value: r.had_value,
        dir: r.dir.clone(),
    }
}

/// 自测用：按游戏目录写一条记录并读回来（不碰驱动）。写读不一致就是 bug。
pub fn test_record(
    game: &str,
    exe: &str,
    profile: &str,
    ours: bool,
    had: bool,
    had_value: Option<u32>,
    game_dir: &std::path::Path,
) -> TestRecord {
    let r = SmRecord {
        game: game.to_owned(),
        exe: exe.to_owned(),
        profile: profile.to_owned(),
        ours,
        had,
        had_value,
        dir: game_dir.display().to_string(),
    };
    let path = record_path(game_dir).expect("算记录路径失败");
    record_write(&path, &r).expect("写记录失败");
    let back = record_read(&path).expect("记录读不回来");
    to_test_record(&back)
}

/// 自测用：按游戏目录把记录读回来。None = 没有这份记录。
pub fn test_record_read(game_dir: &std::path::Path) -> Option<TestRecord> {
    let path = record_path(game_dir).ok()?;
    record_read(&path).as_ref().map(to_test_record)
}

/* 这里原来有两个函数：`orphan_profiles()`（找出「我们建过但没挂应用」的驱动 profile）
和它专用的 `recorded_profile_names()`。egui 版退役后它们**没有任何调用点**
（sidecar 里也没有对应命令），已随本轮代码结构审计删除（见 docs/CHANGELOG.md）；
要恢复从 git 历史里取。 */

// ---------------------------------------------------------------- 内置解锁组件的部署 / 还原

pub const SM_ASSET_DIR: &str = "bundled-sm";
pub const SM_ASSET_FILE: &str = "version.dll";

/// 内置解锁组件的 SHA256（固定值；用它就能在任意游戏目录里认出"这是 SM 放的"）。
/// 当前 = 上游 sm8792 的 RTXMFG-SmoothMotion-next（version (5).dll，默认帧率上限改为不限制）。
pub const SM_ASSET_SHA256: &str =
    "0a9684288be14cdfa44a042c6e2f4c490f0114dca1729aef925be95490b74a50";

/// 历史版本的 30 系组件（sm8791 的 0.30.3.2 / version (34).dll）。
/// 老用户游戏目录里可能还是它 —— 归属识别必须继续认（部署会原地覆盖成新版）。
pub const SM_ASSET_LEGACY_SHA256: &str =
    "4091efe2db3dc8dbb671a187b3904c36a58da18149b309021c5367d1646c21cb";

/// RTX 20 运行库（native 三件套）的指纹与文件名（上游 Fix37，source/rtx20 构建）。
pub const SM20_LOADER_SHA256: &str =
    "7b6fd97e626c2d55ad0597a68e4f6fb84a76eeee3ee42405b032acbb588dce0f";
pub const SM20_RUNTIME_SHA256: &str =
    "e6fe728f7a97caea87d594aa4bf18b239c1aadb9b245ee92f0390ecaba915016";
pub const SM20_MANIFEST_SHA256: &str =
    "92e16f20b372d2897fa7d64c85aef572cdf4b6038db70e1a9c19b13d42be630d";
pub const SM20_MANIFEST_NAME: &str = "runtime-manifest.json";

/// 这个目录里的 version.dll 是不是我们的解锁组件？
pub fn has_our_asset(dir: &std::path::Path) -> bool {
    is_our_asset_file(&dir.join(SM_ASSET_FILE)) || is_our_asset_file(&dir.join("normal.asi"))
}

/// 这个目录里是不是**旧版**（sm8791 的 version (34).dll）30 系组件？
/// 用来提醒老用户「组件有新版本，建议先还原后重新部署」。
pub fn has_legacy_asset(dir: &std::path::Path) -> bool {
    let p = dir.join(SM_ASSET_FILE);
    p.is_file()
        && crate::util::sha256_file(&p)
            .map(|h| h.eq_ignore_ascii_case(SM_ASSET_LEGACY_SHA256))
            .unwrap_or(false)
}

/// 这个游戏「疑似开着 Smooth Motion」吗（给部署帧生成前的那句提醒用）。
///
/// 两条判据任一成立就算：驱动里挂在这个 EXE 上的 SM 值非 0；或者游戏目录里还留着
/// 我们的解锁组件 —— 关闭时没取回成功的话，驱动关了文件还在，同样要提醒。
pub fn looks_enabled_for_dir(dir: &std::path::Path, exe: Option<&std::path::Path>) -> bool {
    if has_our_asset(dir) {
        return true;
    }
    match exe {
        Some(e) => matches!(game_state(e).value, Some(v) if v != 0),
        None => false,
    }
}

/// 这个文件是不是内置解锁组件 —— **按内容哈希认，不认签名**。
///
/// 为什么不能靠签名：这个组件是社区打包的，不是我们自签的，按签名判会被当成
/// 「别人的文件」。而它确实是本工具放进游戏目录的，部署帧生成时该按自己人处理。
pub fn is_our_asset_file(path: &std::path::Path) -> bool {
    path.is_file()
        && crate::util::sha256_file(path)
            .map(|h| is_our_asset_hash(&h))
            .unwrap_or(false)
}

/// 我们放进游戏目录的文件的指纹集合：30 系组件、20 系加载器、20 系运行库。
/// 归属判定一律走这里，别再单独比某个常量（两个系列的组件都要认）。
fn is_our_asset_hash(h: &str) -> bool {
    h.eq_ignore_ascii_case(SM_ASSET_SHA256)
        || h.eq_ignore_ascii_case(SM_ASSET_LEGACY_SHA256)
        || h.eq_ignore_ascii_case(SM20_LOADER_SHA256)
        || h.eq_ignore_ascii_case(SM20_RUNTIME_SHA256)
}

/// 当前 GPU 该用哪套解锁组件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmComponent {
    Rtx30,
    Rtx20,
}

pub fn component_for_gpu() -> SmComponent {
    let gpu = crate::scan::detect_gpu().unwrap_or_default();
    if matches!(crate::scan::classify_gpu(&gpu), crate::scan::GpuRoute::Sm75) {
        SmComponent::Rtx20
    } else {
        SmComponent::Rtx30
    }
}

/// 内置资产根（assets 目录下的 bundled-sm）。
pub fn asset_base() -> Result<std::path::PathBuf, String> {
    let d = crate::util::assets_dir().map_err(|e| format!("找不到内置资产目录：{e:#}"))?;
    Ok(d.join(SM_ASSET_DIR))
}

/// 一套组件 = 要放进游戏目录的 (相对文件名, 内置源相对路径, 期望 SHA256) 列表。
///
/// 30 系 = 单文件解锁组件；20 系 = native 三件套（ASI 加载器 + 运行库 + 清单）。
pub fn component_files(c: SmComponent) -> Vec<(&'static str, &'static str, &'static str)> {
    match c {
        SmComponent::Rtx30 => vec![(SM_ASSET_FILE, "rtx30/version.dll", SM_ASSET_SHA256)],
        SmComponent::Rtx20 => vec![
            (SM_ASSET_FILE, "rtx20/version.dll", SM20_LOADER_SHA256),
            ("normal.asi", "rtx20/normal.asi", SM20_RUNTIME_SHA256),
            (
                SM20_MANIFEST_NAME,
                "rtx20/runtime-manifest.json",
                SM20_MANIFEST_SHA256,
            ),
        ],
    }
}

fn asset_record_path(game_dir: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let dir = crate::util::backups_dir()
        .map_err(|e| format!("{e:#}"))?
        .join("sm-assets");
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录 {} 失败：{e}", dir.display()))?;
    let key: String = game_dir
        .display()
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    Ok(dir.join(format!("{}.json", key)))
}

/// 把内置解锁组件放进这个游戏目录（备份同名原文件，写入后逐字节复核）。
/// 返回给用户看的一句话说明。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AssetFileRecord {
    pub rel: String,
    #[serde(default)]
    pub had_original: bool,
    #[serde(default)]
    pub backup: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

/// 解锁组件的部署记录。v1 = 单文件时代的手写 JSON（没有 v 字段 → 按 1 处理）；
/// v2 = 多文件（RTX 20 的三件套）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AssetRecord {
    pub dir: String,
    #[serde(default = "default_asset_record_v")]
    pub v: u8,
    #[serde(default)]
    pub had_original: bool,
    #[serde(default)]
    pub backup: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
    #[serde(default)]
    pub files: Vec<AssetFileRecord>,
}
fn default_asset_record_v() -> u8 {
    1
}

/// 记录之外、目录里遗留的"我们的文件"（例如上次部署中断留下的）也要认出来。
fn known_asset_rels() -> Vec<&'static str> {
    component_files(SmComponent::Rtx30)
        .into_iter()
        .map(|f| f.0)
        .chain(component_files(SmComponent::Rtx20).into_iter().map(|f| f.0))
        .collect()
}

/// 读一个 PE 文件的 FileVersion 主次号（(major, minor)）。读不到返回 None。
fn file_version_pair(p: &std::path::Path) -> Option<(u16, u16)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };
    let wide: Vec<u16> = p
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let mut dummy = 0u32;
        let size = GetFileVersionInfoSizeW(wide.as_ptr(), &mut dummy);
        if size == 0 {
            return None;
        }
        let mut buf: Vec<u8> = vec![0; size as usize];
        if GetFileVersionInfoW(wide.as_ptr(), 0, size, buf.as_mut_ptr() as *mut _) == 0 {
            return None;
        }
        let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut len: u32 = 0;
        let sub: [u16; 2] = ['\\' as u16, 0];
        if VerQueryValueW(buf.as_ptr() as *const _, sub.as_ptr(), &mut ptr, &mut len) == 0
            || ptr.is_null()
            || len < 52
        {
            return None;
        }
        // VS_FIXEDFILEINFO：signature(0) strucVersion(1) FileVersionMS(2) FileVersionLS(3)
        let info = ptr as *const u32;
        let ms = *info.add(2);
        Some(((ms >> 16) as u16, (ms & 0xffff) as u16))
    }
}

/// 新版 30 系组件（SmoothMotion-next）**动态依赖 VC++ 2015-2022 运行库**。
///
/// 以前版本不够就**直接拒绝部署**，实测有用户被卡住（系统里其实能跑）。
/// 现在改成**只提醒不拦截**：照常部署，把提醒附在结果里。
fn vcruntime_warning(game_dir: &std::path::Path) -> Option<String> {
    let local = game_dir.join("msvcp140.dll");
    let mut ver = if local.is_file() {
        file_version_pair(&local)
    } else {
        None
    };
    if ver.is_none() {
        ver = std::env::var_os("SystemRoot")
            .map(|r| {
                std::path::PathBuf::from(r)
                    .join("System32")
                    .join("msvcp140.dll")
            })
            .as_deref()
            .and_then(file_version_pair);
    }
    match ver {
        Some((major, minor)) if (major, minor) >= (14, 40) => None,
        Some((major, minor)) => Some(format!(
            "提醒：检测到 msvcp140.dll 版本较旧（{major}.{minor}），新版 30 系组件建议 14.40 以上；若进游戏后闪退，请安装微软「Visual C++ 2015-2022 运行库」最新版后重试。"
        )),
        None => Some(
            "提醒：没读到 msvcp140.dll 的版本（可能没装 VC++ 2015-2022 运行库）。已照常部署；若进游戏后闪退，请安装微软「Visual C++ 2015-2022 运行库」最新版后重试。"
                .to_owned(),
        ),
    }
}

pub fn asset_deploy(game_dir: &std::path::Path) -> Result<String, String> {
    let comp = component_for_gpu();
    let base = asset_base()?;
    if !game_dir.is_dir() {
        return Err(format!("游戏目录不存在：{}", game_dir.display()));
    }
    if !crate::util::is_writable(game_dir) {
        return Err(format!("没有权限写入 {}", game_dir.display()));
    }
    let mut vc_warn: Option<String> = None;
    if comp == SmComponent::Rtx30 {
        vc_warn = vcruntime_warning(game_dir);
    }
    let files = component_files(comp);
    let rec = asset_record_path(game_dir)?;
    let bdir = rec
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("originals");
    std::fs::create_dir_all(&bdir).map_err(|e| format!("{e}"))?;

    /* 1) 先做完全部**只读**的准备工作：源文件在不在、目标能不能写、原件长什么样。
    这一步一个字节都不写进游戏目录 —— 任何一项不过就直接返回，用户看到的是「什么都没动」
    （以前是边备份边铺，中途失败还得靠回滚把前面几个文件拼回去）。 */
    let mut done: Vec<AssetFileRecord> = Vec::new();
    let mut items: Vec<crate::backup_restore::WriteItem> = Vec::new();
    let mut plan = crate::backup_restore::BackupPlan::new();
    for (rel, src_rel, want) in &files {
        let mut src = base.join(src_rel.replace('/', "\\"));
        if !src.is_file() {
            // 兼容旧数据目录：老版本铺开是**平铺的** version.dll（内容与 rtx30 组件相同）。
            // 铺开逻辑没跟上之前装的副本，靠这个兜底也能部署。
            if *src_rel == "rtx30/version.dll" {
                let flat = base.join(SM_ASSET_FILE);
                if flat.is_file() {
                    src = flat;
                }
            }
        }
        if !src.is_file() {
            return Err(format!("内置组件不存在：{}", src.display()));
        }
        let dst = game_dir.join(rel);
        match crate::util::lock_state(&dst) {
            crate::util::LockState::InUse => {
                return Err(format!("{rel} 正被占用，请先完全退出游戏。"));
            }
            crate::util::LockState::NoAccess => {
                return Err(format!(
                    "{rel} 现在写不进去：文件带了只读属性，或者权限不足。去掉只读 / 换个可写的目录再试。"
                ));
            }
            crate::util::LockState::Free => {}
        }
        let bp = bdir.join(crate::backup_restore::backup_file_name(rel));
        let mut had_original = false;
        let mut backup: Option<String> = None;
        if dst.is_file() && is_our_asset_file(&dst) {
            /* 目录里已经是**我们自己的组件**（老版本部署的）：绝不能拿它去覆盖备份——
            否则"关闭"时会把这个旧组件当成原件又还原回来（用户实测踩过：关了组件还在）。
            备份里要保留的是**更早的原件**；原件备份不存在就当没有原件（关闭时直接删）。 */
            if bp.is_file() {
                let bytes = std::fs::read(&bp)
                    .map_err(|e| format!("读取备份 {} 失败：{e}", bp.display()))?;
                plan.push(*rel, true, bytes);
                had_original = true;
                backup = Some(bp.display().to_string());
            } else {
                plan.push(*rel, false, Vec::new());
            }
        } else if dst.is_file() {
            // 原件先读进内存并落成 .bak（公共层负责扁平备份名与 safe_join）
            let bytes = crate::backup_restore::snapshot_one(game_dir, rel, &bdir)
                .map_err(|e| format!("{e:#}"))?
                .ok_or_else(|| format!("{rel} 读不出来，无法备份原件"))?;
            plan.push(*rel, true, bytes);
            had_original = true;
            backup = Some(bp.display().to_string());
        } else {
            plan.push(*rel, false, Vec::new());
        }
        done.push(AssetFileRecord {
            rel: (*rel).to_string(),
            had_original,
            backup,
            sha: Some((*want).to_ascii_lowercase()),
        });
        items.push(crate::backup_restore::WriteItem {
            rel: (*rel).to_string(),
            src,
            sha256: (*want).to_string(),
        });
    }

    /* 2) 铺文件：原子替换 + 逐个校验，任何一步失败都按上面的快照回滚
    （有原件的写回原件、没有的删掉），游戏目录不会停在半铺状态。 */
    crate::backup_restore::apply(game_dir, &plan, &items).map_err(|e| format!("{e:#}"))?;
    let had_any = done.iter().any(|f| f.had_original);
    let body = AssetRecord {
        dir: game_dir
            .display()
            .to_string()
            .replace(std::path::MAIN_SEPARATOR, "/"),
        v: 2,
        had_original: false,
        backup: None,
        sha: None,
        files: done,
    };
    std::fs::write(
        &rec,
        serde_json::to_string(&body).map_err(|e| format!("{e}"))?,
    )
    .map_err(|e| format!("写部署记录失败：{e}"))?;
    crate::log::line(&format!("[sm] 已部署解锁组件到 {}", game_dir.display()));
    let label = match comp {
        SmComponent::Rtx30 => "RTX 30 系组件（SmoothMotion-next）",
        SmComponent::Rtx20 => "RTX 20 系运行库（加载器 + 运行库 + 清单）",
    };
    let size = body.files.iter().filter_map(|f| f.sha.as_deref()).count();
    let _ = size;
    let mut msg = format!(
        "已放入解锁组件（{}，{} 个文件）{}",
        label,
        files.len(),
        if had_any { "，原文件已备份" } else { "" }
    );
    if let Some(w) = vc_warn {
        msg.push_str("\n\n");
        msg.push_str(&w);
    }
    Ok(msg)
}

/// 在"这个游戏可能把组件放在哪"的几个目录里挨个找我们的解锁组件，找到就取回。
///
/// 为什么需要它：同一个游戏在库里可能出现两条，目录不一样 —— 自动扫描得到的是安装根目录，
/// 手动选 exe 得到的是 exe 所在的那一层（例如 HogwartsLegacy 与 ...\Binaries\Win64）。
/// 用户在扫出来的那条卡片上点「关闭」，组件却躺在子目录里，就会既关不掉、
/// 卡片上的徽章又一直显示「已被 Smooth Motion 占用」。这里把 exe 目录、游戏目录、
/// 以及它的上一层都搜一遍，谁有就取回谁。
pub fn asset_restore_searching(
    game_dir: &std::path::Path,
    exe: &std::path::Path,
) -> Result<String, String> {
    let mut cands: Vec<std::path::PathBuf> = Vec::new();
    let mut push = |d: &std::path::Path| {
        if d.as_os_str().is_empty() {
            return;
        }
        let key = d.display().to_string().to_lowercase();
        if !cands
            .iter()
            .any(|c| c.display().to_string().to_lowercase() == key)
        {
            cands.push(d.to_path_buf());
        }
    };
    // 顺序：先看卡片自己那两个目录（最可能），再往上看一层
    if let Some(p) = exe.parent() {
        push(p);
    }
    push(game_dir);
    if let Some(p) = game_dir.parent() {
        push(p);
    }
    if let Some(p) = exe.parent().and_then(|p| p.parent()) {
        push(p);
    }
    for d in &cands {
        // 用指纹判断（就是徽章用的那一条），**不要**用 asset_restore 里的"记录+指纹都匹配"，
        // 否则纯手动加进来的目录（没有部署记录）会被它拒掉，组件永远留在盘上。
        if !has_our_asset(d) {
            continue;
        }
        let msg = match asset_restore(d) {
            Ok(m) => m,
            Err(e) => {
                // 记录对不上（或压根没有）时直接删 —— 到这里已经确认它就是我们的组件了。
                // 20 系是三件套：把目录里所有"我们的文件"都按指纹清掉。
                for rel in known_asset_rels() {
                    let f = d.join(rel);
                    if is_our_asset_file(&f) {
                        remove_with_retry(&f)?;
                    }
                }
                crate::log::line(&format!(
                    "[sm] 按指纹直接删除组件 dir={} （asset_restore 说：{e}）",
                    d.display()
                ));
                "已取回解锁组件（按文件指纹确认后删除）".to_owned()
            }
        };
        return Ok(if d == game_dir {
            msg
        } else {
            format!("{msg}（组件原本在 {}）", d.display())
        });
    }
    crate::log::line(&format!(
        "[sm] 附近几个目录都没有我们的组件：{}",
        cands
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(" | ")
    ));
    // 一个都没有：可能是别的 Mod；也可能这里从没放过，那就别报错说"取回失败"。
    Ok("这几个目录里没有本工具的解锁组件（可能已经取回过了）".to_owned())
}

/// 删一个文件，失败时重试几次；还是不行就把**为什么**说清楚。
///
/// 为什么值得重试：游戏刚退出时进程可能还没完全走完（或杀毒/同步软件正握着文件），
/// 隔一下再删往往就成了。真删不掉时，用户看到的现象是「关闭成功、徽章却还写着
/// 已被 Smooth Motion 占用」—— 那句话必须能直接告诉他去关掉谁。
fn remove_with_retry(path: &std::path::Path) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..3 {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = e.to_string();
                // 文件已经不在也算成功（并发/别的东西先删了）
                if !path.exists() {
                    return Ok(());
                }
                if attempt < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
            }
        }
    }
    // 把现状写进日志：远程排查时这就是唯一线索
    crate::log::line(&format!(
        "[sm] 取回组件失败 path={} err={} still_exists={} readonly={}",
        path.display(),
        last,
        path.exists(),
        std::fs::metadata(path)
            .map(|m| m.permissions().readonly())
            .unwrap_or(false)
    ));
    Err(format!(
        "解锁组件还留在游戏目录里（{}）：{last}。\n\n请把游戏（包括它的启动器）完全退出后，再点一次「关闭 Smooth Motion」；\n如果还不行，多半是杀毒软件/同步盘正占着这个文件，可以手动删掉它，或者告诉我。",
        path.display()
    ))
}

/// 取回内置解锁组件：有备份就还原原文件，没有就删掉我们放的那份。
pub fn asset_restore(game_dir: &std::path::Path) -> Result<String, String> {
    let rec = asset_record_path(game_dir)?;
    let has_component = has_our_asset(game_dir);
    let has_record = rec.is_file();
    crate::log::line(&format!(
        "[sm] 取回组件开始 dir={} 组件在={} 部署记录={}",
        game_dir.display(),
        has_component,
        has_record
    ));
    if !rec.is_file() {
        // 没有记录：凡是我们的文件（按指纹）就清掉；否则不动
        let mut removed: Vec<String> = Vec::new();
        for rel in known_asset_rels() {
            let p = game_dir.join(rel);
            if is_our_asset_file(&p) {
                remove_with_retry(&p)?;
                removed.push(rel.to_string());
            }
        }
        if !removed.is_empty() {
            crate::log::line(&format!(
                "[sm] 取回组件完成（无记录、按指纹删除）dir={} files={}",
                game_dir.display(),
                removed.join("、")
            ));
            return Ok("已取回解锁组件（没有部署记录，按文件指纹确认后清理）".to_owned());
        }
        crate::log::line(&format!(
            "[sm] 取回组件：没有记录、指纹也不匹配，未改动 dir={} 组件在={}",
            game_dir.display(),
            has_component
        ));
        return Err("没有本工具的部署记录，未做任何改动".to_owned());
    }
    let text = std::fs::read_to_string(&rec).map_err(|e| format!("{e}"))?;
    let record: AssetRecord = serde_json::from_str(&text)
        .map_err(|e| format!("部署记录读不出来（{e}），不敢按它还原"))?;
    // v1 单文件记录 → 归一化成 files 列表
    let files: Vec<AssetFileRecord> = if record.files.is_empty() {
        vec![AssetFileRecord {
            rel: SM_ASSET_FILE.to_string(),
            had_original: record.had_original,
            backup: record.backup.clone(),
            sha: record.sha.clone(),
        }]
    } else {
        record.files.clone()
    };
    let mut restored: Vec<String> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for f in &files {
        let p = game_dir.join(&f.rel);
        if f.had_original {
            if let Some(bp) = &f.backup {
                let bp_path = std::path::Path::new(bp);
                if bp_path.is_file() {
                    /* 备份本身如果是**我们的组件**（老版本部署时被误备份进去的），
                    不能"还原"——那等于把组件又装回去（用户实测：关了组件还在）。
                    这种情况直接删掉目标文件，让目录回到干净状态。 */
                    if is_our_asset_file(bp_path) {
                        if p.is_file() {
                            remove_with_retry(&p)?;
                            removed.push(f.rel.clone());
                        }
                        let _ = std::fs::remove_file(bp_path);
                        continue;
                    }
                    std::fs::copy(bp_path, &p).map_err(|e| format!("还原原文件失败：{e}"))?;
                    let _ = std::fs::remove_file(bp_path);
                    restored.push(f.rel.clone());
                    continue;
                }
            }
        }
        // 没有可还原的原件：是我们的就删掉；不是我们的（内容对不上）保留并说明
        if p.is_file() && is_our_asset_file(&p) {
            remove_with_retry(&p)?;
            removed.push(f.rel.clone());
        } else if p.is_file() {
            kept.push(f.rel.clone());
        }
    }
    // 记录之外遗留的我们的文件（上次部署中断等）也一并清掉
    for rel in known_asset_rels() {
        if files.iter().any(|f| f.rel == rel) {
            continue;
        }
        let p = game_dir.join(rel);
        if is_our_asset_file(&p) {
            remove_with_retry(&p)?;
            removed.push(rel.to_string());
        }
    }
    let _ = std::fs::remove_file(&rec);
    crate::log::line(&format!(
        "[sm] 取回组件完成 dir={} 还原={} 删除={} 保留={}",
        game_dir.display(),
        restored.join("、"),
        removed.join("、"),
        kept.join("、")
    ));
    let mut msg = String::new();
    if !restored.is_empty() {
        msg.push_str(&format!("已还原 {}", restored.join("、")));
    }
    if !removed.is_empty() {
        msg.push_str(&format!(
            "{}已取回 {}",
            if msg.is_empty() { "" } else { "；" },
            removed.join("、")
        ));
    }
    if !kept.is_empty() {
        msg.push_str(&format!(
            "{}这些文件不是我们放的，已保留：{}",
            if msg.is_empty() { "" } else { "；" },
            kept.join("、")
        ));
    }
    if msg.is_empty() {
        msg = "目录里没有需要取回的组件".to_owned();
    }
    Ok(msg)
}

/// 【只读探测】给诊断命令用的会话入口（不写任何配置）。
pub fn open_session_for_probe() -> Result<nv::Session, String> {
    nv::Session::open()
}

/// 这个目录里部署的 SM 组件是不是**旧于**当前内置资产？
///
/// 判定不看版本号，而是拿部署记录里的文件哈希和当前内置文件比 —— 20/30 系
/// 哪一系列真变了才提示（记录缺哈希视为旧）。
pub fn is_outdated(game_dir: &std::path::Path) -> bool {
    let Ok(rec_path) = asset_record_path(game_dir) else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(rec_path) else {
        return false;
    };
    let Ok(rec) = serde_json::from_str::<AssetRecord>(&text) else {
        return false;
    };
    let files = if rec.files.is_empty() {
        vec![AssetFileRecord {
            rel: SM_ASSET_FILE.to_string(),
            had_original: rec.had_original,
            backup: rec.backup.clone(),
            sha: rec.sha.clone(),
        }]
    } else {
        rec.files.clone()
    };
    if files.is_empty() {
        return false;
    }
    let comp = if files.iter().any(|f| f.rel == "normal.asi") {
        SmComponent::Rtx20
    } else {
        SmComponent::Rtx30
    };
    let Ok(base) = asset_base() else { return false };
    for f in &files {
        let Some(cur) = f.sha.as_ref() else {
            return true;
        };
        let Some((_, src_rel, _)) = component_files(comp)
            .into_iter()
            .find(|(rel, _, _)| *rel == f.rel)
        else {
            continue;
        };
        let src = base.join(src_rel.replace('/', "\\"));
        let Ok(want) = crate::util::sha256_file(&src) else {
            continue;
        };
        if !want.eq_ignore_ascii_case(cur) {
            return true;
        }
    }
    false
}
