# GPT-SoVITS SFT（备选克隆路线）

GPT-SoVITS 走其官方流水线（不 vendor 训练码）。此文件只记录 pin 版本与步骤；
设计口径见 docs/02-repos/momo-models.md「训练边界」。

- 官方仓库 pin commit：TODO（P2 实测时填写）
- 数据放置：`prepare_dataset.py` 产物目录 → SoVITS 期望的 `output/` 布局：TODO
- 产物导出：`$MOMO_MODELS_DIR/tts/ft-sovits-<date>/`，并追加 `models/MANIFEST.toml` 条目
- 推理侧建议子进程隔离而非 import（代码侵入性强，见 docs/02-repos/momo-models.md「provider 与产物」已知坑）
