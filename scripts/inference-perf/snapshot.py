#!/usr/bin/env python3
"""Download only bounded tokenizer files from a specific public HF commit."""
import re
import sys
from pathlib import Path

from huggingface_hub import HfApi, snapshot_download

# Root-level files supported by common fast, BPE and sentencepiece tokenizers.
TOKENIZER_FILES = {
    "config.json", "tokenizer.json", "tokenizer_config.json", "special_tokens_map.json",
    "added_tokens.json", "vocab.json", "vocab.txt", "merges.txt", "tokenizer.model",
    "spiece.model", "sentencepiece.bpe.model", "sentencepiece.model", "vocab.model",
    "tokenizer.tiktoken", "vocab.bpe", "chat_template.jinja",
}
MAX_BYTES = 256 * 1024 * 1024
MAX_ENTRIES = 4096


def main():
    if len(sys.argv) != 4:
        return 1
    repo, revision, work = sys.argv[1:]
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        return 1
    api = HfApi(endpoint="https://huggingface.co", token=False)
    if api.model_info(repo, revision=revision).sha != revision:
        return 1
    selected = []
    total = 0
    for index, item in enumerate(api.list_repo_tree(repo, revision=revision, recursive=False)):
        if index >= MAX_ENTRIES:
            return 1
        path = getattr(item, "path", None)
        if path not in TOKENIZER_FILES:
            continue
        size = getattr(item, "size", None)
        if type(size) is not int or size < 0:
            return 1
        total += size
        if total > MAX_BYTES:
            return 1
        selected.append(path)
    if not selected or not ({"tokenizer.json", "tokenizer.model", "spiece.model", "sentencepiece.bpe.model", "sentencepiece.model", "vocab.json", "vocab.txt", "tokenizer.tiktoken"} & set(selected)):
        return 1
    snapshot_download(repo_id=repo, revision=revision, token=False, allow_patterns=selected,
                      local_dir=str(Path(work) / "tokenizer"), cache_dir=str(Path(work) / "hf"), max_workers=4)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:
        # Never print network errors: they may include URLs or credentials.
        sys.exit(1)
