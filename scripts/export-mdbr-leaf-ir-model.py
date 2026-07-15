#!/usr/bin/env python3
"""Export the pinned mdbr-leaf-ir pipeline as one standard FP32 ONNX graph.

Reproducible toolchain used for the production graph:
torch 2.9.0, transformers 4.57.1, sentence-transformers 5.1.2, onnx 1.22.0.
"""

from __future__ import annotations

import argparse
import hashlib
from pathlib import Path

import torch
from sentence_transformers import SentenceTransformer


EXPECTED_SHA256 = "242a1d386f2f63a7daec443399b32d35b4b155b0820ee19b7c81c50436f95e11"


class LeafEmbeddingModel(torch.nn.Module):
    def __init__(self, bert: torch.nn.Module, dense: torch.nn.Module) -> None:
        super().__init__()
        self.bert = bert
        self.dense = dense

    def forward(
        self,
        input_ids: torch.Tensor,
        attention_mask: torch.Tensor,
        token_type_ids: torch.Tensor,
    ) -> torch.Tensor:
        tokens = self.bert(
            input_ids=input_ids,
            attention_mask=attention_mask,
            token_type_ids=token_type_ids,
            return_dict=False,
        )[0]
        mask = attention_mask.unsqueeze(-1).to(tokens.dtype)
        pooled = (tokens * mask).sum(dim=1) / mask.sum(dim=1).clamp(min=1e-9)
        return torch.nn.functional.normalize(self.dense(pooled), p=2, dim=1)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("model_dir", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()

    sentence_model = SentenceTransformer(str(args.model_dir), device="cpu")
    model = LeafEmbeddingModel(
        sentence_model[0].auto_model.eval(),
        sentence_model[2].linear.eval(),
    ).eval()
    inputs = (
        torch.ones((2, 128), dtype=torch.int64),
        torch.ones((2, 128), dtype=torch.int64),
        torch.zeros((2, 128), dtype=torch.int64),
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with torch.inference_mode():
        torch.onnx.export(
            model,
            inputs,
            args.output,
            input_names=["input_ids", "attention_mask", "token_type_ids"],
            output_names=["sentence_embedding"],
            dynamic_axes={
                "input_ids": {0: "batch", 1: "sequence"},
                "attention_mask": {0: "batch", 1: "sequence"},
                "token_type_ids": {0: "batch", 1: "sequence"},
                "sentence_embedding": {0: "batch"},
            },
            opset_version=17,
            do_constant_folding=True,
            dynamo=False,
        )
    digest = hashlib.sha256(args.output.read_bytes()).hexdigest()
    if digest != EXPECTED_SHA256:
        raise SystemExit(
            f"exported graph SHA-256 {digest} does not match {EXPECTED_SHA256}"
        )
    print(f"{digest}  {args.output}")


if __name__ == "__main__":
    main()
