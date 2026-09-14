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
    //    路径加引号防止含空格时被拆参；-PassThru + exit 取 certutil 真实退出码（-Wait 不取退出码会误报成功）
    let ps = format!(
        "$p = Start-Process certutil -ArgumentList '-addstore','-f','Root','\"{}\"' -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        cer_arg
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("启动证书安装失败: {e}"))?;

    if !status.success() {
        return Err(format!(
            "证书安装被取消或失败（可能需要管理员权限；certutil 退出码 {:?}）",
            status.code()
        ));
    }

    // 3. 复查根存储，防止「命令成功但证书未生效」的误报
    let result = cert_status(app, state);
    if !result.installed {
        return Err("证书安装命令已执行，但根证书存储中未找到 TraeDeviceProxyCA，请检查系统策略".into());
    }
    Ok(result)
}
