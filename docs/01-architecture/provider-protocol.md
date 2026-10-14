# Provider 接入面协议：ASR `/v1/listen` 与 TTS `/v1/audio/speech`

> ADR-029 的线格式定稿：app ↔ provider 的对外接口对齐主流云服务形态，
> 不再使用 momo-ws 私有协议。`momo-svc-asr` / `momo-svc-tts` 在同一进程内
> 同时暴露本页端点与内部 `/ws`（momo-ws v2，保留兼容，见 momo 仓 protocol.md）。
> gateway 对本页端点只做透明反代（gateway.md §1/§4），不感知 payload。

## 1. ASR：`GET /v1/listen`（WebSocket，Deepgram 对齐）

流式转写：客户端上行原始音频二进制帧，服务端回流 JSON 事件。
一条连接 = 一条音频流（capture 生命周期 = 连接生命周期），无应用层握手帧。

### 1.1 连接与鉴权

```
GET /v1/listen?encoding=linear16&sample_rate=16000&channels=1&vad_events=true
Authorization: Token <key>
```

| 项 | 取值 | 说明 |
|---|---|---|
| `encoding` | `linear16`（默认） | 目前唯一支持的编码；其他值 → close `4400` |
| `sample_rate` | `16000`（默认） | 其他值 → close `4400` |
| `channels` | `1`（默认） | 其他值 → close `4400` |
| `vad_events` | `true`（默认）/`false` | 是否回 `SpeechStarted`/`UtteranceEnd` |
| `interim_results` | `true`/`false`（默认） | 接受的兼容参数；当前实现只产 final 结果 |
| `language` | `zh`/`en`/`ja`/`auto` 等 | 透传给 ASR provider；缺省=provider 自动 |
| `model` | str | 接受的兼容参数；当前实现忽略（模型由服务端配置决定） |
| `endpointing` | int ms | 接受的兼容参数；断句阈值以服务端 `MOMO_VAD_SILENCE_MS` 为准 |

鉴权（对齐 Deepgram）：`Authorization: Token <key>` 头优先（`Bearer` 同义接受）；
不能设头的客户端用 `?token=` 或 `?access_token=` query 兜底。`MOMO_ASR_API_KEY`
未配置时不校验（本地开发语义）；已配置时凭据不符 → 拒绝升级（HTTP `403`）。

### 1.2 客户端 → 服务端

- **二进制帧**：裸 PCM `s16le` 16kHz mono 块（无帧头、无序号——TCP/WS 已保序）。
- `{"type":"KeepAlive"}`：保活，无应答。
- `{"type":"Finalize"}`：强制断句——冲刷 VAD 残留段并转写（对应 capture 结束）。
- `{"type":"CloseStream"}`：Finalize 语义 + 服务端回 `{"type":"CloseStream"}` 后 close `1000`。
- 未知 `type` / 非法 JSON → `{"type":"Error",...}`，连接保持。

### 1.3 服务端 → 客户端（JSON 文本帧）

```json
{"type":"Metadata","request_id":"01J…","model_info":{"name":"sensevoice","version":"v2"},"sample_rate":16000}
```

连接建立后第一个事件。

```json
{"type":"SpeechStarted","timestamp":1.240}
{"type":"UtteranceEnd","last_word_end":3.860,"channel":[0,0]}
```

VAD 边界事件（`vad_events=true` 时）；`timestamp`/`last_word_end` 为流内秒。

```json
{"type":"Results","channel_index":[0,1],"duration":2.62,"start":1.24,
 "is_final":true,"speech_final":true,
 "channel":{"alternatives":[{"transcript":"你好，世界。","confidence":1.0,"words":[]}]}}
```

每个 speech_end 段一条，`is_final`/`speech_final` 恒 `true`（当前 provider 无 interim）。
空文本段不产 `Results`（防空调用触发）。

```json
{"type":"Error","message":"provider not ready","code":"ASR_FAILED"}
```

可恢复错误单独上报；致命错误为 `Error` + close 帧。

### 1.4 Close code

