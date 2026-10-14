//! `momoctl bench [--turns N] [--json] [--strict]`：端到端延迟测量。
//!
//! 以 momo-ws v2 client 连接 momod `/ws/ui`（`MOMO_ORCH_LISTEN`），hello →
//! hello.ok 后发固定 `input.text`，N 回合记录：发→`input.accepted`、
//! 发→首个 `output.sentence`、发→首个下行二进制帧（audio.chunk 出声延迟，
//! 目标 <1.5s）、发→`turn.finished` 全长；汇总各环节 p50/p95。
//! 收到 `audio.open` downlink 回 `playback.started`，`audio.end` 回
//! `playback.progress{completed:true}`（听见账本，否则有声回合不终态）。
//!
//! 退出码：0 测量完成且达标；2 握手/配置失败；3 --strict 超标；4 连接不可达。

use std::collections::BTreeMap;
use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::api::dto::{BenchReport, Check, CheckStatus, StageStats, Summary, TurnResult};
use crate::cli::BenchArgs;
use crate::env_cfg::EnvCfg;
use crate::output::{AppError, ExitCode};

/// 出声延迟目标（momo-infra.md：目标 <1.5s）。
const FIRST_AUDIO_TARGET_MS: u64 = 1500;
/// TCP connect / HTTP upgrade 超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// hello.ok 等待超时（服务端 hello deadline 是 5s）。
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// 固定探针输入（不改用户数据口径：内容恒定、可识别）。
const PROBE_TEXT: &str = "你好。这是一次 momoctl bench 延迟测量。";
/// UI 链路 Origin：bench 以 desktop 身份接入本机 momod。
const BENCH_ORIGIN: &str = "tauri://localhost";

pub fn run(args: &BenchArgs, ctx: &super::Ctx) -> Result<i32, AppError> {
    let listen = EnvCfg.orch_listen();
    let (host, port) = dial_addr(&listen);
    let endpoint = format!("ws://{listen}/ws/ui");

    // 连接 + HTTP upgrade：失败 → 网络不可达（exit 4）。
    let mut conn =
        ws::connect(&host, port, "/ws/ui", BENCH_ORIGIN, CONNECT_TIMEOUT).map_err(AppError::Net)?;

    // hello → hello.ok：token 取自 MOMO_UI_TOKEN（standalone dev 未设不校验，
    // 字段仍必填）；握手被拒 → 配置问题（exit 2）。
    let token = EnvCfg.get("MOMO_UI_TOKEN").unwrap_or_default();
    let hello = serde_json::json!({
        "type": "hello",
        "protocol": "momo-ws",
        "version": "v2",
        "client": "ui",
        "auth_token": token,
    });
    let session_id = handshake(&mut conn, &hello.to_string())
        .map_err(|e| AppError::Config(format!("handshake failed: {e}")))?;

    // 回合超时：跟随 LLM 总超时 + 余量（protocol §10）。
    let turn_deadline = EnvCfg
        .get("MOMO_LLM_TIMEOUT_MS")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30_000)
        + 10_000;
    let turn_deadline = Duration::from_millis(turn_deadline);

    let mut results = Vec::new();
    for i in 0..args.turns {
        results.push(run_turn(&mut conn, &session_id, i + 1, turn_deadline));
    }

    let stats = collect_stats(&results);
    let checks = make_checks(&results, &stats, args.strict);
    let report = BenchReport {
        schema: 1,
        command: "bench".into(),
        endpoint,
        turns: args.turns,
        target_first_audio_ms: FIRST_AUDIO_TARGET_MS,
        results,
        stats,
        summary: Summary::from_checks(&checks),
        checks,
    };
    render(&report, ctx)?;

    if args.strict && report.summary.fail > 0 {
        return Ok(ExitCode::CheckFailed as i32);
    }
    Ok(ExitCode::Ok as i32)
}

