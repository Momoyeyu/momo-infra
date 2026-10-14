# eval/ —— 评测口径

设计：docs/02-repos/momo-models.md「训练边界」。

| 脚本 | 目的 | 阶段 |
|---|---|---|
| `persona_regression.py` | 固定 20 问回归：当前主脑 + 人设 prompt 逐题打分，与上次基线 diff；LoRA 候选同口径 | P3 |
| `tts_ab_test.py` | 盲听 A/B：文本集 × 候选 TTS backend 批量合成 → 打分表 → 汇总"像不像/稳不稳/情感范围" | P2 |
| `asr_rtf.py` | SenseVoice 单段 wav 解码计时，复测 RTF 基线（0.040，laptop CPU×4） | P0 |

- 大脑调用走 `providers/llm/openai_compat` 薄客户端（`MOMO_LLM_ENDPOINT`），仅此场景合法；
  运行时编排器直连大脑不经本层。
- 链路延迟不在此测——归 momo-infra `momoctl bench`。
- 上游不稳（vLLM 实测曾 502）时评测需容忍并标注不健康样本。
