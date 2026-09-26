"""Generate wpt-inventory.json from the pinned upstream git tree.

Deterministic: entries sorted by path; the file carries the generation
command and the pinned commit. Raw bytes come from `git cat-file blob`
(no checkout, no shell expansion of paths).
"""
import hashlib
import json
import os
import subprocess
import sys

REPO = "https://github.com/web-platform-tests/wpt"
COMMIT = "0968c868d8095217d18d86b34c7f21dccae58768"
GIT_DIR = os.path.join(os.environ["TEMP"], "wpt-upstream", ".git")


def git(*args):
    env = dict(os.environ)
    env["GIT_DIR"] = GIT_DIR
    out = subprocess.run(
        ["git", *args], capture_output=True, env=env, check=True
    )
    return out.stdout


def main():
    head = git("rev-parse", "FETCH_HEAD").decode().strip()
    assert head == COMMIT, f"pinned mismatch: {head}"
    tree = git("ls-tree", "-r", "FETCH_HEAD", "--", "FileAPI/").decode()
    entries = []
    for line in tree.splitlines():
        # "<mode> blob <sha>\t<path>"
        meta, path = line.split("\t")
        parts = meta.split()
        assert parts[1] == "blob", line
        blob_sha = parts[2]
        raw = git("cat-file", "blob", blob_sha)
        entries.append(
            {
                "path": path,
                "blob_sha": blob_sha,
                "size": len(raw),
                "sha256": hashlib.sha256(raw).hexdigest(),
            }
        )
    entries.sort(key=lambda e: e["path"])
    inventory = {
        "schema_version": 1,
        "repository": REPO,
        "commit": COMMIT,
        "scope": "FileAPI/",
        "file_count": len(entries),
        "generated_by": "tools/gen-wpt-inventory.py (git cat-file over pinned FETCH_HEAD; no checkout, no network at verify time)",
        "files": entries,
    }
    digest = hashlib.sha256(
        json.dumps(inventory, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    inventory["inventory_sha256"] = digest
    out_path = os.path.join("wpt-inventory.json")
    with open(out_path, "w", encoding="utf-8", newline="\n") as f:
        json.dump(inventory, f, indent=2, sort_keys=False)
        f.write("\n")
    print(f"wrote {out_path}: {len(entries)} files, inventory_sha256={digest}")
    # Sanity: manifest upstream_blob_sha values must match inventory blob_sha.
    manifest = json.load(open("wpt-manifest.json", encoding="utf-8"))
    by_path = {e["path"]: e for e in entries}
    for f in manifest["files"]:
        up = f["upstream_path"]
        inv = by_path.get(up)
        if inv is None:
            print(f"WARN: manifest upstream {up} not in inventory scope")
            continue
        status = "OK " if inv["blob_sha"] == f["upstream_blob_sha"] else "MISMATCH"
        print(f"{status} {up} inv={inv['blob_sha']} manifest={f['upstream_blob_sha']}")


if __name__ == "__main__":
    sys.exit(main())
