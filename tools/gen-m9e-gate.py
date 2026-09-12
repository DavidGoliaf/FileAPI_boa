"""Generate expectations.json + wpt-manifest.json (schema 2) for M9-E.

Reads raw pinned upstream bytes (TEMP/wpt-upstream/.git) and the
hand-audited title table in tools/upstream-titles.json; writes
expectations.json (exact per-subtest rows for all 36 .any.js files +
file-level exclusion rows for all 79 non-.any.js inventory files) and
wpt-manifest.json (schema 2, direct provenance, upstream_sha256).

Deterministic: rows sorted by (upstream_path, subtest); JSON written
with indent=2 + trailing newline. Review metadata: owner m9e,
review_by 2027-09-08, trace ids M9E-WPT-01..06.
"""
import hashlib
import json
import os
import subprocess
import sys

REPO = "https://github.com/web-platform-tests/wpt"
COMMIT = "0968c868d8095217d18d86b34c7f21dccae58768"
GIT_DIR = os.path.join(os.environ["TEMP"], "wpt-upstream", ".git")
REVIEW_BY = "2027-09-08"
OWNER = "m9e"

# Non-.any.js inventory -> concrete host capability (never browser-only).
NON_ANY_CAPABILITY = {
    ".html": "navigation",
    ".window.js": "navigation",
    ".worker.js": "worker-runtime",
    "FileReaderSync.worker.js": "worker-runtime",
    "idlharness": "network-wpt-server",
    "send-file-formdata": "fetch",
    "form-helper": "fetch",
    "upload.txt": "network-wpt-server",
    "upload.zip": "network-wpt-server",
    "echo-content": "network-wpt-server",
    "file_test1.txt": "network-wpt-server",
    "blue-100x100.png": "network-wpt-server",
    "common.js": "network-wpt-server",
    "create-helper": "fetch",
    "fetch-tests": "fetch",
    "revoke-helper": "fetch",
    "historical-serviceworker": "worker-runtime",
    ".yml": "network-wpt-server",
}


def git(*args):
    env = dict(os.environ)
    env["GIT_DIR"] = GIT_DIR
    return subprocess.run(["git", *args], capture_output=True, env=env, check=True).stdout


def blob_of(path):
    meta = git("ls-tree", "FETCH_HEAD", "--", path).decode().split()
    assert meta[1] == "blob", path
    raw = git("cat-file", "blob", meta[2])
    return raw, meta[2]


def non_any_capability(path):
    for key, cap in NON_ANY_CAPABILITY.items():
        if key in path:
            return cap
    if path.endswith(".js"):
        return "worker-runtime"
    return "network-wpt-server"


