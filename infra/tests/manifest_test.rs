//! MANIFEST.toml 解析/校验单元测试（口径：momo-models.md `name/repo_id/revision/files/sha256/dest/notes`）。

use momo_infra::manifest::{Manifest, SourceKind};

const SAMPLE: &str = r#"
[[model]]
name = "kokoro-82m"
repo_id = "hexgrad/Kokoro-82M"
revision = "main"
notes = "兜底 TTS，实测 209ms/0.76GB"
license = { spdx = "apache-2.0", source = "https://huggingface.co/hexgrad/Kokoro-82M" }

[[model.files]]
path = "kokoro-v1_0.pth"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
size = 330000000

[[model.files]]
path = "voices.bin"

[[model]]
name = "sensevoice-small"
repo_id = "iic/SenseVoiceSmall"
dest = "sherpa-onnx/sense-voice-zh-en"
license = { spdx = "other", name = "FunASR Model License v1.1", source = "https://github.com/modelscope/FunASR/blob/main/MODEL_LICENSE" }

[[model.files]]
path = "model.int8.onnx"
"#;

#[test]
fn parses_entries() {
    let m = Manifest::parse(SAMPLE).unwrap();
    assert_eq!(m.models.len(), 2);

    let k = &m.models[0];
    assert_eq!(k.name, "kokoro-82m");
    assert_eq!(k.repo_id, "hexgrad/Kokoro-82M");
    assert_eq!(k.revision, "main");
    // dest 缺省 = repo_id（镜像 <owner>/<repo> 布局）
    assert_eq!(k.dest().to_str().unwrap(), "hexgrad/Kokoro-82M");
    assert_eq!(k.files.len(), 2);
    assert_eq!(k.files[0].size, Some(330000000));

    let s = &m.models[1];
    assert_eq!(s.revision, "main"); // 默认 revision
    assert_eq!(s.dest().to_str().unwrap(), "sherpa-onnx/sense-voice-zh-en");
}

#[test]
fn parses_license_metadata() {
    let m = Manifest::parse(SAMPLE).unwrap();

    let k = m.models[0].license.as_ref().unwrap();
    assert_eq!(k.spdx, "apache-2.0");
    assert_eq!(k.source, "https://huggingface.co/hexgrad/Kokoro-82M");
    assert!(k.name.is_none());

    // 非 SPDX 自定义许可：spdx = "other" 且带 name。
    let s = m.models[1].license.as_ref().unwrap();
    assert_eq!(s.spdx, "other");
    assert_eq!(s.name.as_deref(), Some("FunASR Model License v1.1"));
}

#[test]
fn accepts_license_absent() {
    // license 可选：缺省时解析为 None，不报错。
    let text = r#"
[[model]]
name = "x"
repo_id = "a/b"
"#;
    let m = Manifest::parse(text).unwrap();
    assert!(m.models[0].license.is_none());
}

#[test]
fn rejects_license_other_without_name() {
    let bad = r#"
[[model]]
name = "x"
repo_id = "a/b"
license = { spdx = "other", source = "https://example.com/license" }
"#;
    assert!(Manifest::parse(bad).is_err());
}

#[test]
fn rejects_license_empty_spdx() {
    let bad = r#"
[[model]]
name = "x"
repo_id = "a/b"
license = { spdx = "", source = "https://example.com/license" }
"#;
    assert!(Manifest::parse(bad).is_err());
}

#[test]
fn rejects_bad_sha256() {
    let bad = r#"
[[model]]
name = "x"
repo_id = "a/b"
[[model.files]]
path = "f.bin"
sha256 = "not-hex"
"#;
    assert!(Manifest::parse(bad).is_err());
}

#[test]
fn rejects_empty_repo_id() {
    let bad = r#"
[[model]]
name = "x"
repo_id = ""
"#;
    assert!(Manifest::parse(bad).is_err());
}

#[test]
fn rejects_dest_traversal() {
    let bad = r#"
[[model]]
name = "x"
repo_id = "a/b"
dest = "../escape"
"#;
    assert!(Manifest::parse(bad).is_err());
}

#[test]
fn tolerates_empty_manifest() {
    let m = Manifest::parse("").unwrap();
    assert!(m.models.is_empty());
}

#[test]
fn defaults_to_hf_source() {
    // 无 source 字段的条目保持 HF resolve 行为（向后兼容）。
    let m = Manifest::parse(SAMPLE).unwrap();
    assert_eq!(m.models[0].source, SourceKind::Hf);
    assert!(m.models[0].files[0].url.is_none());
}

#[test]
fn parses_url_source_with_file_urls() {
    let m = Manifest::parse(
        r#"
[[model]]
name = "silero-vad"
repo_id = "snakers4/silero-vad"
revision = "master"
source = "url"
files = [
  { path = "silero_vad.onnx", url = "https://raw.githubusercontent.com/snakers4/silero-vad/master/src/silero_vad/data/silero_vad.onnx" },
]
"#,
    )
    .unwrap();
    let e = &m.models[0];
    assert_eq!(e.source, SourceKind::Url);
    assert!(e.files[0].url.as_deref().unwrap().starts_with("https://"));
}

#[test]
fn rejects_url_source_without_file_url() {
    let bad = r#"
[[model]]
name = "x"
repo_id = "a/b"
source = "url"
files = [ { path = "f.bin" } ]
"#;
    assert!(Manifest::parse(bad).is_err());
}

#[test]
fn rejects_non_http_file_url() {
    let bad = r#"
[[model]]
name = "x"
repo_id = "a/b"
files = [ { path = "f.bin", url = "file:///etc/passwd" } ]
"#;
    assert!(Manifest::parse(bad).is_err());
}

#[test]
fn rejects_unknown_source() {
    let bad = r#"
[[model]]
name = "x"
repo_id = "a/b"
source = "ftp"
"#;
    assert!(Manifest::parse(bad).is_err());
}

#[test]
fn hf_entry_allows_per_file_url_override() {
    // source = "hf"（缺省）下 file.url 合法，pull 时对该文件走直链。
    let m = Manifest::parse(
        r#"
[[model]]
name = "x"
repo_id = "a/b"
files = [
  { path = "f.bin", url = "https://example.com/f.bin" },
  { path = "g.bin" },
]
"#,
    )
    .unwrap();
    assert_eq!(m.models[0].source, SourceKind::Hf);
    assert_eq!(
        m.models[0].files[0].url.as_deref(),
        Some("https://example.com/f.bin")
    );
    assert!(m.models[0].files[1].url.is_none());
}
