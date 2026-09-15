//! 文件签名校验（严格版，给「手动导入」用）。
//!
//! 和 scan::identify_dll 的分工要说清：
//!   * identify_dll —— **宽松**。只回答「这个文件能不能安全覆盖」：签名者名字像本项目的就算。
//!     它靠的是在证书里找字符串，能被人自签一张同名证书糊弄过去，所以**不能**用来判断
//!     「这份文件是不是官方发布的那一份」。
//!   * 本模块 —— **严格**。先让 Windows 把签名验一遍（能查出内容有没有被改），
//!     再核对签名者身份：作者自签证书必须是名单里的那张（比指纹），
//!     NVIDIA 的必须能链到受信任的根（公开 CA，别人签不出「NVIDIA Corporation」）。
//!
//! 为什么要这么严：手动导入的文件（网盘下载、别人转发）不受我们控制，
//! 只认内容，不认来源 —— 只要签名和指纹都对，就等价于「从官方拿的」。

use std::path::Path;

use anyhow::{bail, Result};
use windows_sys::Win32::Security::Cryptography::{
    CertCloseStore, CertFindCertificateInStore, CertFreeCertificateContext,
    CertGetCertificateContextProperty, CertGetNameStringW, CryptMsgClose, CryptMsgGetParam,
    CryptQueryObject,
    CERT_FIND_SUBJECT_CERT, CERT_INFO, CERT_NAME_SIMPLE_DISPLAY_TYPE, CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
    CERT_QUERY_FORMAT_FLAG_BINARY, CERT_QUERY_OBJECT_FILE, CERT_SHA1_HASH_PROP_ID,
    CMSG_SIGNER_INFO, CMSG_SIGNER_INFO_PARAM, HCERTSTORE, PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
};
use windows_sys::Win32::Security::WinTrust::{
    WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0,
    WINTRUST_FILE_INFO,
    WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_DISABLE_MD2_MD4, WTD_REVOKE_NONE,
    WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
};

/// 作者自签证书的 SHA-1 指纹（上游代理包 0.3.0 起，"DLSSG for SM86"）。
pub const AUTHOR_CERT_PROXY: &str = "85BA66762F851E49148D706915D09026281418E6";

/// 作者自签证书的 SHA-1 指纹（0.2.4 native 包，"DLSSG Native Project"）。
/// 实测取自 archive/0.2.4/version.dll —— 上游换过一次证书，两张都得认：
/// 否则导入上游源码 zip 时，里面 archive/0.2.4/ 那一堆文件会被误判成「签名者不认识」。
pub const AUTHOR_CERT_NATIVE: &str = "A994735E6A7E9AA31FA926B3023B7C487DAB4850";

/// NVIDIA 的签名证书（310.9.1 那批）SHA-1 指纹。
/// 只是兜底：NVIDIA 走的是公开 CA，正常情况下 Windows 认可就够，换证书也不影响。
pub const NVIDIA_CERT: &str = "7B7B0B6697AFB438CF6F65A155F00E86676FB186";

/// Windows 对签名的结论
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigState {
    /// 签名有效，证书链也能到受信任的根（NVIDIA 的文件就是这种）
    Trusted,
    /// 签名有效，但根证书不受信任（作者自签就是这种）—— 内容没被改，只是 Windows 不认识这张证书
    UntrustedRoot,
    /// 没有签名
    NoSignature,
    /// 签名无效：内容被改过
    BadDigest,
    /// 其它失败（证书过期、吊销检查失败等）
    Other,
}

impl SigState {
    pub fn label(self) -> &'static str {
        match self {
            SigState::Trusted => "签名有效（受信任的证书链）",
            SigState::UntrustedRoot => "签名有效（自签证书，Windows 不信任这张证书本身）",
            SigState::NoSignature => "没有签名",
            SigState::BadDigest => "签名无效：内容被改过",
            SigState::Other => "签名校验失败（证书过期或被吊销等）",
        }
    }
    /// 签名本身是不是「内容没被改过」
    pub fn content_ok(self) -> bool {
        matches!(self, SigState::Trusted | SigState::UntrustedRoot)
    }
}

/// 签名者是谁
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignerKind {
    /// 本项目作者（指纹命中固定名单）
    Author,
    /// NVIDIA（Windows 认可 + 主体是 NVIDIA，或指纹命中）
    Nvidia,
    /// 不认识
    Other,
}

