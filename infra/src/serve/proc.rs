//! spawn/kill/pidfile/日志落盘。
//! 状态目录：`${XDG_STATE_HOME:-~/.local/state}/momo/`（pid/、logs/、active_profile），
//! 不提供 `MOMO_*` 覆盖键（口径见 momo-infra.md「状态目录与进程管理」）。
//! 探活/终止走系统调用（unix `waitpid`+`kill(2)`；windows `OpenProcess`/`TerminateProcess`/
//! Toolhelp32 后代表），不依赖 PATH 上的外部命令（kill/ps/tasklist 均已移除；
//! windows 的 taskkill 只做温和请求/兜底，缺失不阻塞升级）。
//!
//! serve 组件 spawn 时隔离进程组：`uv run` 这类 wrapper 的孙进程（python 服务
//! 本体）继承同组/同父链，终止按组收口——wrapper pid 死掉后残留孙进程也能被
//! 收掉（#149：曾 abort/down 只杀 wrapper，孙进程持端口致下次 up 端口冲突）。

use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime};

use crate::output::AppError;

/// `${XDG_STATE_HOME:-~/.local/state}/momo`
pub fn state_dir() -> PathBuf {
    match std::env::var("XDG_STATE_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v).join("momo"),
        _ => crate::env_cfg::expand_tilde("~/.local/state/momo"),
    }
}

pub fn pid_dir() -> PathBuf {
    state_dir().join("pid")
}

pub fn log_dir() -> PathBuf {
    state_dir().join("logs")
}

/// `active_profile` 状态文件（switch 写，serve 读）。
pub fn active_profile_file() -> PathBuf {
    state_dir().join("active_profile")
}

pub fn pidfile(component: &str) -> PathBuf {
    pid_dir().join(format!("{component}.pid"))
}

pub fn read_pidfile(component: &str) -> Result<u32, AppError> {
    let text = fs::read_to_string(pidfile(component))?;
    text.trim()
        .parse()
        .map_err(|e| AppError::Config(format!("bad pidfile for {component}: {e}")))
}

pub fn write_pidfile(component: &str, pid: u32) -> Result<(), AppError> {
    fs::create_dir_all(pid_dir())?;
    fs::write(pidfile(component), format!("{pid}\n"))?;
    Ok(())
}

pub fn remove_pidfile(component: &str) -> Result<(), AppError> {
    let p = pidfile(component);
    if p.exists() {
        fs::remove_file(p)?;
    }
    Ok(())
}

/// 组件日志落盘：`logs/<name>.log`，stdout/stderr 追加写同一文件。
pub fn log_stdio(component: &str) -> Result<(PathBuf, Stdio, Stdio), AppError> {
    fs::create_dir_all(log_dir())?;
    let path = log_dir().join(format!("{component}.log"));
    let out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let err = out.try_clone()?;
    Ok((path, Stdio::from(out), Stdio::from(err)))
}