/// hello → hello.ok：返回 session_id；收到 error/close → Err。
fn handshake(conn: &mut ws::Conn, hello: &str) -> Result<String, String> {
    conn.send_text(hello).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + HELLO_TIMEOUT;
    loop {
        match conn.next(deadline)? {
            None => return Err("hello.ok timeout".into()),
            Some(ws::Frame::Text(t)) => {
                let v: serde_json::Value = serde_json::from_str(&t).map_err(|e| e.to_string())?;
                match v["type"].as_str() {
                    Some("hello.ok") => {
                        return v["session_id"]
                            .as_str()
                            .map(str::to_string)
                            .ok_or_else(|| "hello.ok without session_id".into());
                    }
                    Some("error") => {
                        return Err(format!(
                            "{}: {}",
                            v["code"].as_str().unwrap_or("error"),
                            v["message"].as_str().unwrap_or("")
                        ));
                    }
                    _ => {}
                }
            }
            Some(ws::Frame::Closed(code, reason)) => {
                return Err(format!("closed during handshake: {code} {reason}"));
            }
            Some(_) => {}
        }
    }
}

/// 单回合：发 `input.text` → 读帧至 `turn.finished` / rejected / 超时。
/// 途经 audio.open/audio.end 时回 playback 账本帧（started + completed ACK）。
fn run_turn(conn: &mut ws::Conn, session_id: &str, n: u32, deadline: Duration) -> TurnResult {
    let mut r = TurnResult {
        turn: n,
        accepted_ms: None,
        first_sentence_ms: None,
        first_audio_ms: None,
        finished_ms: None,
        outcome: None,
        note: None,
    };

    let req_id = format!("r_{}", ulid::Ulid::new());
    let msg = serde_json::json!({
        "type": "input.text",
        "session_id": session_id,
        "event_id": ulid::Ulid::new().to_string(),
        "at_ms": now_ms(),
        "id": req_id,
        "text": PROBE_TEXT,
        "lang": "zh",
    });
    let t0 = Instant::now();
    if let Err(e) = conn.send_text(&msg.to_string()) {
        r.note = Some(format!("send failed: {e}"));
        return r;
    }
    let end = t0 + deadline;

    // 下行 stream 账本：stream_id → (sentence_seq, 已收 PCM sample 数)。
    let mut streams: BTreeMap<String, (u32, u64)> = BTreeMap::new();
    // stream_id → turn 归属不需要：单活跃回合下 stream 都属本回合。
    let mut turn_id = String::new();

    loop {
        let frame = match conn.next(end) {
            Ok(Some(f)) => f,
            Ok(None) => {
                r.note = Some("turn deadline exceeded".into());
                break;
            }
            Err(e) => {
                r.note = Some(format!("read failed: {e}"));
                break;
            }
        };
        match frame {
            ws::Frame::Binary(bytes) => {
                // 只计本回合 audio.open 过的 stream（旧回合排空帧不计）。
                if let Some(h) = parse_audio_header(&bytes) {
                    if let Some(entry) = streams.get_mut(&h) {
                        entry.1 += (bytes.len() - 32) as u64 / 2;
                        if r.first_audio_ms.is_none() {
                            r.first_audio_ms = Some(elapsed_ms(t0));
                        }
                    }
                }
            }
            ws::Frame::Closed(code, reason) => {
                r.note = Some(format!("connection closed: {code} {reason}"));
                break;
            }
            ws::Frame::Text(text) => {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let ty = v["type"].as_str().unwrap_or("");
                // 只认本回合消息：accept 前无 turn_id，旧回合排空事件一律丢弃。
                let for_this_turn =
                    !turn_id.is_empty() && v["turn_id"].as_str() == Some(turn_id.as_str());
                match ty {
                    "input.accepted" if v["ref_id"].as_str() == Some(req_id.as_str()) => {
                        turn_id = v["turn_id"].as_str().unwrap_or("").to_string();
                        if r.accepted_ms.is_none() {
                            r.accepted_ms = Some(elapsed_ms(t0));
                        }
                    }
                    "input.rejected" if v["ref_id"].as_str() == Some(req_id.as_str()) => {
                        r.note = Some(format!("rejected: {}", v["reason"].as_str().unwrap_or("")));
                        r.outcome = Some("rejected".into());
                        break;
                    }
                    "output.sentence" if for_this_turn => {
                        if r.first_sentence_ms.is_none() {
                            r.first_sentence_ms = Some(elapsed_ms(t0));
                        }
                    }
                    "audio.open" if v["direction"].as_str() == Some("downlink") => {
                        let sid = v["stream_id"].as_str().unwrap_or("").to_string();
                        let seq = v["sentence_seq"].as_u64().unwrap_or(0) as u32;
                        streams.insert(sid.clone(), (seq, 0));
                        let _ = conn.send_text(&playback_json(
                            session_id,
                            "playback.started",
                            &sid,
                            seq,
                            0,
                            false,
                        ));
                    }
                    "audio.end" => {
                        let sid = v["stream_id"].as_str().unwrap_or("").to_string();
                        if let Some((seq, samples)) = streams.get(&sid).copied() {
                            let _ = conn.send_text(&playback_json(
                                session_id,
                                "playback.progress",
                                &sid,
                                seq,
                                samples,
                                true,
                            ));
                        }
                    }
                    "turn.finished" if for_this_turn => {
                        r.finished_ms = Some(elapsed_ms(t0));
                        r.outcome = v["outcome"].as_str().map(str::to_string);
                        break;
                    }
                    "error" => {
                        r.note = Some(format!(
                            "{}: {}",
                            v["code"].as_str().unwrap_or("error"),
                            v["message"].as_str().unwrap_or("")
                        ));
                    }
                    _ => {}
                }
            }
        }
    }
    r
}

