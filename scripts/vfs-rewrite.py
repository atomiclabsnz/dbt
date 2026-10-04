#!/usr/bin/env python3
"""ferrion-wasm: route dbt's run-path filesystem access through `dbt-vfs`.

On wasm32-unknown-unknown `std::fs` returns `Unsupported`, so every project read
and write on the run path must go through `dbt_vfs` (which is literally
`std::fs` / `walkdir` / `tokio::fs` natively without its `memory` feature, and an
in-memory filesystem on wasm or with `dbt-vfs/memory`).

This script is the maintenance story for dbt upgrades: run it on a fresh
checkout of the run-path crates and it reproduces the rewrite. It is
idempotent (a second run changes nothing).

    python3 scripts/vfs-rewrite.py            # rewrite in place
    python3 scripts/vfs-rewrite.py --check    # exit 1 if a rewrite is pending
    python3 scripts/vfs-rewrite.py --stats    # per-crate counts of what it rewrites

then `cargo fmt -p <each crate in CRATES>` (the rewrite is not formatter-aware;
`--check` is still clean on the formatted tree). Upgrade recipe: re-apply the
fork's earlier commits, run this, run the three gates (native check, native
check with `--features dbt-vfs/memory`, wasm32 check), and add an EXCEPTIONS
entry for each `no method named vfs_*` the compiler reports.

Rewrites, outside `#[cfg(test)]` modules, test files and `//` comment lines:

  std::fs / ::std::fs        -> dbt_vfs::fs     (incl. `use std::{fs, ..}` groups)
  tokio::fs                  -> dbt_vfs::tokio_fs
  walkdir::                  -> dbt_vfs::walkdir::
  <path>.exists() etc.       -> <path>.vfs_exists() etc. (+ `use dbt_vfs::PathExt as _;`)

plus the literal PATCHES below, and adds `dbt-vfs = { workspace = true }` to
each crate it touches. The path methods are rewritten blindly (minus receivers
NON_PATH_RECEIVER recognises); a receiver that is not a path is rejected by the
compiler (`cargo check -p dbt-main -p dbt-tasks-sa` with `dbt-vfs/memory`
natively, or on wasm32) and recorded in EXCEPTIONS.

What is deliberately not routed is listed in NOT_ROUTED.
"""

from __future__ import annotations

import argparse
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The crates on `dbt run`'s path whose filesystem access is routed.
CRATES = [
    "dbt-adapter",
    "dbt-auth",
    "dbt-clap-core",
    "dbt-cloud-config",
    "dbt-common",
    "dbt-compilation",
    "dbt-csv",
    "dbt-deps",
    "dbt-df-providers",
    "dbt-error",
    "dbt-jinja-ctx",
    "dbt-jinja-utils",
    "dbt-loader",
    "dbt-main",
    "dbt-metadata",
    "dbt-metadata-parquet",
    "dbt-parser",
    "dbt-profile",
    "dbt-scheduler",
    "dbt-schema-store",
    "dbt-schemas",
    "dbt-state",
    "dbt-tasks-core",
    "dbt-tasks-sa",
    "dbt-tracing",
]

# Files left on the real filesystem on purpose (file relative to crates/, why).
SKIP_FILES = [
    ("dbt-main/src/update.rs", "self-update replaces the real binary; native only"),
]

# Extra dbt-vfs features per crate.
CRATE_FEATURES = {
    # they read parquet through fs::File (parquet's ChunkReader)
    "dbt-metadata-parquet": ["parquet"],
    "dbt-schema-store": ["parquet"],
}

# Path methods that touch the filesystem, and their PathExt spelling.
PATH_METHODS = {
    "exists": "vfs_exists",
    "try_exists": "vfs_try_exists",
    "is_file": "vfs_is_file",
    "is_dir": "vfs_is_dir",
    "canonicalize": "vfs_canonicalize",
    "read_dir": "vfs_read_dir",
    "symlink_metadata": "vfs_symlink_metadata",
}

# Receivers that are never paths (FileType / Metadata): a path-method call
# whose receiver text ends with one of these is left alone.
NON_PATH_RECEIVER = re.compile(
    r"(?:file_type\(\)\??|metadata\(\)\??|\bft|\bfile_type|\bmeta|\bmetadata|\bmd)\s*$"
)

