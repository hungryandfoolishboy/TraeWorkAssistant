//! CA 证书状态与安装（P4-9 Rust 化）：
//! - 生成：直调 [`crate::device_proxy::ca::ensure_ca`]（兼容 Python 版 RSA CA，
//!   缺失则用 rcgen 生成，布局 data/certs/{ca.crt,ca.key,ca.cer} 不变）；
//!   原「device_proxy.py --gen-ca + pip 依赖自愈」链路随 Python 移除一并删除。
//! - 安装：certutil 管理员写入受信任根（PowerShell RunAs 触发 UAC），流程不变。

use std::os::windows::process::CommandExt;
use std::process::Command;
use tauri::{AppHandle, State};

use crate::state::AppState;

#[derive(serde::Serialize)]
pub struct CertStatus {
    pub installed: bool,
}

const CREATE_NO_WINDOW: u32 = 0x08000000;

/// 探测文件当前用户可读（空 DACL 等 ACL 损坏时返回 false）
fn file_readable(p: &std::path::Path) -> bool {
    std::fs::File::open(p).is_ok()
}

/// 0x80070005（E_ACCESSDENIED）的 i32 表示：进程退出码按有符号解释为 -2147024891
const E_ACCESSDENIED: i32 = 0x80070005u32 as i32;

/// certutil/PowerShell 失败退出码 → 人话（5=Win32 拒绝访问；0x80070005=HRESULT 拒绝访问；1223=用户取消 UAC）
fn explain_certutil_exit(code: Option<i32>) -> String {
    match code {
        Some(1223) => "用户取消了 UAC 授权".into(),
        Some(5) | Some(E_ACCESSDENIED) => {
            "证书文件权限不足（拒绝访问），可尝试删除数据目录下 certs 文件夹后重试".into()
        }
        Some(_) => "可能需要管理员权限或证书文件不可读".into(),
        None => "进程异常退出".into(),
    }
}

#[tauri::command(async)]
pub fn cert_status(_app: AppHandle, _state: State<AppState>) -> CertStatus {
    let out = Command::new("certutil")
        .args(["-store", "Root"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let installed = match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.contains("TraeDeviceProxyCA")
        }
        Err(_) => false,
    };
    CertStatus { installed }
}

#[tauri::command(async)]
pub fn cert_install(app: AppHandle, state: State<AppState>) -> Result<CertStatus, String> {
    // 1. 确保 CA 证书已生成（data_dir/certs/ca.cer；ensure_ca 内部兼容历史 RSA CA）
    let certs_dir = state.path("certs");
    crate::device_proxy::ca::ensure_ca(&certs_dir)?;
    let cer = certs_dir.join("ca.cer");
    let cer_arg = cer.to_string_lossy().replace('\\', "/").to_string();

    // 2. 管理员权限安装到本地计算机受信任根证书颁发机构（触发 UAC）：
    //    路径加引号防止含空格时被拆参；-PassThru + exit 取 certutil 真实退出码；
    //    try/catch 把「用户取消 UAC」映射为 1223（取消时 Start-Process 抛错 →
    //    $p 为 null → exit $null 会误报成功，随后只能靠复查根存储兜底且文案误导）
    let ps = format!(
        "try {{ $p = Start-Process certutil -ArgumentList '-addstore','-f','Root','\"{}\"' -Verb RunAs -Wait -PassThru -ErrorAction Stop; exit $p.ExitCode }} catch {{ exit 1223 }}",
        cer_arg
    );
    let mut status = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("启动证书安装失败: {e}"))?;

    // 3. 失败自愈：certutil 失败常见根因是证书文件 ACL 异常（历史版本收紧 certs
    //    目录可能留下空 DACL，certutil 提权后也读不到 ca.cer，UAC 允许后控制台一闪
    //    而过即退出）。探测文件可读性，不可读则 icacls /reset 恢复继承后重试一次。
    if !status.success() && !file_readable(&cer) {
        let _ = Command::new("icacls")
            .arg(&certs_dir)
            .args(["/reset", "/T"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        if file_readable(&cer) {
            status = Command::new("powershell")
                .args(["-NoProfile", "-Command", &ps])
                .creation_flags(CREATE_NO_WINDOW)
                .status()
                .map_err(|e| format!("启动证书安装失败: {e}"))?;
        }
    }

    if !status.success() {
        return Err(format!(
            "证书安装被取消或失败（{}；certutil 退出码 {:?}）",
            explain_certutil_exit(status.code()),
            status.code()
        ));
    }

    // 4. 复查根存储，防止「命令成功但证书未生效」的误报
    let result = cert_status(app, state);
    if !result.installed {
        return Err("证书安装命令已执行，但根证书存储中未找到 TraeDeviceProxyCA，请检查系统策略".into());
    }
    Ok(result)
}
