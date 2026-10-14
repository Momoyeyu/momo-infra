#!/usr/bin/env python3
"""参考音频 → 训练集：切分/去噪/响度归一/标注（P2 实现，当前 stub）。

零样本 10-30s 干净参考音 / SFT 10-60min；产出单人声无 BGM 的 24k mono
wav + 标注（口径 docs/02-repos/momo-models.md「训练边界」）。
"""

import argparse


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", required=True, help="raw source material directory")
    parser.add_argument("--output", required=True, help="training set output directory")
    parser.parse_args()
    raise SystemExit("prepare_dataset: implemented in P2 (see momo-docs/02-repos/momo-models.md)")


if __name__ == "__main__":
    main()