/// spawn 前置：unix `process_group(0)` 让 child 自成进程组（pgid == child
/// pid，即 pidfile 记录的 pid），孙进程继承同组，终止按组收口。
/// windows 无前置配置——后代归属靠 `th32ParentProcessID` 快照追溯。
pub fn prepare_spawn(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// 优雅停止：先发 SIGTERM（win: taskkill 温和请求），`grace` 内轮询存活；
/// 未退出再 SIGKILL（win: TerminateProcess）。返回进程最终已退出。
pub fn stop_pid(pid: u32, grace: Duration) -> bool {
    if !is_alive(pid) {
        return true;
    }
    term_pid(pid);
    if wait_dead(pid, grace) {
        return true;
    }
    force_pid(pid);
    wait_dead(pid, Duration::from_secs(2))
}

/// 组件级存活：`is_alive(pid)` 之外补查后代存活——
/// unix `kill(-pid, 0)` 进程组仍有成员（wrapper 死后孙进程残留），
/// windows Toolhelp 后代表仍有成员。serve down/up/status 用此口径
/// 收孤儿（#149）；jobs runner 仍用单 pid `is_alive`/`stop_pid`。
pub fn component_alive(component: &str, pid: u32) -> bool {
    if is_alive(pid) {
        return true;
    }
    group_alive(component, pid)
}

/// 组关系存活：pid 本体已死，但其进程组/后代链里还有成员。
#[cfg(unix)]
fn group_alive(_component: &str, pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // `kill(2)` 负 pid 是进程组语义：送达或 EPERM → 组内仍有活进程。
    unsafe {
        if libc::kill(-(pid as i32), 0) == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(windows)]
fn group_alive(_component: &str, pid: u32) -> bool {
    !descendants(pid).is_empty()
}

#[cfg(not(any(unix, windows)))]
fn group_alive(_component: &str, _pid: u32) -> bool {
    false
}

/// 组件级优雅停止（serve down / up 孤儿清理）：先温和终止，`grace` 内轮询
/// 组关系存活；未退出再强杀。pid 本体已死只剩孤儿时跳过温和阶段（温和
/// 请求无处投递）。返回组件（含孙进程）最终已全部退出。
///
/// unix：信号同时发 `pid` 与进程组 `-pid`（`prepare_spawn` 定了 pgid==pid，
/// pid 被组外进程复用时仍有单 pid 兜底）。
/// windows：温和/强杀两段都按 `taskkill /T` + Toolhelp 后代表逐个终止
/// 收口（孤儿分支靠 ppid 归因）。
pub fn stop_component(component: &str, pid: u32, grace: Duration) -> bool {
    if !component_alive(component, pid) {
        return true;
    }
    if is_alive(pid) {
        term_component(component, pid);
        if wait_component_dead(component, pid, grace) {
            return true;
        }
    }
    force_component(component, pid);
    wait_component_dead(component, pid, Duration::from_secs(2))
}

/// 单次强杀整组/整树（serve up abort 清理 owned children）。
/// unix `SIGKILL` 进程组+pid；windows `taskkill /F /T` + Toolhelp 后代
/// 逐个终止 + `TerminateProcess` 兜底。返回是否有信号送达。
pub fn kill_component(component: &str, pid: u32) -> bool {
    force_component(component, pid)
}

fn wait_component_dead(component: &str, pid: u32, timeout: Duration) -> bool {
    let start = Instant::now();
    while component_alive(component, pid) {
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// 温和终止：unix 对 pid 与进程组 `-pid` 同发 SIGTERM；
/// windows 对 pid 与全部后代逐个 `taskkill`（不带 /F 温和请求——
/// `/T` 覆盖活树，Toolhelp 后代表补孤儿分支）。
fn term_component(component: &str, pid: u32) -> bool {
    #[cfg(unix)]
    {
        let _ = component;
        let group = send_signal_group(pid, libc::SIGTERM);
        let single = send_signal(pid, libc::SIGTERM);
        group || single
    }
    #[cfg(windows)]
    {
        let _ = component;
        let mut ok = taskkill(pid, false);
        for d in descendants(pid) {
            ok |= taskkill(d, false);
        }
        ok
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (component, pid);
        false
    }
}

/// 升级强杀：unix 对进程组 `-pid` 与 pid 同发 SIGKILL；
/// windows `taskkill /F /T` 收活树 + Toolhelp 后代表逐个 `taskkill /F`
/// 与 `TerminateProcess` 兜底（孤儿分支父链已断，靠 ppid 归因）。
fn force_component(component: &str, pid: u32) -> bool {
    #[cfg(unix)]
    {
        let _ = component;
        let group = send_signal_group(pid, libc::SIGKILL);
        let single = send_signal(pid, libc::SIGKILL);
        group || single
    }
    #[cfg(windows)]
    {
        let _ = component;
        let mut ok = taskkill(pid, true);
        ok |= terminate_process(pid);
        for d in descendants(pid) {
            ok |= taskkill(d, true);
            ok |= terminate_process(d);
        }
        ok
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (component, pid);
        false
    }
}

/// unix `kill(2)` 进程组语义：信号发 `-pgid` 全组（spawn 时 pgid==pidfile pid）。
#[cfg(unix)]
fn send_signal_group(pgid: u32, sig: i32) -> bool {
    if pgid == 0 || pgid > i32::MAX as u32 {
        return false;
    }
    unsafe { libc::kill(-(pgid as i32), sig) == 0 }
}

/// windows `taskkill /PID <pid> /T [/F]`：树级终止，`force` 追加 `/F`。
#[cfg(windows)]
fn taskkill(pid: u32, force: bool) -> bool {
    let mut args = vec!["/PID".to_string(), pid.to_string(), "/T".to_string()];
    if force {
        args.push("/F".to_string());
    }
    std::process::Command::new("taskkill")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// windows 进程树后代：Toolhelp32 快照按 `th32ParentProcessID` BFS。
/// 父死后其 pid 仍记录在孩子的 parent 字段里，孤儿后代一样找得到。
/// 局限：中间层已死且其 pid 未被记录时，更深层孤儿无法归因——与组杀
/// 同水位，可接受。
#[cfg(windows)]
fn descendants(root_pid: u32) -> Vec<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut kids_of: std::collections::BTreeMap<u32, Vec<u32>> = Default::default();
        if Process32FirstW(snap, &mut e) != 0 {
            loop {
                kids_of
                    .entry(e.th32ParentProcessID)
                    .or_default()
                    .push(e.th32ProcessID);
                if Process32NextW(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        let mut out: Vec<u32> = Vec::new();
        let mut queue = vec![root_pid];
        while let Some(p) = queue.pop() {
            if let Some(kids) = kids_of.get(&p) {
                for &k in kids {
                    if !out.contains(&k) {
                        out.push(k);
                        queue.push(k);
                    }
                }
            }
        }
        out
    }
}

/// 单次终止信号：unix SIGTERM；windows `TerminateProcess`；其他平台 no-op。
pub fn kill_pid(pid: u32) -> bool {
    #[cfg(unix)]
    {
        send_signal(pid, libc::SIGTERM)
    }
    #[cfg(windows)]
    {
        terminate_process(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// 停止前置信号：unix SIGTERM；windows `taskkill /PID`（不带 /F 的温和请求）。
/// windows 侧 taskkill 缺失仅失去温和请求，`stop_pid` 仍会升级 TerminateProcess。
fn term_pid(pid: u32) -> bool {
    #[cfg(unix)]
    {
        send_signal(pid, libc::SIGTERM)
    }
    #[cfg(windows)]
    {
        std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// 升级强杀：unix SIGKILL；windows `TerminateProcess`（不依赖 taskkill）。
fn force_pid(pid: u32) -> bool {
    #[cfg(unix)]
    {
        send_signal(pid, libc::SIGKILL)
    }
    #[cfg(windows)]
    {
        terminate_process(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// unix `kill(2)` 封装：返回信号是否送达（EPERM 即「存在但无权限」也算未送达，
/// 由 `is_alive` 的 kill(pid,0) 分支另行判定存活）。
/// `kill(2)` 的 pid<=0 是进程组语义（0=本组、负数=进程组），不是 pid 语义：拒绝。
#[cfg(unix)]
fn send_signal(pid: u32, sig: i32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    unsafe { libc::kill(pid as i32, sig) == 0 }
}

/// windows 强杀原语：`OpenProcess(PROCESS_TERMINATE)` + `TerminateProcess`。
/// 打不开句柄（无此进程/无权限）→ false。
#[cfg(windows)]
fn terminate_process(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h == 0 {
            return false;
        }
        let ok = TerminateProcess(h, 1);
        CloseHandle(h);
        ok != 0
    }
}

fn wait_dead(pid: u32, timeout: Duration) -> bool {
    let start = Instant::now();
    while is_alive(pid) {
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// pid 存活探测，不依赖外部命令。
///
/// unix：
/// 1. `waitpid(WNOHANG)` 先试收本进程 child 的尸——返回 pid 表示 child
///    已退出（收尸完成）→ 死；返回 0 表示 child 仍在运行 → 活；
///    -1（ECHILD 等，非本进程 child）→ 落到下一步。
///    这一步替代旧的 `ps stat` 排僵尸（kill(pid,0) 对僵尸仍成功）。
/// 2. `kill(pid, 0)`：送达 → 活；`EPERM`（存在但属其他用户）→ 活；
///    `ESRCH` 及其余 errno → 死。
///
/// windows：`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` 打不开时
/// `GetLastError()==ERROR_ACCESS_DENIED`（存在但无权限）→ 活，其余 → 死；
/// 打开后 `GetExitCodeProcess`：`STILL_ACTIVE` → 活，拿到退出码 → 死，
/// 查询失败保守判活（宁 down 不收敛，勿 up 重复拉起）。
///
/// 其他平台保守返回 false。
pub fn is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        if pid > i32::MAX as u32 {
            return false;
        }
        unsafe {
            match libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) {
                r if r == pid as i32 => return false,
                0 => return true,
                _ => {}
            }
            if libc::kill(pid as i32, 0) == 0 {
                return true;
            }
            std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE,
        };
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h == 0 {
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(h, &mut code);
            CloseHandle(h);
            if ok == 0 {
                return true;
            }
            code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// pidfile 创建时间 ≈ 组件启动时间（serve status 报 uptime 用）。
pub fn pidfile_started(component: &str) -> Option<SystemTime> {
    let md = fs::metadata(pidfile(component)).ok()?;
    md.created().ok().or_else(|| md.modified().ok())
}

/// `PATH` 查找可执行文件（momod 定位；`--app-bin` 优先于此）。
/// windows 可执行文件带 `.exe` 后缀：`bin` 未以 `.exe` 结尾时追加 `bin.exe` 探测。
pub fn find_in_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| candidates(dir, bin))
        .find(|p| p.is_file())
}

/// 各平台候选文件名：unix 仅 `bin`；windows 追加 `.exe`（已带 .exe 不重复探测）。
#[cfg(windows)]
fn candidates(dir: PathBuf, bin: &str) -> Vec<PathBuf> {
    let mut v = vec![dir.join(bin)];
    if !bin.to_ascii_lowercase().ends_with(".exe") {
        v.push(dir.join(format!("{bin}.exe")));
    }
    v
}

/// unix 无扩展名探测：可执行判定靠文件存在（权限位由 spawn 阶段兜底）。
#[cfg(not(windows))]
fn candidates(dir: PathBuf, bin: &str) -> Vec<PathBuf> {
    vec![dir.join(bin)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 自身 pid 必活；`is_alive` 不能把它误判为死。
    /// （win: OpenProcess 自查；unix: kill(pid,0) 自查。）
    #[test]
    fn is_alive_reports_self() {
        assert!(is_alive(std::process::id()));
    }

    /// 不可能存活的 pid 必判死（4_000_000 超出常见 pid 空间）。
    #[test]
    fn is_alive_reports_dead_pid() {
        assert!(!is_alive(4_000_000));
        // pid 0 在 kill(2)/OpenProcess 都是特殊语义，显式判死。
        assert!(!is_alive(0));
    }

    /// `find_in_path` 能找到 PATH 上真实存在的工具（cargo 是构建依赖，必在）。
    /// windows 分支顺带验证 `.exe` 追加探测路径可达。
    #[test]
    fn find_in_path_locates_cargo() {
        assert!(find_in_path("cargo").is_some());
        assert!(find_in_path("definitely-not-a-real-bin-momo").is_none());
    }
}