def main():
    titles = json.load(open("tools/upstream-titles.json", encoding="utf-8"))
    inv = json.load(open("wpt-inventory.json", encoding="utf-8"))
    assert inv["commit"] == COMMIT
    by_path = {e["path"]: e for e in inv["files"]}
    assert set(titles.keys()) == set(
        e["path"] for e in inv["files"] if e["path"].endswith(".any.js")), \
        "titles table must cover exactly all .any.js files"

    expectations = []
    manifest_files = []
    for path in sorted(titles.keys()):
        entry = titles[path]
        raw, blob_sha = blob_of(path)
        assert blob_sha == by_path[path]["blob_sha"], f"blob drift {path}"
        sha256 = hashlib.sha256(raw).hexdigest()
        assert sha256 == by_path[path]["sha256"], f"sha drift {path}"
        if path in ("FileAPI/fileReader.any.js", "FileAPI/idlharness.any.js",
                    "FileAPI/unicode.any.js"):
            group = "FileAPI/root"
        elif path.startswith("FileAPI/url/"):
            group = "FileAPI/url"
        else:
            group = "FileAPI/" + path[len("FileAPI/"):].split("/")[0]
        stem = path.rsplit("/", 1)[-1]
        assert stem.endswith(".any.js")
        # Lowercase corpus names EXCEPT the seven legacy M7 adapted files,
        # which keep their exact names and are overwritten in place below
        # (their upstream_path values are remapped to the new files).
        # Rationale: the repo may sit on a case-insensitive filesystem
        # (Windows/macOS), where `Blob-constructor.js` and
        # `blob-constructor.js` are one file.
        corpus = "corpus/" + (stem[:-len(".any.js")] + ".js").lower()
        file_capability = entry["capability"]
        subs = []
        for row in entry["subtests"]:
            subtest = row["subtest"]
            # Effective capability: the subtest-level value wins; when the
            # row omits it, the file-level capability applies (the loader
            # inherits it the same way, so manifest and expectations agree
            # exactly and `resolve_expectations` compares effective values).
            effective_capability = row.get("capability", "") or file_capability
            rec = {
                "upstream_path": path,
                "test": stem,
                "subtest": row["subtest"],
                "status": row.get("status", "PASS"),
                "classification": row.get("classification", "supported"),
                "capability": effective_capability,
                "reason": row.get("reason", ""),
                "owner": row.get("owner", OWNER),
                "review_by": row.get("review_by", REVIEW_BY),
                "trace": row["trace"],
                "spec_section": row.get("spec_section", ""),
                "issue": row.get("issue", ""),
                "adapter": "direct",
            }
            expectations.append(rec)
            subs.append({
                "test": stem,
                "subtest": row["subtest"],
                "status": row.get("status", "PASS"),
                "reason": row.get("reason", ""),
                "capability": effective_capability,
                "owner": row.get("owner", OWNER),
                "review_by": row.get("review_by", REVIEW_BY),
                "trace": row["trace"],
                "classification": row.get("classification", "supported"),
                "spec_section": row.get("spec_section", ""),
                "issue": row.get("issue", ""),
            })
        manifest_files.append({
            "path": corpus,
            "upstream_path": path,
            "upstream_blob_sha": blob_sha,
            "upstream_sha256": sha256,
            "sha256": sha256,
            "group": group,
            "capability": entry["capability"],
            "provenance": "direct",
            "subtests": subs,
        })

    # File-level exclusion rows for every non-.any.js inventory file.
    non_any = sorted(e["path"] for e in inv["files"] if not e["path"].endswith(".any.js"))
    for path in non_any:
        cap = non_any_capability(path)
        stem = path.rsplit("/", 1)[-1]
        rec = {
            "upstream_path": path,
            "test": stem,
            "subtest": "file-level exclusion",
            "status": "NOTRUN",
            "classification": "unsupported-host-capability",
            "capability": cap,
            "reason": f"requires {cap}; no JS-only subtest surface in this harness",
            "owner": OWNER,
            "review_by": REVIEW_BY,
            "trace": "M9E-WPT-03",
            "spec_section": "",
            "issue": "QUESTIONS.md Q1-Q3",
            "adapter": "",
        }
        expectations.append(rec)

    expectations.sort(key=lambda r: (r["upstream_path"], r["subtest"]))
    for f in manifest_files:
        f["subtests"].sort(key=lambda s: s["subtest"])
        assert f["subtests"], f"no executable rows for {f['upstream_path']}"
    manifest_files.sort(key=lambda f: f["upstream_path"])

    # Project-owned FileList host-fixture file (§6): no upstream .any.js
    # covers FileList (only filelist.html, which needs html-file-input),
    # so the handwritten corpus file below executes against the real
    # host-created FileList. classification project-acceptance: never WPT.
    corpus_dir = os.path.join("crates", "boa_fapi_wpt", "corpus")
    fixture_src = os.path.join(corpus_dir, "filelist-host.js")
    fixture_raw = open(fixture_src, "rb").read()
    fixture_sha = hashlib.sha256(fixture_raw).hexdigest()
    fixture_subs = [
        "FileList has no public constructor",
        "host FileList length and brand",
        "host FileList identity and order",
        "host FileList out-of-range behavior",
        "host FileList descriptors",
        "host FileList iteration contract",
    ]
    manifest_files.append({
        "path": "corpus/filelist-host.js",
        "upstream_path": "FileAPI/filelist-section/filelist.html",
        "upstream_blob_sha": by_path["FileAPI/filelist-section/filelist.html"]["blob_sha"],
        "upstream_sha256": by_path["FileAPI/filelist-section/filelist.html"]["sha256"],
        "sha256": fixture_sha,
        "group": "FileAPI/filelist-section",
        "capability": "filelist",
        "provenance": "adapted",
        "adapter": "m9e-filelist-fixture-01",
        "fixture": "filelist",
        "subtests": [{
            "test": "filelist-host",
            "subtest": name,
            "status": "PASS",
            "reason": "",
            "capability": "filelist",
            "owner": OWNER,
            "review_by": REVIEW_BY,
            "trace": "M9E-WPT-04",
            "classification": "project-acceptance",
            "spec_section": "FileAPI WD FileList",
            "issue": "",
        } for name in fixture_subs],
    })
    for name in fixture_subs:
        expectations.append({
            "upstream_path": "FileAPI/filelist-section/filelist.html",
            "test": "filelist-host",
            "subtest": name,
            "status": "PASS",
            "classification": "project-acceptance",
            "capability": "filelist",
            "reason": "",
            "owner": OWNER,
            "review_by": REVIEW_BY,
            "trace": "M9E-WPT-04",
            "spec_section": "FileAPI WD FileList",
            "issue": "",
            "adapter": "m9e-filelist-fixture-01",
        })
    manifest_files.sort(key=lambda f: f["upstream_path"])

    # Materialize corpus/: for `direct` files the corpus bytes ARE the raw
    # upstream bytes (byte-identical provenance, verified by sha256 before
    # every run). Deterministic write: LF newlines preserved from git.
    # NOTE: git on Windows may normalize line endings on checkout; write
    # then verify byte-identity and fail loudly on any drift.
    # Case-insensitivity guard: the seven legacy M7 adapted names
    # (blob-constructor.js, blob-slice.js, file-constructor.js,
    # filelist-section.js, reading-data-section.js, filereader-read.js,
    # bloburl-create-revoke.js) collide modulo case with new direct files.
    # They are removed via git before writing (then re-added as raw), so
    # the working tree never holds two case-variants of one name.
    os.makedirs(corpus_dir, exist_ok=True)
    # The FileList fixture source is handwritten (kept, never overwritten).
    assert os.path.exists(os.path.join(corpus_dir, "filelist-host.js")), \
        "missing handwritten corpus/filelist-host.js"
    legacy_names = ["blob-constructor.js", "blob-slice.js", "file-constructor.js",
                    "filelist-section.js", "reading-data-section.js",
                    "filereader-read.js", "bloburl-create-revoke.js"]
    legacy_paths = ["crates/boa_fapi_wpt/corpus/" + n for n in legacy_names]
    subprocess.run(["git", "rm", "-q", "--cached", "--ignore-unmatch"] + legacy_paths,
                   check=False)
    for name in legacy_names:
        candidate = os.path.join(corpus_dir, name)
        if os.path.exists(candidate):
            os.remove(candidate)
    for path in sorted(titles.keys()):
        raw, _ = blob_of(path)
        stem = path.rsplit("/", 1)[-1]
        name = (stem[:-len(".any.js")] + ".js").lower()
        dest = os.path.join(corpus_dir, name)
        with open(dest, "wb") as f:
            f.write(raw)
        check = open(dest, "rb").read()
        assert check == raw, f"line-ending drift writing {dest}"
        assert hashlib.sha256(check).hexdigest() == by_path[path]["sha256"], \
            f"sha drift writing {dest}"

    with open("expectations.json", "w", encoding="utf-8", newline="\n") as f:
        json.dump({
            "schema_version": 1,
            "repository": REPO,
            "commit": COMMIT,
            "review_by_default": REVIEW_BY,
            "note": "Hand-audited per M9-E section 4 from tools/upstream-titles.json. status: PASS (must run green) | FAIL (open defect, needs issue) | NOTRUN (unsupported-host-capability with closed-list capability, or project-acceptance which is never WPT). Wildcards forbidden; every row is an exact (test, subtest) id.",
            "expectations": expectations,
        }, f, indent=2, ensure_ascii=False)
        f.write("\n")

    with open("wpt-manifest.json", "w", encoding="utf-8", newline="\n") as f:
        json.dump({
            "schema_version": 2,
            "source": {"repository": REPO, "commit": COMMIT, "license": "BSD-3-Clause"},
            "default_timeout_ms": 30000,
            "corpus_root": "crates/boa_fapi_wpt/corpus",
            "files": manifest_files,
        }, f, indent=2, ensure_ascii=False)
        f.write("\n")

    n_pass = sum(1 for r in expectations if r["status"] == "PASS")
    n_notrun = sum(1 for r in expectations if r["status"] == "NOTRUN")
    print(f"expectations: {len(expectations)} rows "
          f"({n_pass} PASS, {n_notrun} NOTRUN), manifest: {len(manifest_files)} files")


if __name__ == "__main__":
    sys.exit(main())
