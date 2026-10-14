//! `momoctl doctor [--profile X] [--json]`：环境检查矩阵。
//! 组：system / toolchain / python / network / env / models / endpoints。
//! 退出码：全 ok → 0；有 fail → 3。检查项口径见 momo-infra.md「子命令清单」。

use std::path::PathBuf;
use std::process::Command;
use std::thread;

use crate::api::dto::{Check, CheckGroup, DoctorReport};
use crate::cli::{DoctorArgs, Profile};
use crate::env_cfg::{self, EnvCfg};
use crate::manifest::Manifest;
use crate::net;
use crate::output::{AppError, ExitCode};

pub fn run(args: &DoctorArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    let report = collect_report(args.profile);

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        let mut out = std::io::stdout().lock();
        crate::output::render_doctor_human(&report, &mut out)?;
    }

    Ok(if report.has_fail() {
        ExitCode::CheckFailed as i32
    } else {
        ExitCode::Ok as i32
    })
}

/// doctor 检查矩阵 core：CLI `doctor` 与 `GET /api/doctor` 共用同一 `DoctorReport`。
/// 做真实探测（进程/网络/文件 IO），web 侧须 `spawn_blocking` 调用。
pub fn collect_report(profile_arg: Option<Profile>) -> DoctorReport {
    let env = EnvCfg;
    let profile = env.profile(profile_arg);
    let groups = vec![
        check_system(),
        check_toolchain(),
        check_python_deps(),
        check_network(&env),
        check_env(&env, profile),
        check_models(&env),
        check_endpoints(&env),
    ];
    DoctorReport::new(profile.as_str(), groups)
}

// ---------- system ----------

fn check_system() -> CheckGroup {
    let mut checks = vec![];
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let detail = match os_version() {
        Some(v) => format!("{os} {v} ({arch})"),
        None => format!("{os} ({arch})"),
    };
    checks.push(Check::ok("os", detail));

    match os {
        "macos" => {
            // Metal 框架存在即视为 GPU 加速可用（mlx/llama.cpp 走 Metal）。
            if PathBuf::from("/System/Library/Frameworks/Metal.framework").exists() {
                checks.push(Check::ok("gpu.metal", "Metal framework present"));
            } else {
                checks.push(Check::warn(
                    "gpu.metal",
                    "Metal framework not found",
                    "MLX/llama.cpp will fall back to CPU; check macOS version and Xcode CLT",
                ));
            }
        }
        "linux" => match tool_version("nvidia-smi", &["--version"]) {
            Some(v) => checks.push(Check::ok("gpu.cuda", v)),
            None => checks.push(Check::warn(
                "gpu.cuda",
                "nvidia-smi not found",
                "TTS CUDA profile needs NVIDIA drivers; ignore on pure-CPU deploys",
            )),
        },
        _ => checks.push(Check::ok("gpu", "no gpu probe for this OS")),
    }
    CheckGroup::new("system", checks)
}

/// 系统版本：macOS `sw_vers -productVersion`；其余 unix `uname -r`；
/// Windows `cmd /C ver`（无 `uname`）。取不到返回 None，doctor 只显示 os/arch。
#[cfg(target_os = "macos")]
fn os_version() -> Option<String> {
    tool_version("sw_vers", &["-productVersion"])
}

/// `uname -r` 是 unix-only 命令，必须 cfg(unix) 限定（Windows 无此命令）。
#[cfg(all(unix, not(target_os = "macos")))]
fn os_version() -> Option<String> {
    tool_version("uname", &["-r"])
}

#[cfg(windows)]
fn os_version() -> Option<String> {
    tool_version("cmd", &["/C", "ver"]).and_then(|v| parse_windows_ver(&v))
}

/// 既非 unix 也非 windows 的目标：不探版本。
#[cfg(not(any(unix, windows)))]
fn os_version() -> Option<String> {
    None
}

/// 从 `cmd /C ver` 输出提取版本串（纯函数，便于单测）：
/// `Microsoft Windows [Version 10.0.22631.3155]` → `10.0.22631.3155`；
/// 无 `[Version ...]` 标记时退回首个非空行；空输出返回 None。
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_windows_ver(stdout: &str) -> Option<String> {
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty())?;
    if let Some((_, rest)) = line.split_once("[Version ") {
        let v = rest.split(']').next().unwrap_or("").trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    Some(line.to_string())
}

// ---------- toolchain ----------