impl SignerKind {
    pub fn label(self) -> &'static str {
        match self {
            SignerKind::Author => "本项目作者",
            SignerKind::Nvidia => "NVIDIA",
            SignerKind::Other => "不认识",
        }
    }
}

/// 一份文件的校验结论
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub sig: SigState,
    /// 签名者主体名（拿不到就是空）
    pub subject: String,
    /// 签名者证书 SHA-1 指纹，大写十六进制（拿不到就是空）
    pub thumbprint: String,
    pub kind: SignerKind,
    /// 拿签名者信息时的错误（不影响结论，只用来解释）
    pub note: Option<String>,
}

impl VerifyReport {
    /// 内容可不可信：签名有效（没被改）**并且**签名者是我们认识的这两家之一。
    ///
    /// 这是「静默通过」的判据。
    pub fn content_trusted(&self) -> bool {
        if !self.sig.content_ok() {
            return false;
        }
        match self.kind {
            // NVIDIA 走公开 CA：Windows 认可 = 只有 NVIDIA 能签出来
            SignerKind::Nvidia => self.sig == SigState::Trusted,
            // 作者是自签：必须指纹命中我们的固定名单
            SignerKind::Author => true,
            SignerKind::Other => false,
        }
    }

    /// 给人看的一行结论
    pub fn summary(&self) -> String {
        format!(
            "{}；签名者：{}（{}）{}",
            self.sig.label(),
            if self.subject.is_empty() { "未知" } else { &self.subject },
            self.kind.label(),
            if self.thumbprint.is_empty() {
                String::new()
            } else {
                format!("，证书指纹 {}", self.thumbprint)
            }
        )
    }
}

fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 让 Windows 把签名验一遍。返回原始 HRESULT 之外的分类结论。
fn winverify(path: &Path) -> SigState {
    unsafe {
        let wpath = wide(path);
        let mut file_info = WINTRUST_FILE_INFO {
            cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
            pcwszFilePath: wpath.as_ptr(),
            ..Default::default()
        };

        let mut data = WINTRUST_DATA {
            cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
            dwUIChoice: WTD_UI_NONE,
            fdwRevocationChecks: WTD_REVOKE_NONE,
            dwUnionChoice: WTD_CHOICE_FILE,
            Anonymous: WINTRUST_DATA_0 { pFile: &mut file_info },
            dwStateAction: WTD_STATEACTION_VERIFY,
            // 不联网吊销检查（离线也要能用），并且禁掉早就废弃的 MD2/MD4
            dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_DISABLE_MD2_MD4,
            ..Default::default()
        };

        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        let status = WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action,
            &mut data as *mut WINTRUST_DATA as *mut core::ffi::c_void,
        );
        // 无论成功失败都要收尾一次，否则会漏状态
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action,
            &mut data as *mut WINTRUST_DATA as *mut core::ffi::c_void,
        );

        match status {
            0 => SigState::Trusted,
            // CERT_E_UNTRUSTEDROOT：签名没问题，只是根证书不受信任（自签证书的正常结果）
            s if s as u32 == 0x800B_0109 => SigState::UntrustedRoot,
            // TRUST_E_NOSIGNATURE / TRUST_E_SUBJECT_FORM_UNKNOWN / TRUST_E_PROVIDER_UNKNOWN
            s if (s as u32 == 0x800B_0100)
                || (s as u32 == 0x800B_0003)
                || (s as u32 == 0x800B_0002) =>
            {
                SigState::NoSignature
            }
            // TRUST_E_BAD_DIGEST：内容被改过（这是最关键的一条）
            s if s as u32 == 0x8009_6010 => SigState::BadDigest,
            _ => SigState::Other,
        }
    }
}

