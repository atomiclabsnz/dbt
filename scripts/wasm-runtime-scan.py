#!/usr/bin/env python3
"""ferrion-wasm: report calls that COMPILE on wasm32-unknown-unknown but PANIC
(or hang) at runtime in a browser, in the crates `dbt run` links on wasm.

    python3 scripts/wasm-runtime-scan.py              # every remaining hit
    python3 scripts/wasm-runtime-scan.py --summary    # counts only
    python3 scripts/wasm-runtime-scan.py --all        # also the gated hits
    python3 scripts/wasm-runtime-scan.py --strict     # exit 1 on an ungated hit
                                                      # not in LEFTOVERS

On wasm32-unknown-unknown, std has no clock, no threads and no blocking:

  Instant::now / SystemTime::now   panic ("time not implemented on this platform")
  thread::spawn / thread::Builder  panic (cannot spawn a thread)
  thread::sleep / thread::park     panic, or block the only thread forever
  spawn_blocking / block_in_place  tokio cannot start its blocking pool
  tokio::time                      the timer driver reads std Instant
  env::vars / vars_os / temp_dir   panic ("not supported on this platform")
  env::set_var / remove_var        panic (the platform setter returns Err)
  env::current_dir                 an Err (no cwd), which dbt may `.expect()`
  process::exit / abort            unwind into a trap, taking the host with it
  process::id                      panic ("no pids on this platform")
  dirs::home_dir                   None, which dbt `.expect()`s for its leases

The fix is a seam, not a rewrite: time goes through `dbt_vfs::time` (std
natively, `web-time` and no-wait timers on wasm) and threads, sleeps and the
blocking pool through `dbt_vfs::thread` (std/tokio natively, single-threaded
stand-ins on wasm), both by `scripts/wasm-runtime-rewrite.py`; the rest are
`#[cfg(not(target_arch = "wasm32"))]` with a wasm arm, or LEFTOVERS.

A hit is ROUTED when it resolves to the seam: spelled `dbt_vfs::time::..` /
`dbt_vfs::thread::..`, or a bare `Instant` / `thread::` the file imports from
it. A hit is GATED when an enclosing item or statement carries a cfg attribute
that excludes wasm32 (`cfg(not(target_arch = "wasm32"))`, `cfg(not(vfs_memory))`
is NOT counted: memory mode runs natively too), when its module file is
declared behind one, or when it sits in `#[cfg(test)]` code / test files.
Approximate by design: it reads text, not the compiler's cfg evaluation.

Scope: the workspace crates in the wasm32 dependency closure of dbt-main and
dbt-tasks-sa (`cargo metadata --filter-platform wasm32-unknown-unknown`).
Each remaining hit there is either a defect on the run path or a LEFTOVERS
entry saying why it is not reached.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ROOTS = ["dbt-main", "dbt-tasks-sa"]
TARGET = "wasm32-unknown-unknown"

KINDS = [
    ("Instant::now", re.compile(r"\bInstant::now\s*\(")),
    ("SystemTime::now", re.compile(r"\bSystemTime::now\s*\(")),
    ("thread::spawn", re.compile(r"\bthread::spawn\b|\bthread::scope\b")),
    ("thread::Builder", re.compile(r"\bthread::Builder\b")),
    ("thread::sleep", re.compile(r"\bthread::sleep\b")),
    ("park", re.compile(r"\bthread::park\b|\.park\(\)|\bpark_timeout\b")),
    ("spawn_blocking", re.compile(r"\bspawn_blocking\b")),
    ("block_in_place", re.compile(r"\bblock_in_place\b")),
    ("tokio::time", re.compile(r"\btokio::time\b")),
    ("env::vars", re.compile(r"\benv::vars(?:_os)?\s*\(")),
    ("env::set_var", re.compile(r"\benv::(?:set_var|remove_var)\s*\(")),
    ("env::temp_dir", re.compile(r"\benv::temp_dir\s*\(")),
    ("env::current_dir", re.compile(r"\benv::(?:current_dir|set_current_dir)\s*\(")),
    ("process::exit", re.compile(r"\bprocess::(?:exit|abort)\s*\(")),
    ("process::id", re.compile(r"\bprocess::id\s*\(")),
    ("home_dir", re.compile(r"\b(?:dirs|env)::home_dir\s*\(")),
]

# A cfg attribute (outer or inner) whose predicate excludes wasm32.
CFG_ATTR = re.compile(r"#!?\[cfg\((.*?)\)\]", re.S)


def excludes_wasm(pred: str) -> bool:
    p = re.sub(r"\s+", "", pred)
    if p == "test" or p.startswith("all(test") or ",test)" in p or "(test," in p:
        return True
    # not(target_arch="wasm32") directly, or inside all(...)
    if 'not(target_arch="wasm32")' in p and not p.startswith("any("):
        return True
    if p.startswith("not(any(") and 'target_arch="wasm32"' in p:
        return True
    return False


# Justified leftovers: (file relative to crates/, kind, why). A hit matching
# file + kind is reported as LEFTOVER instead of REMAINING.
LEFTOVERS: list[tuple[str, str, str]] = [
    (
        "dbt-dist/src/proc.rs",
        "thread::spawn",
        "pipe drainers after `Command::spawn().ok()?`, which is None on wasm (no processes)",
    ),
    (
        "dbt-dist/src/lib.rs",
        "thread::spawn",
        "PATH discovery for distribution info, not `dbt run`; no PATH on wasm, so it returns first",
    ),
    (
        "dbt-main/src/main_impl.rs",
        "process::exit",
        "the CLI binary's entry (parse/exit codes); a wasm host calls dbt_lib::setup_and_execute_fs",
    ),
    (
        "dbt-main/src/uninstall.rs",
        "process::exit",
        "`dbt uninstall`, not `dbt run`",
    ),
    (
        "dbt-clap-core/src/lib.rs",
        "process::exit",
        "`--version --output json` from the binary's argv, before a host's parse",
    ),
    (
        "dbt-common/src/source_lineage.rs",
        "process::exit",
        "interactive relaunch of the CLI as a subprocess; not on the run path",
    ),
    (
        "dbt-adbc/src/bin/adbc_sync.rs",
        "process::exit",
        "a standalone dev binary",
    ),
    (
        "dbt-adbc/src/bin/repl.rs",
        "process::exit",
        "a standalone dev binary",
    ),
]


def closure() -> list[tuple[str, Path]]:
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--filter-platform", TARGET],
        cwd=ROOT, capture_output=True, text=True, check=True,
    ).stdout
    meta = json.loads(out)
    pkgs = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    local = {
        pid for pid, p in pkgs.items()
        if p.get("source") is None and Path(p["manifest_path"]).is_relative_to(ROOT)
    }
    todo = [pid for pid in local if pkgs[pid]["name"] in ROOTS]
    seen: set[str] = set()
    while todo:
        pid = todo.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            if dep["pkg"] in local and any(k["kind"] is None for k in dep["dep_kinds"]):
                todo.append(dep["pkg"])
    return sorted((pkgs[p]["name"], Path(pkgs[p]["manifest_path"]).parent) for p in seen)


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


def header_gated(src: str, mask: list[bool], end: int) -> bool:
    """Does the item/statement header ending at `end` carry a wasm-excluding cfg?

    The header runs back to the previous code `;`, `{` or `}` (attributes are
    part of it)."""
    j = end - 1
    while j >= 0:
        if mask[j] and src[j] in ";{}":
            # `#[cfg(..)]` contains no braces or semicolons, so this is safe
            break
        j -= 1
    head = src[j + 1 : end]
    return any(excludes_wasm(m.group(1)) for m in CFG_ATTR.finditer(head))


def gated_spans(src: str, mask: list[bool]) -> list[tuple[int, int]]:
    """Spans of brace blocks (and brace-less statements) behind a wasm-excluding cfg."""
    spans = []
    stack: list[int] = []
    for i, c in enumerate(src):
        if not mask[i]:
            continue
        if c == "{":
            stack.append(i)
        elif c == "}" and stack:
            o = stack.pop()
            if header_gated(src, mask, o):
                spans.append((o, i + 1))
    # statements / items without a block: `#[cfg(not(..))] let x = ...;`
    for m in CFG_ATTR.finditer(src):
        if not mask[m.start()] or not excludes_wasm(m.group(1)) or src[m.start() + 1] == "!":
            continue
        j = m.end()
        depth = 0
        while j < len(src):
            if mask[j]:
                if src[j] in "({[":
                    depth += 1
                elif src[j] in ")}]":
                    depth -= 1
                    if depth < 0:
                        break
                    if depth == 0 and src[j] == "}":
                        j += 1
                        break
                elif src[j] in ";," and depth == 0:
                    break
            j += 1
        spans.append((m.start(), j + 1))
    return spans


def file_gated(src: str) -> bool:
    head = src[:4000]
    return any(excludes_wasm(m.group(1)) for m in re.finditer(r"#!\[cfg\((.*?)\)\]", head, re.S))


def gated_modules(src_dir: Path) -> set[Path]:
    """Module files declared behind a wasm-excluding cfg (`#[cfg(..)] mod x;`)."""
    out: set[Path] = set()
    for path in src_dir.rglob("*.rs"):
        text = path.read_text(errors="replace")
        for m in re.finditer(
            r"((?:#\[[^\]]*\]\s*)+)(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;", text
        ):
            if not any(excludes_wasm(a.group(1)) for a in CFG_ATTR.finditer(m.group(1))):
                continue
            name = m.group(2)
            # A `#[path = "…"]` module lives where it says, relative to the declaring
            # file's directory (dbt-docs-server's `server_tests.rs`, #450).
            path_attr = re.search(r'#\[\s*path\s*=\s*"([^"]+)"\s*\]', m.group(1))
            if path_attr:
                out.add(path.parent / path_attr.group(1))
                continue
            base = path.parent if path.name in ("lib.rs", "main.rs", "mod.rs") else path.with_suffix("")
            out.add(base / f"{name}.rs")
            out.add(base / name)
    return out


SEAM_CRATE = "dbt-vfs"  # the seam itself: its wasm arms are the implementation

SEAM_INSTANT = re.compile(r"\buse\s+dbt_vfs::time::(?:Instant\b|\{[^}]*\bInstant\b)")
SEAM_THREAD_MOD = re.compile(r"\buse\s+dbt_vfs::thread(?:\s*;|::\{\s*self\b)")
SEAM_ENV_MOD = re.compile(r"\buse\s+dbt_vfs::(?:env\s*;|\{[^}]*\benv\b)")


def routed(src: str, pos: int, kind: str) -> bool:
    before = src[max(0, pos - 20) : pos]
    if before.endswith(("dbt_vfs::time::", "dbt_vfs::thread::", "dbt_vfs::")):
        return True
    if (kind.startswith("env::") or src.startswith("env::", pos)) and src[pos - 2 : pos] != "::" and SEAM_ENV_MOD.search(src):
        return True
    if kind == "Instant::now" and src[pos - 2 : pos] != "::" and SEAM_INSTANT.search(src):
        return True
    if kind.startswith(("thread::", "park")) and src[pos - 2 : pos] != "::" and SEAM_THREAD_MOD.search(src):
        return True
    return False


def scan():
    for crate, cdir in closure():
        if crate == SEAM_CRATE:
            continue
        src_dir = cdir / "src"
        if not src_dir.is_dir():
            continue
        gmods = gated_modules(src_dir)
        for path in sorted(src_dir.rglob("*.rs")):
            # No skipping by file name (#450): `runnable/test.rs` is production code.
            # Test code is excluded by its `cfg(test)` gate, as every other gate is.
            src = path.read_text(errors="replace")
            if not any(p.search(src) for _k, p in KINDS):
                continue
            rel = path.relative_to(ROOT / "crates").as_posix() if path.is_relative_to(ROOT / "crates") else str(path)
            mod_gated = file_gated(src) or any(path == g or g in path.parents for g in gmods)
            mask = code_mask(src)
            spans = None if mod_gated else gated_spans(src, mask)
            line_starts = [0]
            for m in re.finditer("\n", src):
                line_starts.append(m.end())
            for kind, pat in KINDS:
                for m in pat.finditer(src):
                    if not mask[m.start()]:
                        continue
                    gated = mod_gated or any(a <= m.start() < b for a, b in spans)
                    status = "gated" if gated else ("routed" if routed(src, m.start(), kind) else None)
                    line = _bisect(line_starts, m.start())
                    yield crate, rel, line + 1, kind, status, src[line_starts[line] :].split("\n", 1)[0].strip()


def _bisect(starts: list[int], pos: int) -> int:
    lo, hi = 0, len(starts)
    while lo + 1 < hi:
        mid = (lo + hi) // 2
        if starts[mid] <= pos:
            lo = mid
        else:
            hi = mid
    return lo


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--summary", action="store_true")
    ap.add_argument("--all", action="store_true", help="also list routed and gated hits")
    ap.add_argument("--strict", action="store_true", help="exit 1 on an unexplained ungated hit")
    args = ap.parse_args()

    statuses = ("remaining", "leftover", "routed", "gated")
    counts: dict[str, Counter] = {s: Counter() for s in statuses}
    rows = []
    for crate, rel, line, kind, status, text in scan():
        why = next((w for f, k, w in LEFTOVERS if f == rel and k == kind), None)
        if status is None:
            status = "leftover" if why else "remaining"
        else:
            why = None
        counts[status][kind] += 1
        rows.append((status, rel, line, kind, text, why))

    if not args.summary:
        for status, rel, line, kind, text, why in rows:
            if status in ("gated", "routed") and not args.all:
                continue
            tail = f"   # {why}" if why else ""
            print(f"{status.upper():9} {rel}:{line}  [{kind}]  {text[:110]}{tail}")
        print()
    for status in statuses:
        c = counts[status]
        print(f"{status:9} {sum(c.values()):4}  " + "  ".join(f"{k}={c[k]}" for k, _ in KINDS if c[k]))
    return 1 if args.strict and counts["remaining"] else 0


if __name__ == "__main__":
    sys.exit(main())
