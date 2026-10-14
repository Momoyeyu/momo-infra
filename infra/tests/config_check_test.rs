//! `momoctl config check` 集成测试：真实仓库必须零漂移；fixture 漂移 → 退出码 3。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn momoctl(args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_momoctl"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn momoctl")
}

/// 与真实 monorepo 一致的最小 fixture：schema + 三侧 .env.example + config.md。
fn write_fixture(root: &Path) {
    fs::create_dir_all(root.join("app")).unwrap();
    fs::create_dir_all(root.join("models")).unwrap();
    fs::create_dir_all(root.join("infra")).unwrap();
    fs::create_dir_all(root.join("docs/01-architecture")).unwrap();

    fs::write(
        root.join("config.schema.toml"),
        r#"schema = 1
[[key]]
name = "MOMO_FOO"
type = "string"
default = "x"
sensitive = false
readers = ["app"]
files = ["app"]
component = "app"
description = "d"
[[key]]
name = "MOMO_SECRET"
type = "string"
default = ""
sensitive = true
readers = ["infra"]
files = ["infra"]
component = "infra"
description = "s"
[[passthrough]]
name = "HF_ENDPOINT"
type = "url"
default = "https://hf-mirror.com"
files = ["app"]
description = "m"
"#,
    )
    .unwrap();
    fs::write(
        root.join("app/.env.example"),
        "MOMO_FOO=x\nHF_ENDPOINT=https://hf-mirror.com\n",
    )
    .unwrap();
    fs::write(root.join("models/.env.example"), "").unwrap();
    fs::write(root.join("infra/.env.example"), "MOMO_SECRET=\n").unwrap();
    fs::write(
        root.join("docs/01-architecture/config.md"),
        r#"# 配置规范

## 2. 变量全集

### 2.2 编排器（app 读）

| 名称 | 含义 | 默认值 | 取值 | 敏感 |
|---|---|---|---|---|
| `MOMO_FOO` | d | `x` | str | 否 |

### 2.4 infra 读

| 名称 | 含义 | 默认值 | 取值 | 敏感 |
|---|---|---|---|---|
| `MOMO_SECRET` | s | 空 | str | **是** |

### 2.5 上游工具直通变量

| 名称 | 含义 | 默认值 | 说明 |
|---|---|---|---|
| `HF_ENDPOINT` | HF 镜像 | `https://hf-mirror.com` | m |

## 3. `.env.example`

```dotenv
MOMO_FOO=x
HF_ENDPOINT=https://hf-mirror.com
```
"#,
    )
    .unwrap();
}

fn fixture_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("momoctl-config-check-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn repo_config_check_passes() {
    // CWD = infra/ crate 根；find_root 向上定位 monorepo 根的 config.schema.toml。
    let out = momoctl(&["config", "check"], Path::new(env!("CARGO_MANIFEST_DIR")));
    assert_eq!(
        out.status.code(),
        Some(0),
        "repo drift check failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn config_check_clean_fixture_exit_0() {
    let dir = fixture_dir("clean");
    write_fixture(&dir);
    let out = momoctl(&["config", "check", "--root", dir.to_str().unwrap()], &dir);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn config_check_detects_drift_exit_3() {
    let dir = fixture_dir("drift");
    write_fixture(&dir);
    // extra 键 → drift
    let env = dir.join("app/.env.example");
    let mut text = fs::read_to_string(&env).unwrap();
    text.push_str("MOMO_NOT_IN_SCHEMA=1\n");
    fs::write(&env, text).unwrap();

    let out = momoctl(&["config", "check", "--root", dir.to_str().unwrap()], &dir);
    assert_eq!(out.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("MOMO_NOT_IN_SCHEMA"), "{stdout}");

    // 修掉 extra、删掉必需键 → 仍然 3（missing）
    fs::write(&env, "HF_ENDPOINT=https://hf-mirror.com\n").unwrap();
    let out = momoctl(&["config", "check", "--root", dir.to_str().unwrap()], &dir);
    assert_eq!(out.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("MOMO_FOO"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn config_check_json_shape() {
    let dir = fixture_dir("json");
    write_fixture(&dir);
    let out = momoctl(
        &["config", "check", "--json", "--root", dir.to_str().unwrap()],
        &dir,
    );
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("config check --json must emit valid JSON");
    assert_eq!(v["schema"], 1);
    assert!(v["sources"].is_array());
    assert_eq!(v["summary"]["drift"], 0);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn config_check_missing_root_exit_2() {
    let dir = fixture_dir("noroot");
    // 空目录：找不到 config.schema.toml → usage 错误退出码 2
    let out = momoctl(&["config", "check"], &dir);
    assert_eq!(out.status.code(), Some(2));
    fs::remove_dir_all(&dir).unwrap();
}