| code | 语义 |
|---|---|
| `1000` | `CloseStream` 正常收尾 |
| `4400` | 参数非法（encoding/sample_rate/channels 不支持、二进制帧非法） |
| `4401` | 鉴权失败（`MOMO_ASR_API_KEY` 已设且凭据不符；升级前拒绝表现为 HTTP `403`） |
| `1011` | 服务端内部错误 |

## 2. TTS：`POST /v1/audio/speech`（OpenAI 对齐）

请求-响应式合成，chunked transfer 流式返回音频字节。

### 2.1 请求

```
POST /v1/audio/speech
Authorization: Bearer <key>          # MOMO_TTS_API_KEY 已设时必需
Content-Type: application/json
```

```json
{
  "model": "momo-tts",
  "input": "你好，世界。",
  "voice": "zf_xiaoxiao",
  "response_format": "pcm",
  "speed": 1.0,
  "lang": "zh",
  "emotion": "happy"
}
```

| 字段 | 必需 | 说明 |
|---|---|---|
| `input` | 是 | 非空文本，≤4096 字符（OpenAI 上限口径） |
| `model` | 否 | 兼容字段，当前实现忽略（provider 由服务端配置决定） |
| `voice` | 否 | 音色名，透传 `VoiceRef.name`；缺省=`MOMO_TTS_VOICE`，再缺省=按 `lang` 默认 |
| `response_format` | 否 | `pcm`（默认，流式 s16le 24kHz mono）或 `wav`（整段缓冲后单次返回） |
| `speed` | 否 | 0.25–4.0；透传给支持 speed/rate 的 provider |
| `lang` | 否 | **momo 扩展**：`zh`/`en`/`ja`/`auto`，缺省=`MOMO_TTS_LANG` |
| `emotion` | 否 | **momo 扩展**：固定 8 tag（neutral/happy/sad/angry/surprised/thinking/shy/sleepy） |

`lang`/`emotion` 为非标扩展字段；严格 OpenAI 客户端不发也无碍，服务端按缺省处理。

### 2.2 响应

- `200` + `Content-Type: audio/pcm`（或 `audio/wav`），`Transfer-Encoding: chunked`：
  `pcm` 边合成边吐 `s16le` 24kHz mono 块；`wav` 收集完毕后带 RIFF 头单次返回。
- 错误一律 OpenAI 信封：

```json
{"error":{"message":"input must be a non-empty string","type":"invalid_request_error","param":"input","code":null}}
```

| status | 场景 |
|---|---|
| `400` | 字段非法（空 input、超长、未知 response_format、speed 越界） |
| `401` | `MOMO_TTS_API_KEY` 已设且 Bearer 不符 |
| `503` | provider 未 ready |
| `500` | 合成失败（`error.type=server_error`） |

客户端断连即取消在途合成（无单独取消端点——HTTP 语义天然覆盖
momo-ws `control.cancel` 的取消职责）。

## 3. 与 momo-ws `/ws` 的关系

| | `/v1/listen` + `/v1/audio/speech`（本页） | `/ws`（momo-ws v2） |
|---|---|---|
| 定位 | **对外 provider 接入面**（app↔svc、gateway 上游、第三方接入） | 内部/遗留链路 |
| 流模型 | 一连接一流；HTTP 单请求单合成 | 一连接多路 stream 复用 |
| 鉴权 | `Authorization` 头 / token query | hello 帧内 `auth_token` |
| 断句/取消 | VAD 服务端内建；断连即取消 | `control.cancel` 应用层消息 |
| 状态 | 保留兼容，不新增能力 | 冻结——新接入一律走本页形态 |

app 侧 endpoint 形态识别（momo 仓 settings.md 详述）：ASR `ws(s)://…/ws`
走 momo-ws、`ws(s)://…/v1/listen`（及其他路径）走本页形态；TTS `http(s)://…`
走 `/v1/audio/speech`、`ws(s)://…` 走 momo-ws。同一 endpoint 槽位自动识别，
用户不感知协议差异。
