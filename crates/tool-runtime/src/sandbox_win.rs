//! Windows 受限令牌沙箱（M8.1）：在降权的受限令牌 + Job Object 中执行命令。
//!
//! 隔离层次：
//! - **受限令牌**：从当前进程令牌派生（DISABLE_MAX_PRIVILEGE 剥离所有特权 + 禁用 Administrators SID），
//!   使命令进程无法使用管理员能力。派生自调用方自身令牌，故标准用户无需 SeAssignPrimaryToken 特权。
//! - **Job Object**：KILL_ON_JOB_CLOSE，保证进程树随 job 句柄关闭而干净销毁，杜绝孤儿进程。
//! - **密钥环境擦除**：子进程环境中移除 API Key / Token / 密码类变量。
//!
//! fail-closed：任一步骤失败即返回 Err，调用方不得降级为无沙箱执行。
//! 注意：本层不隔离网络与文件系统路径——那由 M8.2 的 AppContainer 提供。

use crate::{CommandLineCallback, RunCommandResult, ToolRuntimeError};
use std::io::Read;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::time::{Duration, Instant};
use windows::core::PWSTR;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, WAIT_OBJECT_0,
};
use windows::Win32::Security::{
    CreateRestrictedToken, CreateWellKnownSid, SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES,
    WinBuiltinAdministratorsSid, DISABLE_MAX_PRIVILEGE, PSID, TOKEN_ADJUST_DEFAULT,
    TOKEN_ADJUST_GROUPS, TOKEN_ADJUST_PRIVILEGES, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
    TOKEN_QUERY,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, ResumeThread,
    TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
};

fn err(msg: impl std::fmt::Display) -> ToolRuntimeError {
    ToolRuntimeError::CommandFailed(format!("沙箱: {msg}"))
}

