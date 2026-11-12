#!/usr/bin/env python3
"""Materialize/push the fixed multi-repo corpus, then run the #32 retrieval harness.

Usage: multirepo.py LABEL URL KEY --bin /absolute/path/to/lore --work /absolute/scratch/path
The key needs ingest,read. Uses no language model. Reusing --work retains source commits.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess

import retrieval

HERE = Path(__file__).resolve().parent


def prepare(work, binary, url, key):
    corpus = HERE / "testdata/multirepo.json"
    content = corpus.read_bytes()
    data = json.loads(content)
    commits = {}
    env = dict(os.environ, LORE_URL=url, LORE_KEY=key)
    for source, files in data["sources"].items():
        repo = work / source
        if not (repo / ".git").exists():
            repo.mkdir(parents=True, exist_ok=False)
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            for name, text in files.items():
                path = repo / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text, encoding="utf-8")
            subprocess.run(["git", "-C", str(repo), "add", "."], check=True)
            subprocess.run(["git", "-C", str(repo), "commit", "-qm", "Fixture: add product corpus"], check=True)
        # Refuse stale or edited fixtures: matched comparisons must index identical text.
        for name, text in files.items():
            if (repo / name).read_text(encoding="utf-8") != text:
                raise ValueError(f"{repo / name}: fixture differs; use a fresh --work")
        commits[source] = subprocess.check_output(
            ["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()
        subprocess.run([binary, "repo", "push", str(repo), "--name", source], env=env, check=True)
    manifest = {"corpus_sha256": hashlib.sha256(content).hexdigest(), "commits": commits}
    (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("label")
    parser.add_argument("url")
    parser.add_argument("key")
    parser.add_argument("--bin", required=True)
    parser.add_argument("--work", type=Path, required=True)
    args = parser.parse_args()
    args.work.mkdir(parents=True, exist_ok=True)
    manifest = prepare(args.work, args.bin, args.url, args.key)
    print(json.dumps(manifest, indent=2))
    result, out = retrieval.run(
        args.label, args.url, args.key, [str(HERE / "questions/multirepo.json")])
    result["fixture"] = manifest
    Path(out).write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(retrieval.report(result))
    print(f"Wrote {out}")


if __name__ == "__main__":
    main()