/// 取出「真正签名的那张证书」——不是证书包里随便一张。
///
/// 做法是 MSDN 的标准路子：从 PKCS#7 里读 SignerInfo，用它的 Issuer + SerialNumber
/// 去证书库里查那张证书。**不能**改成「取证书库里的第一张」——
/// 那样子别人只要把作者的证书塞进包里、用自己的私钥签名就能骗过去。
fn signer_cert(path: &Path) -> Result<(String, String)> {
    unsafe {
        let wpath = wide(path);
        let (mut encoding, mut content, mut format) = (0u32, 0u32, 0u32);
        let mut store: HCERTSTORE = std::ptr::null_mut();
        let mut msg: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut ctx: *mut core::ffi::c_void = std::ptr::null_mut();

        let ok = CryptQueryObject(
            CERT_QUERY_OBJECT_FILE,
            wpath.as_ptr() as *const core::ffi::c_void,
            CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
            CERT_QUERY_FORMAT_FLAG_BINARY,
            0,
            &mut encoding,
            &mut content,
            &mut format,
            &mut store,
            &mut msg,
            &mut ctx,
        );
        if ok == 0 || store.is_null() || msg.is_null() {
            bail!("读不出签名信息（没有签名或不是 PKCS#7）");
        }

        // 1. SignerInfo
        let mut cb: u32 = 0;
        if CryptMsgGetParam(
            msg,
            CMSG_SIGNER_INFO_PARAM,
            0,
            std::ptr::null_mut(),
            &mut cb,
        ) == 0
            || cb == 0
        {
            CryptMsgClose(msg);
            CertCloseStore(store, 0);
            bail!("读不出签名者信息");
        }
        let mut buf = vec![0u8; cb as usize];
        if CryptMsgGetParam(
            msg,
            CMSG_SIGNER_INFO_PARAM,
            0,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            &mut cb,
        ) == 0
        {
            CryptMsgClose(msg);
            CertCloseStore(store, 0);
            bail!("读不出签名者信息");
        }
        let si = &*(buf.as_ptr() as *const CMSG_SIGNER_INFO);

        // 2. 用 Issuer + SerialNumber 在证书库里定位那张证书
        let want = CERT_INFO {
            Issuer: si.Issuer,
            SerialNumber: si.SerialNumber,
            ..Default::default()
        };
        let cert = CertFindCertificateInStore(
            store,
            X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
            0,
            CERT_FIND_SUBJECT_CERT,
            &want as *const CERT_INFO as *const core::ffi::c_void,
            std::ptr::null(),
        );
        if cert.is_null() {
            CryptMsgClose(msg);
            CertCloseStore(store, 0);
            bail!("证书库里找不到签名者那张证书");
        }

        // 3. SHA-1 指纹
        let mut len: u32 = 0;
        CertGetCertificateContextProperty(cert, CERT_SHA1_HASH_PROP_ID, std::ptr::null_mut(), &mut len);
        let mut hash = vec![0u8; len as usize];
        let mut got_len = len;
        CertGetCertificateContextProperty(
            cert,
            CERT_SHA1_HASH_PROP_ID,
            hash.as_mut_ptr() as *mut core::ffi::c_void,
            &mut got_len,
        );
        let thumbprint: String = hash.iter().map(|b| format!("{b:02X}")).collect();

        // 4. 主体名
        let need = CertGetNameStringW(
            cert,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        );
        let mut name = vec![0u16; need.max(1) as usize];
        let written = CertGetNameStringW(
            cert,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            std::ptr::null(),
            name.as_mut_ptr(),
            name.len() as u32,
        );
        let subject = if written > 1 {
            String::from_utf16_lossy(&name[..(written as usize - 1)])
        } else {
            String::new()
        };

        CertFreeCertificateContext(cert);
        CryptMsgClose(msg);
        CertCloseStore(store, 0);
        Ok((subject, thumbprint))
    }
}

/// 校验一个文件。任何异常都不 panic —— 失败就走「可疑，让用户确认」那条路。
pub fn verify_file(path: &Path) -> VerifyReport {
    let sig = winverify(path);
    let (subject, thumbprint, note) = if sig == SigState::NoSignature {
        (String::new(), String::new(), None)
    } else {
        match signer_cert(path) {
            Ok((s, t)) => (s, t, None),
            Err(e) => (String::new(), String::new(), Some(e.to_string())),
        }
    };

    let kind = if thumbprint.is_empty() {
        // 拿不到指纹时只能靠主体名兜底（NVIDIA 那串名字别人签不出来，因为要公开 CA）
        if subject.to_ascii_lowercase().contains("nvidia") {
            SignerKind::Nvidia
        } else {
            SignerKind::Other
        }
    } else if thumbprint.eq_ignore_ascii_case(AUTHOR_CERT_PROXY)
        || (!AUTHOR_CERT_NATIVE.is_empty() && thumbprint.eq_ignore_ascii_case(AUTHOR_CERT_NATIVE))
    {
        SignerKind::Author
    } else if thumbprint.eq_ignore_ascii_case(NVIDIA_CERT)
        || subject.to_ascii_lowercase().contains("nvidia")
    {
        SignerKind::Nvidia
    } else {
        SignerKind::Other
    };

    VerifyReport {
        sig,
        subject,
        thumbprint,
        kind,
        note,
    }
}