fn check_toolchain() -> CheckGroup {
    // (bin, required)。必需项缺失 → fail；可选项缺失 → warn。
    const TOOLS: &[(&str, bool)] = &[
        ("rustc", true),
        ("cargo", true),
        ("python3", true),
        ("uv", true),
        ("node", false),
        ("pnpm", false),
        ("ffmpeg", false),
    ];
    let checks = TOOLS
        .iter()
        .map(|(bin, required)| match tool_version(bin, &["--version"]) {
            Some(v) => Check::ok(format!("tool.{bin}"), v),
            None if *required => Check::fail(
                format!("tool.{bin}"),
                format!("{bin} not found in PATH"),
                "install and retry; rust via rustup, uv via astral.sh",
            ),
            None => Check::warn(
                format!("tool.{bin}"),
                format!("{bin} not found in PATH"),
                "needed for frontend build / audio transcoding",
            ),
        })
        .collect();
    CheckGroup::new("toolchain", checks)
}

/// 运行 `<bin> <args>` 取首行版本输出；二进制缺失或失败返回 None。
fn tool_version(bin: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let line = stdout
        .lines()
        .chain(stderr.lines())
        .next()
        .unwrap_or("")
        .trim();
    if line.is_empty() {
        Some(format!("{bin} (version output empty)"))
    } else {
        Some(line.to_string())
    }
}

// ---------- python deps ----------

fn check_python_deps() -> CheckGroup {
    let mut checks = vec![];
    // torch/torchaudio 存在性与 CUDA build；laptop 的 TTS 依赖它。
    let probe = Command::new("python3")
        .args([
            "-c",
            "import torch, torchaudio; \
             print(torch.__version__, torchaudio.__version__, \
             torch.cuda.is_available())",
        ])
        .output();
    match probe {
        Ok(out) if out.status.success() => {
            let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let parts: Vec<&str> = line.split_whitespace().collect();
            let cuda = parts.get(2).is_some_and(|v| *v == "True");
            checks.push(Check::ok("py.torch", &line));
            if cuda {
                checks.push(Check::ok("py.cuda", "torch.cuda.is_available()=True"));
            } else if cfg!(target_os = "linux") {
                checks.push(Check::warn(
                    "py.cuda",
                    "torch.cuda.is_available()=False",
                    "TTS CUDA profile needs a CUDA-enabled torch build; ignore on cpu/mps",
                ));
            } else {
                checks.push(Check::ok("py.cuda", "cuda=False (non-linux; mps/cpu path)"));
            }
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .unwrap_or("import failed")
                .trim()
                .to_string();
            checks.push(Check::fail(
                "py.torch",
                format!("torch/torchaudio import failed: {err}"),
                "run `uv sync` in the momo-models repo to create the venv; doctor probes system python3",
            ));
        }
        Err(_) => checks.push(Check::fail(
            "py.torch",
            "python3 not found",
            "install python3 and uv",
        )),
    }
    CheckGroup::new("python", checks)
}

// ---------- network ----------

fn check_network(env: &EnvCfg) -> CheckGroup {
    // 远端三个 URL 并行探，各 3s 超时。
    let targets: [(&str, &str); 3] = [
        ("net.hf-mirror", "https://hf-mirror.com"),
        ("net.huggingface", "https://huggingface.co"),
        ("net.deepseek", "https://api.deepseek.com"),
    ];
    let results = thread::scope(|s| {
        targets
            .iter()
            .map(|(id, url)| s.spawn(move || (*id, *url, net::probe_head(url))))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });

    let mut checks = vec![];
    for (id, url, p) in &results {
        let check = match *id {
            // hf-mirror 是 laptop 主源（05-ops/model-fetch.md），不可达为 fail。
            "net.hf-mirror" if p.reachable => Check::ok(*id, p.describe()),
            "net.hf-mirror" => Check::fail(
                *id,
                format!("{url} unreachable: {}", p.describe()),
                "laptop primary model source must be reachable; check proxy/DNS, ModelScope as fallback",
            ),
            // huggingface.co 直连不可达是已知事实（laptop），warn + hint HF_ENDPOINT。
            "net.huggingface" if p.reachable => Check::ok(*id, p.describe()),
            "net.huggingface" => Check::warn(
                *id,
                format!("{url} unreachable: {}", p.describe()),
                "export HF_ENDPOINT=https://hf-mirror.com",
            ),
            // DeepSeek 仅降级档/P4 视觉需要，不可达 warn。
            _ if p.reachable => Check::ok(*id, p.describe()),
            _ => Check::warn(
                *id,
                format!("{url} unreachable: {}", p.describe()),
                "only needed by the degraded DeepSeek brain / P4 vision",
            ),
        };
        checks.push(check);
    }

    // PyPI 镜像：只查 env 是否配置（安装期生效，不探活）。
    if env.get("UV_INDEX_URL").is_some() || env.get("PIP_INDEX_URL").is_some() {
        checks.push(Check::ok("net.pypi-mirror", "index mirror configured"));
    } else {
        checks.push(Check::warn(
            "net.pypi-mirror",
            "UV_INDEX_URL / PIP_INDEX_URL unset",
            "configure a PyPI mirror on laptop (05-ops/model-fetch.md)",
        ));
    }
    CheckGroup::new("network", checks)
}