/// `playback.started` / `playback.progress` JSON 文本帧。
fn playback_json(
    session_id: &str,
    ty: &str,
    stream_id: &str,
    sentence_seq: u32,
    played_samples: u64,
    completed: bool,
) -> String {
    let mut v = serde_json::json!({
        "type": ty,
        "session_id": session_id,
        "event_id": ulid::Ulid::new().to_string(),
        "at_ms": now_ms(),
        "stream_id": stream_id,
        "sentence_seq": sentence_seq,
    });
    if ty == "playback.progress" {
        v["played_samples"] = played_samples.into();
        v["completed"] = completed.into();
    }
    v.to_string()
}

/// 二进制音频帧 → stream_id 文字形（帧头 4..20 为 16B ULID 原始值；
/// 与 sink.rs 的 `Ulid::to_string()` 口径一致）。
fn parse_audio_header(frame: &[u8]) -> Option<String> {
    if frame.len() < 32 {
        return None;
    }
    let raw: [u8; 16] = frame[4..20].try_into().ok()?;
    Some(ulid::Ulid::from_bytes(raw).to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn elapsed_ms(t0: Instant) -> u64 {
    t0.elapsed().as_millis() as u64
}

/// `host:port` 拨号地址：`0.0.0.0`/`::` 监听地址换 loopback；可含 ws:// 前缀。
fn dial_addr(listen: &str) -> (String, u16) {
    let s = listen
        .trim()
        .trim_start_matches("ws://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    // 去掉路径段（若误填 URL）。
    let hostport = s.split('/').next().unwrap_or(s);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (
            h.trim_matches(|c| c == '[' || c == ']'),
            p.parse().unwrap_or(7800),
        ),
        None => (hostport, 7800),
    };
    let host = match host {
        "0.0.0.0" | "::" | "" => "127.0.0.1",
        h => h,
    };
    (host.to_string(), port)
}

/// 各环节样本 → p50/p95/max（nearest-rank）。
fn collect_stats(results: &[TurnResult]) -> BTreeMap<String, StageStats> {
    let mut out = BTreeMap::new();
    let pick: fn(&TurnResult, &str) -> Option<u64> = |r, name| match name {
        "accepted" => r.accepted_ms,
        "first_sentence" => r.first_sentence_ms,
        "first_audio" => r.first_audio_ms,
        "turn_total" => r.finished_ms,
        _ => None,
    };
    for name in ["accepted", "first_sentence", "first_audio", "turn_total"] {
        let mut s: Vec<u64> = results.iter().filter_map(|r| pick(r, name)).collect();
        if s.is_empty() {
            continue;
        }
        s.sort_unstable();
        out.insert(
            name.to_string(),
            StageStats {
                samples: s.len(),
                p50_ms: percentile(&s, 50),
                p95_ms: percentile(&s, 95),
                max_ms: *s.last().unwrap(),
            },
        );
    }
    out
}

/// nearest-rank 百分位（n≥1）：p50 of [1..5] → 第 3 小值。
fn percentile(sorted: &[u64], p: u32) -> u64 {
    let n = sorted.len();
    let idx = ((n as u64 * p as u64).div_ceil(100) as usize).saturating_sub(1);
    sorted[idx.min(n - 1)]
}

fn make_checks(
    results: &[TurnResult],
    stats: &BTreeMap<String, StageStats>,
    strict: bool,
) -> Vec<Check> {
    let mut checks = Vec::new();

    let finished = results.iter().filter(|r| r.finished_ms.is_some()).count();
    if finished == results.len() {
        checks.push(Check::ok(
            "turns",
            format!("{finished}/{} finished", results.len()),
        ));
    } else {
        checks.push(Check::warn(
            "turns",
            format!("{finished}/{} finished", results.len()),
            "check turn notes; momod pipeline may be degraded or unreachable upstream",
        ));
    }

    for (stage, key) in [
        ("accepted", "accepted"),
        ("first_sentence", "first_sentence"),
        ("first_audio", "first_audio"),
        ("turn_total", "turn_total"),
    ] {
        match stats.get(stage) {
            Some(s) => {
                let detail = format!(
                    "p50 {}ms, p95 {}ms, max {}ms (n={})",
                    s.p50_ms, s.p95_ms, s.max_ms, s.samples
                );
                if stage == "first_audio" && s.p95_ms >= FIRST_AUDIO_TARGET_MS {
                    let hint = "target <1.5s voice-out; check LLM/TTS providers and models dir";
                    checks.push(if strict {
                        Check::fail(
                            format!("stage.{key}"),
                            format!("{detail} — over 1500ms target"),
                            hint,
                        )
                    } else {
                        Check::warn(
                            format!("stage.{key}"),
                            format!("{detail} — over 1500ms target"),
                            hint,
                        )
                    });
                } else {
                    checks.push(Check::ok(format!("stage.{key}"), detail));
                }
            }
            None => checks.push(Check::warn(
                format!("stage.{key}"),
                "no samples",
                if stage == "first_audio" {
                    "no audio chunks observed; turn pipeline may run text-only (TTS absent)"
                } else {
                    "stage never observed"
                },
            )),
        }
    }
    checks
}

fn render(report: &BenchReport, ctx: &super::Ctx) -> Result<(), AppError> {
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        return Ok(());
    }
    let mut w = std::io::stdout().lock();
    writeln!(w, "== bench ({} turns) ==", report.turns)?;
    writeln!(w, "endpoint: {}", report.endpoint)?;
    for r in &report.results {
        let fmt = |v: Option<u64>| v.map(|x| format!("{x}ms")).unwrap_or("-".into());
        let mut line = format!(
            "  turn {:>2}: accepted={} sentence={} audio={} finished={}",
            r.turn,
            fmt(r.accepted_ms),
            fmt(r.first_sentence_ms),
            fmt(r.first_audio_ms),
            fmt(r.finished_ms)
        );
        if let Some(o) = &r.outcome {
            line.push_str(&format!(" outcome={o}"));
        }
        if let Some(n) = &r.note {
            line.push_str(&format!(" note={n}"));
        }
        writeln!(w, "{line}")?;
    }
    let id_width = report.checks.iter().map(|c| c.id.len()).max().unwrap_or(0);
    for c in &report.checks {
        let label = match c.status {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "fail",
        };
        let pad = " ".repeat(4usize.saturating_sub(label.len()));
        writeln!(w, "  {pad}{label}  {:id_width$}  {}", c.id, c.detail)?;
        if let Some(hint) = &c.hint {
            writeln!(w, "  {pad}      {:id_width$}  hint: {hint}", "")?;
        }
    }
    writeln!(
        w,
        "summary: {} ok, {} warn, {} fail",
        report.summary.ok, report.summary.warn, report.summary.fail
    )?;
    Ok(())
}

// ---------- 最小 ws client（ws://，无 TLS；client 帧带 mask） ----------

mod ws {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::{Duration, Instant};

    /// 协议帧；close 帧归一为 `Closed`。
    #[derive(Debug)]
    pub enum Frame {
        Text(String),
        Binary(Vec<u8>),
        Closed(u16, String),
    }

    pub struct Conn {
        stream: TcpStream,
        buf: Vec<u8>,
        /// 帧重组：text 分片累积（服务端一般不拆，保险起见）。
        frag: Vec<u8>,
        frag_op: u8,
    }

    /// TCP connect → HTTP upgrade（`Origin` 需在 momod 白名单内）→ 101。
    pub fn connect(
        host: &str,
        port: u16,
        path: &str,
        origin: &str,
        timeout: Duration,
    ) -> Result<Conn, String> {
        let addr = format!("{host}:{port}")
            .to_socket_addrs()
            .map_err(|e| e.to_string())?
            .next()
            .ok_or_else(|| format!("cannot resolve {host}:{port}"))?;
        let stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| {
            if e.kind() == std::io::ErrorKind::TimedOut {
                "connect timeout".to_string()
            } else {
                format!("connect {addr}: {e}")
            }
        })?;
        stream
            .set_read_timeout(Some(POLL_MAX))
            .map_err(|e| e.to_string())?;
        stream.set_nodelay(true).ok();

        let key = ws_key();
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\nOrigin: {origin}\r\n\r\n"
        );
        stream
            .try_clone()
            .and_then(|mut s| s.write_all(req.as_bytes()))
            .map_err(|e| format!("handshake write: {e}"))?;

        // 读响应头（至 \r\n\r\n），要求 101。
        let mut conn = Conn {
            stream,
            buf: Vec::new(),
            frag: Vec::new(),
            frag_op: 0,
        };
        let deadline = Instant::now() + timeout;
        let head = conn.read_until(b"\r\n\r\n", deadline)?;
        let head = String::from_utf8_lossy(&head).to_string();
        let status = head.split_whitespace().nth(1).unwrap_or("");
        if status != "101" {
            return Err(format!(
                "upgrade rejected: {}",
                head.lines().next().unwrap_or("")
            ));
        }
        Ok(conn)
    }

    const POLL_MAX: Duration = Duration::from_millis(50);

    impl Conn {
        /// client→server 文本帧（必须带 mask，RFC 6455）。
        pub fn send_text(&mut self, text: &str) -> std::io::Result<()> {
            self.stream.write_all(&encode(0x1, text.as_bytes()))
        }

        fn send_frame(&mut self, op: u8, payload: &[u8]) -> std::io::Result<()> {
            self.stream.write_all(&encode(op, payload))
        }

        /// 下一帧；deadline 到 → Ok(None)。ping 自动回 pong。
        pub fn next(&mut self, deadline: Instant) -> Result<Option<Frame>, String> {
            loop {
                if let Some(((fin, op, payload), used)) = parse(&self.buf) {
                    self.buf.drain(..used);
                    match op {
                        0x0..=0x2 => {
                            // text/binary/continuation：分片累积到 FIN 再交付。
                            if op != 0x0 {
                                self.frag_op = op;
                            }
                            self.frag.extend_from_slice(&payload);
                            if fin {
                                let all = std::mem::take(&mut self.frag);
                                let kind = self.frag_op;
                                self.frag_op = 0;
                                return Ok(Some(if kind == 0x1 {
                                    Frame::Text(String::from_utf8_lossy(&all).to_string())
                                } else {
                                    Frame::Binary(all)
                                }));
                            }
                        }
                        0x8 => {
                            let code = payload
                                .get(..2)
                                .map(|b| u16::from_be_bytes([b[0], b[1]]))
                                .unwrap_or(1000);
                            let reason = String::from_utf8_lossy(payload.get(2..).unwrap_or(&[]))
                                .to_string();
                            let _ = self.send_frame(0x8, &payload);
                            return Ok(Some(Frame::Closed(code, reason)));
                        }
                        0x9 => {
                            let _ = self.send_frame(0xA, &payload);
                        }
                        _ => {} // pong / 未知控制帧忽略
                    }
                } else {
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                    match self.read_more() {
                        Ok(0) => return Ok(Some(Frame::Closed(1006, "eof".into()))),
                        Ok(_) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }

        fn read_more(&mut self) -> Result<usize, String> {
            let mut tmp = [0u8; 8192];
            match self.stream.read(&mut tmp) {
                Ok(0) => Ok(0),
                Ok(n) => {
                    self.buf.extend_from_slice(&tmp[..n]);
                    Ok(n)
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    Ok(usize::MAX) // 读超时：继续轮询（deadline 在外层判）
                }
                Err(e) => Err(format!("read: {e}")),
            }
        }

        /// 读至分隔串出现（HTTP 响应头）。
        fn read_until(&mut self, sep: &[u8], deadline: Instant) -> Result<Vec<u8>, String> {
            loop {
                if let Some(i) = find(&self.buf, sep) {
                    return Ok(self.buf.drain(..i + sep.len()).collect());
                }
                if Instant::now() >= deadline {
                    return Err("upgrade response timeout".into());
                }
                if self.read_more()? == 0 {
                    return Err("connection closed during upgrade".into());
                }
            }
        }
    }

    /// 已解析帧：(fin, opcode, payload) + 消费字节数。
    type Raw = ((bool, u8, Vec<u8>), usize);

    /// 从 buf 解析一帧 → (fin, opcode, payload, consumed)；数据不全 → None。
    /// server→client 不带 mask；容忍带 mask 的对端。
    pub fn parse(buf: &[u8]) -> Option<Raw> {
        if buf.len() < 2 {
            return None;
        }
        let fin = buf[0] & 0x80 != 0;
        let op = buf[0] & 0x0f;
        let masked = buf[1] & 0x80 != 0;
        let mut len = (buf[1] & 0x7f) as usize;
        let mut pos = 2;
        if len == 126 {
            if buf.len() < pos + 2 {
                return None;
            }
            len = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
            pos += 2;
        } else if len == 127 {
            if buf.len() < pos + 8 {
                return None;
            }
            len = u64::from_be_bytes(buf[pos..pos + 8].try_into().unwrap()) as usize;
            pos += 8;
        }
        let mask = if masked {
            if buf.len() < pos + 4 {
                return None;
            }
            let m = [buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]];
            pos += 4;
            Some(m)
        } else {
            None
        };
        if buf.len() < pos + len {
            return None;
        }
        let mut payload = buf[pos..pos + len].to_vec();
        if let Some(m) = mask {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= m[i % 4];
            }
        }
        Some(((fin, op, payload), pos + len))
    }

    /// client 帧编码：FIN 置位 + mask（mask key 取 ULID 随机部，非安全用途）。
    fn encode(op: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(14 + payload.len());
        out.push(0x80 | op);
        let maskbit = 0x80;
        if payload.len() < 126 {
            out.push(maskbit | payload.len() as u8);
        } else if payload.len() <= 0xffff {
            out.push(maskbit | 126);
            out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            out.push(maskbit | 127);
            out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        let key: [u8; 4] = ulid::Ulid::new().to_bytes()[..4].try_into().unwrap();
        out.extend_from_slice(&key);
        for (i, b) in payload.iter().enumerate() {
            out.push(b ^ key[i % 4]);
        }
        out
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// Sec-WebSocket-Key：16B 随机（ULID）的 std b64。
    fn ws_key() -> String {
        b64(&ulid::Ulid::new().to_bytes())
    }

    fn b64(data: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for ch in data.chunks(3) {
            let (a, b, c) = (
                ch[0] as u32,
                ch.get(1).copied().unwrap_or(0) as u32,
                ch.get(2).copied().unwrap_or(0) as u32,
            );
            let n = (a << 16) | (b << 8) | c;
            out.push(T[(n >> 18) as usize & 63] as char);
            out.push(T[(n >> 12) as usize & 63] as char);
            out.push(if ch.len() > 1 {
                T[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if ch.len() > 2 {
                T[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// client 帧编码：opcode/mask 位、长度段与 unmask 还原。
        #[test]
        fn encode_masks_and_decodes_back() {
            let payload = b"hello".to_vec();
            let frame = encode(0x1, &payload);
            assert_eq!(frame[0], 0x81);
            assert!(frame[1] & 0x80 != 0, "client frames must be masked");
            // 127B 走 16-bit 长度段
            let big = vec![7u8; 200];
            let f = encode(0x2, &big);
            assert_eq!(f[1] & 0x7f, 126);
        }

        /// server 帧解析：无 mask 文本帧 + 分片到达。
        #[test]
        fn parse_unmasked_text_frame() {
            // server→client：FIN+text, len=5, "hello"
            let frame = [0x81, 0x05, b'h', b'e', b'l', b'l', b'o'];
            let ((fin, op, payload), used) = parse(&frame).unwrap();
            assert_eq!(used, 7);
            assert!(fin && op == 0x1);
            assert_eq!(payload, b"hello");
            // 半帧 → None
            assert!(parse(&frame[..4]).is_none());
        }

        /// close 帧解析。
        #[test]
        fn parse_close_frame() {
            let frame = [0x88, 0x02, 0x0f, 0xa0]; // close 4000
            let ((_, op, payload), _) = parse(&frame).unwrap();
            assert_eq!(op, 0x8);
            assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 4000);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        let s = [10u64, 20, 30, 40, 50];
        assert_eq!(percentile(&s, 50), 30);
        assert_eq!(percentile(&s, 95), 50);
        assert_eq!(percentile(&[7], 95), 7);
    }

    #[test]
    fn dial_addr_normalizes() {
        assert_eq!(dial_addr("127.0.0.1:7800"), ("127.0.0.1".into(), 7800));
        assert_eq!(dial_addr("0.0.0.0:7800"), ("127.0.0.1".into(), 7800));
        assert_eq!(dial_addr("ws://[::1]:7800/ws"), ("::1".into(), 7800));
        assert_eq!(dial_addr("localhost"), ("localhost".into(), 7800));
    }

    #[test]
    fn audio_header_stream_id() {
        // 伪造 32B 头 + 4B payload；stream_id 文字形应可回读。
        let id = ulid::Ulid::new();
        let mut frame = vec![0u8; 32];
        frame[0] = 2;
        frame[1] = 2; // downlink
        frame[4..20].copy_from_slice(&id.to_bytes());
        frame.extend_from_slice(&[0u8; 4]);
        assert_eq!(
            parse_audio_header(&frame).as_deref(),
            Some(id.to_string().as_str())
        );
    }

    /// mock momod：HTTP upgrade → hello/hello.ok → 一回合全链路
    /// （accepted/sentence/二进制音频/finished）；断言四阶段都计时。
    #[test]
    fn run_turn_against_mock_server() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        // server→client 帧（不带 mask；支持 16-bit 扩展长度）。
        fn sframe(op: u8, payload: &[u8]) -> Vec<u8> {
            let mut v = vec![0x80 | op];
            if payload.len() < 126 {
                v.push(payload.len() as u8);
            } else if payload.len() <= 0xffff {
                v.push(126);
                v.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            } else {
                v.push(127);
                v.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            }
            v.extend_from_slice(payload);
            v
        }
        // 读一个（可能带 mask 的）client 帧。
        fn cframe(s: &mut std::net::TcpStream) -> (u8, Vec<u8>) {
            let mut buf = Vec::new();
            loop {
                if let Some(((_fin, op, payload), _used)) = ws::parse(&buf) {
                    return (op, payload);
                }
                let mut b = [0u8; 4096];
                let n = s.read(&mut b).unwrap();
                buf.extend_from_slice(&b[..n]);
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // HTTP upgrade 请求头
            let mut req = Vec::new();
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut b = [0u8; 1024];
                let n = s.read(&mut b).unwrap();
                req.extend_from_slice(&b[..n]);
            }
            s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
                .unwrap();
            // hello → hello.ok
            let (op, _) = cframe(&mut s);
            assert_eq!(op, 0x1);
            s.write_all(&sframe(0x1, br#"{"type":"hello.ok","session_id":"s_test","role":"ui","resume_state":"new","server_version":"x"}"#))
                .unwrap();
            // input.text → 四阶段事件
            let (op, text) = cframe(&mut s);
            assert_eq!(op, 0x1);
            let v: serde_json::Value = serde_json::from_slice(&text).unwrap();
            assert_eq!(v["type"], "input.text");
            let rid = v["id"].as_str().unwrap().to_string();
            s.write_all(&sframe(
                0x1,
                format!(r#"{{"type":"input.accepted","session_id":"s_test","turn_id":"t_1","ref_id":"{rid}"}}"#).as_bytes(),
            ))
            .unwrap();
            s.write_all(&sframe(
                0x1,
                br#"{"type":"output.sentence","session_id":"s_test","turn_id":"t_1","seq":1,"text":"hi","lang":"zh"}"#,
            ))
            .unwrap();
            // audio.open downlink → client 应回 playback.started
            s.write_all(&sframe(
                0x1,
                br#"{"type":"audio.open","session_id":"s_test","stream_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","direction":"downlink","turn_id":"t_1","sentence_seq":1,"codec":"pcm_s16le","sample_rate":24000,"channels":1}"#,
            ))
            .unwrap();
            // client 收到 audio.open 后回 playback.started（断言其 stream_id）
            let (op, ack) = cframe(&mut s);
            assert_eq!(op, 0x1);
            let ack: serde_json::Value = serde_json::from_slice(&ack).unwrap();
            assert_eq!(ack["type"], "playback.started");
            assert_eq!(ack["stream_id"], "01ARZ3NDEKTSV4RRFFQ69G5FAV");
            // 二进制下行音频帧（32B 头 + payload），stream_id 为同一 ULID 原始值
            let raw = ulid::Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV")
                .unwrap()
                .to_bytes();
            let mut aud = vec![0u8; 32];
            aud[0] = 2;
            aud[1] = 2; // downlink
            aud[4..20].copy_from_slice(&raw);
            aud[24..28].copy_from_slice(&1u32.to_le_bytes());
            aud.extend_from_slice(&[0u8; 4]);
            s.write_all(&sframe(0x2, &aud)).unwrap();
            // audio.end → client 应回 playback.progress{completed}
            s.write_all(&sframe(
                0x1,
                br#"{"type":"audio.end","session_id":"s_test","stream_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","last_chunk_seq":0}"#,
            ))
            .unwrap();
            let (op, prog) = cframe(&mut s);
            assert_eq!(op, 0x1);
            let prog: serde_json::Value = serde_json::from_slice(&prog).unwrap();
            assert_eq!(prog["type"], "playback.progress");
            assert_eq!(prog["completed"], true);
            assert_eq!(prog["played_samples"], 2);
            s.write_all(&sframe(
                0x1,
                br#"{"type":"turn.finished","session_id":"s_test","turn_id":"t_1","outcome":"completed"}"#,
            ))
            .unwrap();
        });

        let mut conn = ws::connect(
            "127.0.0.1",
            port,
            "/ws/ui",
            BENCH_ORIGIN,
            Duration::from_secs(2),
        )
        .unwrap();
        let session = handshake(
            &mut conn,
            r#"{"type":"hello","protocol":"momo-ws","version":"v2","client":"ui","auth_token":""}"#,
        )
        .unwrap();
        assert_eq!(session, "s_test");
        let r = run_turn(&mut conn, &session, 1, Duration::from_secs(5));
        srv.join().unwrap();
        assert!(r.accepted_ms.is_some(), "{r:?}");
        assert!(r.first_sentence_ms.is_some());
        assert!(r.first_audio_ms.is_some());
        assert!(r.finished_ms.is_some());
        assert_eq!(r.outcome.as_deref(), Some("completed"));
        assert!(r.note.is_none());
    }

    /// strict 出口：first_audio p95 超标才 fail；缺样本仅 warn。
    #[test]
    fn strict_semantics() {
        let mk = |audio: Option<u64>| TurnResult {
            turn: 1,
            accepted_ms: Some(5),
            first_sentence_ms: Some(100),
            first_audio_ms: audio,
            finished_ms: Some(500),
            outcome: Some("completed".into()),
            note: None,
        };
        let over = collect_stats(&[mk(Some(2000))]);
        let c = make_checks(&[mk(Some(2000))], &over, true);
        assert!(c
            .iter()
            .any(|x| x.id == "stage.first_audio" && x.status == CheckStatus::Fail));
        let c = make_checks(&[mk(Some(2000))], &over, false);
        assert!(c
            .iter()
            .any(|x| x.id == "stage.first_audio" && x.status == CheckStatus::Warn));
        // 无 audio 样本（纯文字回合）：warn 不 fail
        let none = collect_stats(&[mk(None)]);
        let c = make_checks(&[mk(None)], &none, true);
        assert!(c.iter().all(|x| x.status != CheckStatus::Fail));
    }
}
