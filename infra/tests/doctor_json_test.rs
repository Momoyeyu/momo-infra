//! `momoctl doctor --json` schema 稳定回归：
//! `{schema:1, profile, groups:[{name, checks:[{id,status,detail,hint}]}], summary}`。
//! 干净机器上不 panic、退出码 ∈ {0,3}（有 fail→3，全 ok→0）。

use std::process::Command;

#[test]
fn doctor_json_schema_is_stable() {
    // 隔离用户 .env：显式 --env-file 指向空文件
    let env_file = std::env::temp_dir().join("momoctl-doctor-test.env");
    std::fs::write(&env_file, "").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_momoctl"))
        .args(["doctor", "--json", "--env-file"])
        .arg(&env_file)
        .output()
        .expect("spawn momoctl");

    // 不 panic（101），退出码只可能是 0 或 3
    let code = out.status.code().unwrap_or(-1);
    assert!(
        code == 0 || code == 3,
        "unexpected exit code {code}; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("doctor --json must emit valid JSON");

    assert_eq!(v["schema"], 1);
    assert!(v["profile"].is_string());
    assert!(v["summary"]["ok"].is_u64());
    assert!(v["summary"]["warn"].is_u64());
    assert!(v["summary"]["fail"].is_u64());

    let groups = v["groups"].as_array().expect("groups must be array");
    assert!(!groups.is_empty(), "doctor must emit at least one group");

    let group_names: Vec<&str> = groups
        .iter()
        .map(|g| g["name"].as_str().expect("group.name string"))
        .collect();
    for required in [
        "system",
        "toolchain",
        "python",
        "network",
        "env",
        "models",
        "endpoints",
    ] {
        assert!(
            group_names.contains(&required),
            "missing group '{required}'; got {group_names:?}"
        );
    }

    for g in groups {
        let checks = g["checks"].as_array().expect("group.checks array");
        for c in checks {
            assert!(c["id"].is_string(), "check.id must be string: {c}");
            assert!(c["detail"].is_string(), "check.detail must be string: {c}");
            assert!(
                c.get("hint").is_some(),
                "check.hint must be present (null allowed): {c}"
            );
            let status = c["status"].as_str().expect("check.status string");
            assert!(
                ["ok", "warn", "fail"].contains(&status),
                "bad status '{status}'"
            );
        }
    }
}