# Compile-driven exceptions: (file relative to crates/, original line
# stripped, why). Every rewrite on such a line is skipped.
EXCEPTIONS: list[tuple[str, str, str]] = [
    ("dbt-loader/src/upload_artifact_ingest.rs", '.map(|m| m.is_file() && m.len() > 0)', "receiver is Metadata"),
]

# One-off edits (file, old, new, why), applied to the mechanically rewritten
# text. Idempotent: skipped when `new` is already there (a `REGEX:` old is
# skipped when it no longer matches).
PATCHES: list[tuple[str, str, str, str]] = [
    (
        "dbt-common/src/tokiofs.rs",
        """// ferrion-wasm: tokio refuses its `fs` feature on every wasm target (it is
// `std::fs` behind `spawn_blocking`, and wasm has no blocking pool). On wasm the
// same wrappers run `std::fs` inline. That COMPILES on wasm32-unknown-unknown,
// where every call returns `Unsupported` at runtime; an embedder must supply the
// project through a virtual filesystem above this layer (or via wasip1 preopens).
#[cfg(not(target_arch = "wasm32"))]
use dbt_vfs::tokio_fs as afs;
""",
        """// ferrion-wasm: every wrapper goes through dbt_vfs::tokio_fs, which is
// tokio::fs natively and, on wasm32 (where tokio refuses `fs`) or with
// `dbt-vfs/memory`, the in-memory filesystem with every call inline.
use dbt_vfs::tokio_fs as afs;
""",
        "tokiofs: one afs, the VFS",
    ),
    (
        "dbt-common/src/tokiofs.rs",
        "REGEX:(?s)\n#\\[cfg\\(target_arch = \"wasm32\"\\)\\]\nmod afs \\{.*?\n\\}\n",
        "\n",
        "tokiofs: drop the wasm-only std::fs shim (dbt_vfs::tokio_fs replaces it)",
    ),
    (
        "dbt-common/src/tokiofs.rs",
        """pub struct File {}
#[cfg(not(target_arch = "wasm32"))]
impl File {""",
        """pub struct File {}
// ferrion-wasm: on every target (dbt_vfs::tokio_fs has a File everywhere).
impl File {""",
        "tokiofs: File::create on every target",
    ),
    (
        "dbt-loader/src/loader.rs",
        """        // add() returns Option<Error> where None means success and Some(err) is an error
        match builder.add(&dbtignore_path) {""",
        """        // add() returns Option<Error> where None means success and Some(err) is an error
        // ferrion-wasm: GitignoreBuilder::add reads the file with std::fs; read it
        // through the VFS and add its lines instead (what add() does).
        match fs::read_to_string(&dbtignore_path)
            .map_err(ignore::Error::from)
            .and_then(|text| {
                for line in text.lines() {
                    builder.add_line(Some(dbtignore_path.clone()), line)?;
                }
                Ok(())
            })
            .err()
        {""",
        "dbtignore: read through the VFS, not GitignoreBuilder::add's std::fs",
    ),
]

# Filesystem access deliberately left alone (reported, not rewritten).
NOT_ROUTED = [
    ("dbt-common/src/stdfs.rs, dbt-error/src/utils.rs", "dunce::canonicalize: Windows only"),
    ("dbt-scheduler, dbt-state", "glob::Pattern: pattern matching only, no filesystem"),
]

USE_PATHEXT = "use dbt_vfs::PathExt as _;\n"


# ---------------------------------------------------------------------------
# Test regions


def is_test_file(rel: Path) -> bool:
    name = rel.name
    return (
        "tests" in rel.parts[:-1]
        or "test" in rel.parts[:-1]
        or name in ("tests.rs", "test.rs", "test_utils.rs", "testing.rs")
        or name.endswith("_tests.rs")
        or name.endswith("_test.rs")
        or name.startswith("test_")
    )