// ---------- env ----------

fn check_env(env: &EnvCfg, profile: Profile) -> CheckGroup {
    let mut checks = vec![];

    checks.push(Check::ok("env.MOMO_PROFILE", profile.as_str()));

    // 必需键：缺失 fail；敏感键打码。
    for key in env_cfg::REQUIRED_KEYS {
        checks.push(match env.get(key) {
            Some(v) => Check::ok(format!("env.{key}"), v),
            None => Check::fail(
                format!("env.{key}"),
                "unset",
                format!("set {key} in .env (template: .env.example)"),
            ),
        });
    }

    // 有默认值的键：报告生效值或 default。
    for key in env_cfg::DEFAULTED_KEYS {
        checks.push(match env.get(key) {
            Some(v) => Check::ok(format!("env.{key}"), v),
            None => Check::ok(format!("env.{key}"), "unset → built-in default"),
        });
    }

    // 敏感键：只报 set/unset（config.md §4 不回显）。
    for key in env_cfg::SENSITIVE_KEYS {
        let detail = if env.get(key).is_some() {
            "set"
        } else {
            "unset"
        };
        checks.push(Check::ok(format!("env.{key}"), detail));
    }

    // 条件必需：LLM 端点指 DeepSeek 时必须有 key。
    if let Some(ep) = env.llm_endpoint() {
        if ep.contains("deepseek.com") && env.get("MOMO_LLM_API_KEY").is_none() {
            checks.push(Check::fail(
                "env.MOMO_LLM_API_KEY",
                "unset but MOMO_LLM_ENDPOINT is api.deepseek.com",
                "MOMO_LLM_API_KEY is required for the degraded DeepSeek profile",
            ));
        }
    }
    CheckGroup::new("env", checks)
}

// ---------- models ----------