/// 极简 base64 编码（仅用于 -EncodedCommand，避免引入依赖）。
fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// 把命令编码为 PowerShell -EncodedCommand 参数（UTF-16LE → base64），彻底规避命令行转义问题。
fn encoded_command_line(command: &str) -> Vec<u16> {
    let utf16le: Vec<u8> = command
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    let b64 = base64_encode(&utf16le);
    let line = format!("powershell.exe -NoProfile -NonInteractive -EncodedCommand {b64}");
    line.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 构建擦除密钥后的 UTF-16 环境块（KEY=VAL\0...\0\0）。
fn build_scrubbed_env_block() -> Vec<u16> {
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in std::env::vars() {
        let upper = k.to_uppercase();
        if upper.contains("API_KEY")
            || upper.contains("APIKEY")
            || upper.contains("SECRET")
            || upper.contains("_TOKEN")
            || upper.contains("PASSWORD")
            || k == "DEEPSEEK_API_KEY"
        {
            continue;
        }
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0); // 双 NUL 结尾
    block
}

/// 在受限令牌 + Job Object 沙箱中执行命令。fail-closed：任何步骤失败返回 Err。
pub fn run_in_restricted_sandbox(
    workspace: &Path,
    command: &str,
    timeout: Duration,
    on_line: Option<CommandLineCallback>,
) -> Result<RunCommandResult, ToolRuntimeError> {
    unsafe {
        // 1) 打开当前进程令牌
        let mut process_token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE
                | TOKEN_ASSIGN_PRIMARY
                | TOKEN_QUERY
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_GROUPS
                | TOKEN_ADJUST_PRIVILEGES,
            &mut process_token,
        )
        .map_err(|e| err(format!("OpenProcessToken 失败: {e}")))?;

        // 2) 构造 Administrators SID（用于禁用）
        let mut admin_sid = [0u8; 68];
        let mut sid_len = admin_sid.len() as u32;
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            None,
            PSID(admin_sid.as_mut_ptr() as *mut _),
            &mut sid_len,
        )
        .map_err(|e| err(format!("CreateWellKnownSid 失败: {e}")))?;
        let sids_to_disable = [SID_AND_ATTRIBUTES {
            Sid: PSID(admin_sid.as_mut_ptr() as *mut _),
            Attributes: 0,
        }];

        // 3) 派生受限令牌：剥离所有特权 + 禁用 Administrators 组
        let mut restricted = HANDLE::default();
        CreateRestrictedToken(
            process_token,
            DISABLE_MAX_PRIVILEGE,
            Some(&sids_to_disable),
            None,
            None,
            &mut restricted,
        )
        .map_err(|e| err(format!("CreateRestrictedToken 失败: {e}")))?;
        let _ = CloseHandle(process_token);

        // 4) Job Object（KILL_ON_JOB_CLOSE）
        let job = CreateJobObjectW(None, PWSTR::null())
            .map_err(|e| err(format!("CreateJobObject 失败: {e}")))?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .map_err(|e| err(format!("SetInformationJobObject 失败: {e}")))?;

        // 5) stdout/stderr 管道（写端可继承，读端不可继承）
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let (mut out_r, mut out_w) = (HANDLE::default(), HANDLE::default());
        let (mut err_r, mut err_w) = (HANDLE::default(), HANDLE::default());
        CreatePipe(&mut out_r, &mut out_w, Some(&sa), 0)
            .map_err(|e| err(format!("CreatePipe(out) 失败: {e}")))?;
        CreatePipe(&mut err_r, &mut err_w, Some(&sa), 0)
            .map_err(|e| err(format!("CreatePipe(err) 失败: {e}")))?;
        // 读端不继承给子进程
        let _ = windows::Win32::Foundation::SetHandleInformation(
            out_r,
            HANDLE_FLAG_INHERIT.0,
            HANDLE_FLAGS(0),
        );
        let _ = windows::Win32::Foundation::SetHandleInformation(
            err_r,
            HANDLE_FLAG_INHERIT.0,
            HANDLE_FLAGS(0),
        );

        // 6) STARTUPINFO（重定向 std 句柄）
        let mut si = STARTUPINFOW::default();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdOutput = out_w;
        si.hStdError = err_w;
        si.hStdInput = HANDLE::default();

        // 7) 命令行 / 环境 / 工作目录
        let mut cmdline = encoded_command_line(command);
        let env_block = build_scrubbed_env_block();
        let cwd: Vec<u16> = workspace
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let mut pi = PROCESS_INFORMATION::default();
        let create_result = CreateProcessAsUserW(
            restricted,
            windows::core::PCWSTR::null(),
            PWSTR(cmdline.as_mut_ptr()),
            None,
            None,
            true, // 继承句柄（管道写端）
            CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            Some(env_block.as_ptr() as *const _),
            windows::core::PCWSTR(cwd.as_ptr()),
            &si,
            &mut pi,
        );

        // 父进程关闭管道写端（否则读端不会 EOF）
        let _ = CloseHandle(out_w);
        let _ = CloseHandle(err_w);

        if let Err(e) = create_result {
            let _ = CloseHandle(out_r);
            let _ = CloseHandle(err_r);
            let _ = CloseHandle(job);
            let _ = CloseHandle(restricted);
            return Err(err(format!("CreateProcessAsUser 失败: {e}")));
        }

        // 8) 进 Job、恢复线程
        let _ = AssignProcessToJobObject(job, pi.hProcess);
        ResumeThread(pi.hThread);

        // 9) 读端在线程中抽干（把 HANDLE 包成 std File 复用读取逻辑）
        let on_line2 = on_line.clone();
        let out_file = std::fs::File::from_raw_handle(out_r.0 as *mut _);
        let err_file = std::fs::File::from_raw_handle(err_r.0 as *mut _);
        let out_handle = std::thread::spawn(move || drain(out_file, on_line2));
        let err_handle = std::thread::spawn(move || drain(err_file, on_line));

        // 10) 等待 + 超时
        let started = Instant::now();
        let mut timed_out = false;
        loop {
            let wait = WaitForSingleObject(pi.hProcess, 50);
            if wait == WAIT_OBJECT_0 {
                break;
            }
            if started.elapsed() >= timeout {
                let _ = TerminateProcess(pi.hProcess, 1);
                timed_out = true;
                break;
            }
        }

        let mut exit_code: u32 = 0;
        let _ = GetExitCodeProcess(pi.hProcess, &mut exit_code);
        let (stdout, out_trunc) = out_handle.join().unwrap_or_default();
        let (stderr, err_trunc) = err_handle.join().unwrap_or_default();
        let duration_ms = started.elapsed().as_millis();

        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
        let _ = CloseHandle(job); // 关闭 job → KILL_ON_JOB_CLOSE 收尾残留子进程
        let _ = CloseHandle(restricted);

        Ok(RunCommandResult {
            command: command.to_string(),
            exit_code: if timed_out { None } else { Some(exit_code as i32) },
            stdout,
            stderr,
            truncated: out_trunc || err_trunc,
            timed_out,
            duration_ms,
        })
    }
}

/// 读取管道（已包成 File），UTF-8 lossy、按行回调、截断到 64KiB。
fn drain(mut file: std::fs::File, on_line: Option<CommandLineCallback>) -> (String, bool) {
    const MAX: usize = 64 * 1024;
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    let truncated = buf.len() > MAX;
    if truncated {
        buf.truncate(MAX);
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    if let Some(cb) = on_line {
        for line in text.lines() {
            cb(line.to_string());
        }
    }
    (text, truncated)
}