def code_mask(src: str) -> list[bool]:
    """True for each char that is code (not a comment, string or char literal)."""
    n = len(src)
    mask = [True] * n
    i = 0
    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            mask[i:j] = [False] * (j - i)
            i = j
            continue
        if src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            mask[i:j] = [False] * (j - i)
            i = j
            continue
        m = re.match(r'b?r(#*)"', src[i : i + 260])
        if m and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            close = '"' + m.group(1)
            j = src.find(close, i + m.end())
            j = n if j < 0 else j + len(close)
            mask[i:j] = [False] * (j - i)
            i = j
            continue
        if c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            j = min(j + 1, n)
            mask[i:j] = [False] * (j - i)
            i = j
            continue
        if c == "'":
            m = re.match(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.)|[^\\'])'", src[i : i + 12])
            if m:
                mask[i : i + m.end()] = [False] * m.end()
                i += m.end()
                continue
        i += 1
    return mask


def test_regions(src: str, mask: list[bool]) -> list[tuple[int, int]]:
    """Spans of `#[cfg(test)] mod x { ... }` blocks."""
    out = []
    pat = r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{"
    for m in re.finditer(pat, src):
        if not mask[m.start()]:
            continue
        depth, j = 1, m.end()
        while j < len(src) and depth:
            if mask[j]:
                if src[j] == "{":
                    depth += 1
                elif src[j] == "}":
                    depth -= 1
            j += 1
        out.append((m.start(), j))
    return out


# ---------------------------------------------------------------------------
# The rewrite


def rewrite_use_std_group(text: str) -> str:
    """`use std::{fs, io};` -> `use std::{io};` + `use dbt_vfs::fs;` (one line)."""

    def fix(m: re.Match) -> str:
        items = [s.strip() for s in m.group(1).split(",") if s.strip()]
        fs_items = [s for s in items if s == "fs" or s.startswith("fs::")]
        if not fs_items:
            return m.group(0)
        rest = [s for s in items if s not in fs_items]
        uses = []
        if rest:
            uses.append("use std::{" + ", ".join(rest) + "};")
        uses += ["use dbt_vfs::" + s + ";" for s in fs_items]
        return "\n".join(uses)

    return re.sub(r"use std::\{([^{}]*)\};", fix, text)


SUBS = [
    ("std::fs", re.compile(r"(?<![\w:])(?:::)?std::fs\b"), "dbt_vfs::fs"),
    ("tokio::fs", re.compile(r"\buse tokio::fs;"), "use dbt_vfs::tokio_fs as fs;"),
    ("tokio::fs", re.compile(r"(?<![\w:])tokio::fs\b"), "dbt_vfs::tokio_fs"),
    ("walkdir", re.compile(r"(?<![\w:])walkdir::"), "dbt_vfs::walkdir::"),
]

PATH_CALL = re.compile(r"\.(\s*)(" + "|".join(PATH_METHODS) + r")\(\)")


def in_comment_or_string(line: str, pos: int) -> bool:
    before = line[:pos]
    return "//" in before or before.count('"') % 2 == 1


# Virtual file contents: everything is computed in memory and written at the
# end, so --check and the patches see the rewritten text without touching disk.
FILES: dict[Path, str] = {}


def read(path: Path) -> str:
    if path not in FILES:
        FILES[path] = path.read_text()
    return FILES[path]


def rewrite_file(path: Path, rel: str, stats: Counter) -> bool:
    src = read(path)
    exc_lines = {line for f, line, _why in EXCEPTIONS if f == rel}
    regions = test_regions(src, code_mask(src))

    out = []
    used_pathext = False
    pos = 0
    for line in src.splitlines(keepends=True):
        start, pos = pos, pos + len(line)
        if (
            line.strip() in exc_lines
            or line.lstrip().startswith("//")
            or any(a <= start < b for a, b in regions)
        ):
            out.append(line)
            continue
        new = line
        if "use std::{" in new:
            new2 = rewrite_use_std_group(new)
            if new2 != new:
                stats["std::fs"] += 1
                new = new2
        for key, pat, repl in SUBS:
            new, k = pat.subn(repl, new)
            stats[key] += k

        def path_call(m: re.Match, text: str) -> str:
            nonlocal used_pathext
            if in_comment_or_string(text, m.start()) or NON_PATH_RECEIVER.search(text[: m.start()]):
                return m.group(0)
            stats["path ." + m.group(2)] += 1
            used_pathext = True
            return "." + m.group(1) + PATH_METHODS[m.group(2)] + "()"

        new = PATH_CALL.sub(lambda m, t=new: path_call(m, t), new)
        out.append(new)

    text = "".join(out)
    if used_pathext and USE_PATHEXT not in text:
        text = insert_use(text, USE_PATHEXT)
    if text == src:
        return False
    FILES[path] = text
    return True


def insert_use(text: str, use: str) -> str:
    """Before the first top-level `use`, else after the leading attributes/docs."""
    m = re.search(r"^(?:pub(?:\([^)]*\))?\s+)?use\s", text, re.M)
    if m:
        return text[: m.start()] + use + text[m.start() :]
    lines = text.splitlines(keepends=True)
    i = 0
    while i < len(lines) and (
        lines[i].startswith("//!") or lines[i].startswith("#![") or not lines[i].strip()
    ):
        i += 1
    return "".join(lines[:i]) + use + "".join(lines[i:])


def apply_patch(rel: str, old: str, new: str, stats: Counter) -> bool:
    path = ROOT / "crates" / rel
    text = read(path)
    if old.startswith("REGEX:"):
        pat = re.compile(old[len("REGEX:") :])
        if not pat.search(text):
            return False
        text2 = pat.sub(lambda _m: new, text, count=1)
    else:
        if new in text:
            return False
        if old not in text:
            sys.exit(f"vfs-rewrite: patch anchor not found in {rel}:\n{old[:300]}")
        text2 = text.replace(old, new, 1)
    stats["patches"] += 1
    FILES[path] = text2
    return True


def add_dep(crate: str) -> bool:
    toml = ROOT / "crates" / crate / "Cargo.toml"
    text = read(toml)
    if re.search(r"^dbt-vfs\s*=", text, re.M):
        return False
    m = re.search(r"^\[dependencies\]\n", text, re.M)
    if not m:
        sys.exit(f"vfs-rewrite: no [dependencies] in {toml}")
    feats = CRATE_FEATURES.get(crate)
    dep = "dbt-vfs = { workspace = true"
    dep += (", features = [" + ", ".join(f'"{f}"' for f in feats) + "] }") if feats else " }"
    FILES[toml] = text[: m.end()] + dep + "\n" + text[m.end() :]
    return True


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="exit 1 if anything would change")
    ap.add_argument("--stats", action="store_true", help="print per-crate counts")
    args = ap.parse_args()

    per_crate: dict[str, Counter] = defaultdict(Counter)
    changed: list[str] = []
    for crate in CRATES:
        src = ROOT / "crates" / crate / "src"
        touched = False
        for path in sorted(src.rglob("*.rs")):
            relp = path.relative_to(src)
            if is_test_file(relp):
                continue
            rel = f"{crate}/src/{relp.as_posix()}"
            if any(rel == f for f, _why in SKIP_FILES):
                continue
            if rewrite_file(path, rel, per_crate[crate]):
                touched = True
                changed.append(rel)
        if touched or any(p[0].startswith(crate + "/") for p in PATCHES):
            if add_dep(crate):
                changed.append(f"{crate}/Cargo.toml")
    patch_stats: Counter = Counter()
    for rel, old, new, _why in PATCHES:
        if apply_patch(rel, old, new, patch_stats):
            changed.append(f"{rel} (patch)")

    if args.stats:
        # Counts describe this run: on an already-rewritten tree they are 0.
        total: Counter = Counter()
        for crate in CRATES:
            if per_crate[crate]:
                total.update(per_crate[crate])
                print(f"{crate:22} " + " ".join(f"{k}={v}" for k, v in sorted(per_crate[crate].items())))
        print(f"{'TOTAL':22} " + " ".join(f"{k}={v}" for k, v in sorted(total.items())))
        print(
            f"patches applied: {patch_stats['patches']}; "
            f"exceptions recorded: {len(EXCEPTIONS)}"
        )
    if args.check:
        for c in changed:
            print("pending:", c)
        return 1 if changed else 0
    for path, text in FILES.items():
        if path.read_text() != text:
            path.write_text(text)
    print(f"vfs-rewrite: {len(changed)} change(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