/// MANIFEST.toml 定位：当前目录 ./models/，其次兄弟仓 ../momo-models/models/。
fn find_manifest() -> Option<PathBuf> {
    [
        "models/MANIFEST.toml",
        "../momo-models/models/MANIFEST.toml",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

fn check_models(env: &EnvCfg) -> CheckGroup {
    let mut checks = vec![];
    let models_dir = env.models_dir();
    if models_dir.is_dir() {
        checks.push(Check::ok(
            "models.dir",
            format!("{} exists", models_dir.display()),
        ));
    } else {
        checks.push(Check::warn(
            "models.dir",
            format!("{} missing", models_dir.display()),
            "created by the first `momoctl pull`",
        ));
    }

    let manifest_path = match find_manifest() {
        Some(p) => p,
        None => {
            checks.push(Check::fail(
                "models.manifest",
                "MANIFEST.toml not found",
                "expected ../momo-models/models/MANIFEST.toml (clone the momo-models repo)",
            ));
            return CheckGroup::new("models", checks);
        }
    };
    checks.push(Check::ok(
        "models.manifest",
        format!("{}", manifest_path.display()),
    ));

    let manifest = match Manifest::load(&manifest_path) {
        Ok(m) => m,
        Err(e) => {
            checks.push(Check::fail(
                "models.manifest",
                format!("parse failed: {e}"),
                "entry schema in momo-models.md: name/repo_id/revision/files/sha256/dest",
            ));
            return CheckGroup::new("models", checks);
        }
    };

    if manifest.models.is_empty() {
        checks.push(Check::ok("models.entries", "0 entries (empty manifest)"));
    }
    for m in &manifest.models {
        let dir = m.dest_dir(&models_dir);
        if !dir.is_dir() {
            checks.push(Check::fail(
                format!("model.{}", m.name),
                format!("{} missing", dir.display()),
                format!("momoctl pull --only {}", m.name),
            ));
            continue;
        }
        let missing: Vec<&str> = m
            .files
            .iter()
            .filter(|f| !dir.join(&f.path).exists())
            .map(|f| f.path.as_str())
            .collect();
        if !missing.is_empty() {
            checks.push(Check::fail(
                format!("model.{}", m.name),
                format!("{}/{} files missing", missing.len(), m.files.len()),
                format!("momoctl pull --only {}", m.name),
            ));
            continue;
        }
        // sha256 抽查：第一条带 hash 的文件。
        if let Some(f) = m.files.iter().find(|f| f.sha256.is_some()) {
            let path = dir.join(&f.path);
            match crate::manifest::sha256_file(&path) {
                Ok(h) if h.eq_ignore_ascii_case(f.sha256.as_ref().unwrap()) => {
                    checks.push(Check::ok(
                        format!("model.{}", m.name),
                        format!("{} files ok, sha256 spot-check passed", m.files.len()),
                    ));
                }
                Ok(_) => checks.push(Check::fail(
                    format!("model.{}", m.name),
                    format!("sha256 mismatch on {}", f.path),
                    format!("delete it and re-run `momoctl pull --only {}`", m.name),
                )),
                Err(e) => checks.push(Check::warn(
                    format!("model.{}", m.name),
                    format!("sha256 spot-check failed: {e}"),
                    "check file readability",
                )),
            }
        } else {
            checks.push(Check::ok(
                format!("model.{}", m.name),
                format!("{} files present (no sha256 to spot-check)", m.files.len()),
            ));
        }
    }
    CheckGroup::new("models", checks)
}

// ---------- endpoints ----------

/// `0.0.0.0`/`::` 监听地址探活时换成 loopback。
fn probeable(listen: &str) -> String {
    listen
        .replacen("0.0.0.0", "127.0.0.1", 1)
        .replacen("[::]", "127.0.0.1", 1)
}

fn check_endpoints(env: &EnvCfg) -> CheckGroup {
    let mut checks = vec![];

    let local: [(&str, String, &str); 3] = [
        ("ep.momod", probeable(&env.orch_listen()), "/healthz"),
        ("ep.svc-asr", probeable(&env.asr_listen()), "/healthz"),
        ("ep.svc-tts", probeable(&env.tts_listen()), "/healthz"),
    ];
    for (id, addr, path) in &local {
        let url = format!("http://{addr}{path}");
        let p = net::probe_get(&url);
        if p.ready() {
            checks.push(Check::ok(*id, format!("{url} → {}", p.describe())));
        } else {
            checks.push(Check::fail(
                *id,
                format!("{url} → {}", p.describe()),
                "not running: `momoctl serve up`; should return 200 once started",
            ));
        }
    }

    // 大脑：MOMO_LLM_ENDPOINT（含 /v1）+ /models，OpenAI 端点直连探活。
    match env.llm_endpoint() {
        Some(ep) => {
            let url = format!("{}/models", ep.trim_end_matches('/'));
            let p = net::probe_get(&url);
            if p.ready() {
                checks.push(Check::ok("ep.brain", format!("{url} → {}", p.describe())));
            } else {
                checks.push(Check::fail(
                    "ep.brain",
                    format!("{url} → {}", p.describe()),
                    "normal profile uses mac-studio MLX; if unavailable run `momoctl switch --to degraded`",
                ));
            }
        }
        None => checks.push(Check::fail(
            "ep.brain",
            "MOMO_LLM_ENDPOINT unset",
            "set MOMO_LLM_ENDPOINT in .env",
        )),
    }
    CheckGroup::new("endpoints", checks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cmd /C ver` 典型输出 → 只看 `[Version ...]` 里的版本号。
    #[test]
    fn parse_windows_ver_extracts_bracketed_version() {
        let out = "\r\nMicrosoft Windows [Version 10.0.22631.3155]\r\n";
        assert_eq!(parse_windows_ver(out).as_deref(), Some("10.0.22631.3155"));
    }

    /// 多行输出取首个非空行，前导空行不影响。
    #[test]
    fn parse_windows_ver_skips_leading_blank_lines() {
        let out = "\n\nMicrosoft Windows [Version 6.3.9600]\n";
        assert_eq!(parse_windows_ver(out).as_deref(), Some("6.3.9600"));
    }

    /// 无 `[Version ]` 标记时退回整行原文。
    #[test]
    fn parse_windows_ver_falls_back_to_raw_line() {
        assert_eq!(
            parse_windows_ver("Windows unknown build\n").as_deref(),
            Some("Windows unknown build")
        );
    }

    /// 空输出（命令无 stdout）返回 None。
    #[test]
    fn parse_windows_ver_empty_output_is_none() {
        assert_eq!(parse_windows_ver("   \n\n").as_deref(), None);
    }

    /// unix 实机探测：macOS `sw_vers -productVersion` / Linux `uname -r` 应有输出。
    #[cfg(unix)]
    #[test]
    fn os_version_probe_returns_something_on_unix() {
        let v = os_version().expect("os_version probe must succeed on unix");
        assert!(!v.trim().is_empty(), "empty version output");
    }
}
