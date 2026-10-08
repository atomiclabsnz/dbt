#!/usr/bin/env python3
"""ferrion-wasm: route dbt's run-path clock, timer, sleep, thread and process
environment calls through `dbt_vfs::time` / `dbt_vfs::thread` / `dbt_vfs::env`.

On wasm32-unknown-unknown these compile and then PANIC at runtime (no clock,
no threads, no blocking pool, tokio's timer driver reads std's clock, and
`env::vars`/`set_var`/`temp_dir` panic for want of a process environment).
`dbt_vfs::time` / `dbt_vfs::thread` / `dbt_vfs::env` are std/tokio natively and
single-threaded / in-memory stand-ins on wasm32 (see their module docs), so
natively nothing changes.

    python3 scripts/wasm-runtime-rewrite.py            # rewrite in place
    python3 scripts/wasm-runtime-rewrite.py --check    # exit 1 if a rewrite is pending
    python3 scripts/wasm-runtime-rewrite.py --stats    # per-crate counts

then `cargo fmt -p <each touched crate>`, the gates, and
`python3 scripts/wasm-runtime-scan.py` for what is left. Same conventions as
scripts/vfs-rewrite.py (whose helpers it imports): idempotent, `#[cfg(test)]`
modules / test files / comment lines are left alone, and so is code already
behind a `cfg(not(target_arch = "wasm32"))` (scan.gated_spans).

Scope: every workspace crate in the wasm32 dependency closure of dbt-main and
dbt-tasks-sa (the scan's closure), minus dbt-vfs itself and SKIP_FILES.

Rewrites:

  use std::time::{.., Instant, ..}       -> use dbt_vfs::time::{..}  (the std::time
  (also nested in `use std::{..}`)          leaves of that `use` move together;
                                            dbt_vfs::time re-exports std::time)
  std::time::Instant                     -> dbt_vfs::time::Instant
  [std::time::|time::]SystemTime::now()  -> dbt_vfs::time::system_now()
  tokio::time::{sleep,timeout,interval,
    MissedTickBehavior,Duration,error}   -> dbt_vfs::time::..  (also in `use`)
  [std::]thread::sleep(                  -> dbt_vfs::thread::sleep(
  [tokio::]task::spawn_blocking(         -> dbt_vfs::thread::spawn_blocking(
  std::thread::scope(                    -> dbt_vfs::thread::scope(
  use std::env[::..]  (also nested)      -> use dbt_vfs::env[::..]   (every std::env
                                            leaf moves: dbt_vfs::env re-exports
                                            std::env and shadows what wasm lacks)
  std::env::X                            -> dbt_vfs::env::X
  std::process::id()                     -> dbt_vfs::env::process_id()
  dirs::home_dir()                       -> dbt_vfs::env::home_dir()  (None on wasm,
                                            and dbt .expect()s it for leases)

then drops `SystemTime` / `thread` import leaves the rewrite orphaned, applies
the literal PATCHES below, and adds the dbt-vfs dependency.
"""

from __future__ import annotations

import argparse
import importlib.util
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def _load(name: str, file: str):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / file)
    mod = importlib.util.module_from_spec(spec)
    assert spec.loader
    spec.loader.exec_module(mod)
    return mod


vfs = _load("vfs_rewrite", "vfs-rewrite.py")
scan = _load("wasm_runtime_scan", "wasm-runtime-scan.py")

SKIP_CRATES = {"dbt-vfs"}
SKIP_FILES = [
    ("dbt-main/src/update.rs", "self-update replaces the real binary; native only"),
]

# One-off edits (file relative to crates/, old, new, why); idempotent as in
# vfs-rewrite.py (skipped when `new` is already present).
PATCHES: list[tuple[str, str, str, str]] = [
    (
        "dbt-main/src/main_impl.rs",
        """    #[cfg(target_arch = "wasm32")]
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()""",
        """    // No `enable_all()` on wasm: the time driver reads std's clock as it is
    // built (a panic on wasm32-unknown-unknown), and there is no IO driver to
    // enable. Timers on the run path go through dbt_vfs::time, which never
    // waits on wasm.
    #[cfg(target_arch = "wasm32")]
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .build()""",
        "wasm runtime: no time driver (it panics reading std's clock)",
    ),
    (
        "dbt-tracing/src/background_writer.rs",
        "use std::thread::{self, JoinHandle};\n",
        "// ferrion-wasm: on wasm the writer \"thread\" runs when the shutdown handle\n"
        "// joins it (dbt_vfs::thread::spawn); until then writes queue in the channel.\n"
        "use dbt_vfs::thread::{self, JoinHandle};\n",
        "log writer: dbt_vfs::thread (runs at join on wasm)",
    ),
    (
        "dbt-tracing/src/layers/parquet_writer.rs",
        "use std::thread::{self, JoinHandle};\n",
        "// ferrion-wasm: on wasm the writer \"thread\" runs when the shutdown handle\n"
        "// joins it (dbt_vfs::thread::spawn); until then records queue in the channel.\n"
        "use dbt_vfs::thread::{self, JoinHandle};\n",
        "parquet telemetry writer: dbt_vfs::thread (runs at join on wasm)",
    ),
    (
        "dbt-tui-progress/src/controller.rs",
        """    pub fn start_ticker(&mut self) {
        if self.ticker.is_some() {""",
        """    #[cfg(not(target_arch = "wasm32"))]
    pub fn start_ticker(&mut self) {
        if self.ticker.is_some() {""",
        "progress ticker thread: native only",
    ),
    (
        "dbt-tui-progress/src/controller.rs",
        """    /// Starts the background ticker thread for progress bar animations.
""",
        """    /// ferrion-wasm: no ticker thread on wasm (bars redraw on their own
    /// updates; spinners do not animate).
    #[cfg(target_arch = "wasm32")]
    pub fn start_ticker(&mut self) {}

    /// Starts the background ticker thread for progress bar animations.
""",
        "progress ticker: a no-op on wasm",
    ),
    (
        "dbt-tui-progress/src/controller.rs",
        "use std::sync::Mutex;\nuse std::time::Duration;\n",
        "use std::sync::Mutex;\n#[cfg(not(target_arch = \"wasm32\"))]\nuse std::time::Duration;\n",
        "progress ticker: its Duration is native only",
    ),
    (
        "dbt-tui-progress/src/bar.rs",
        "    /// Ticks the progress bar to update animations.\n    pub fn tick(&self) {",
        "    /// Ticks the progress bar to update animations.\n"
        "    #[cfg_attr(target_arch = \"wasm32\", allow(dead_code))] // only the ticker ticks\n"
        "    pub fn tick(&self) {",
        "progress ticker: tick() is unused on wasm",
    ),
    (
        "dbt-common/src/lease.rs",
        """fn spawn_lease_renewal(mut lease: Lease, ttl: Duration, interval: Duration) -> LeaseGuard {
    let handle = tokio::spawn(async move {""",
        """fn spawn_lease_renewal(mut lease: Lease, ttl: Duration, interval: Duration) -> LeaseGuard {
    // ferrion-wasm: one wasm instance is one process, and a renewal loop over a
    // sleep that never waits (dbt_vfs::time) would rewrite the lease file on
    // every scheduler turn. Renew once and hold it until the guard drops.
    #[cfg(target_arch = "wasm32")]
    let handle = {
        let _ = interval;
        lease.renew(ttl).ok();
        tokio::spawn(async move {
            let _held = lease;
            std::future::pending::<()>().await
        })
    };
    #[cfg(not(target_arch = "wasm32"))]
    let handle = tokio::spawn(async move {""",
        "lease: no renewal loop on wasm",
    ),
    (
        "dbt-main/src/dbt_lib.rs",
        # REGEX: cargo fmt re-indents the closure body afterwards, so the
        # anchor is a regex that no longer matches once applied.
        r"REGEX:let handle = std::thread::Builder::new\(\)\n\s*\.stack_size\(8 \* 1024 \* 1024\)\n\s*\.spawn\(move \|\| -> FsResult<\(\)> \{",
        "let worker = move || -> FsResult<()> {",
        "catalog workers: the worker is a closure first",
    ),
    (
        "dbt-main/src/dbt_lib.rs",
        r'REGEX:\}\)\n\s*\.expect\("failed to spawn worker thread"\);\n\s*handles\.push\(handle\);',
        """};
        // ferrion-wasm: no threads on wasm; each worker runs inline (the first
        // drains the whole queue, so the poll loop below sees it finished).
        #[cfg(target_arch = "wasm32")]
        let handle = {
            let _ = worker();
        };
        #[cfg(not(target_arch = "wasm32"))]
        let handle = std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(worker)
            .expect("failed to spawn worker thread");
        handles.push(handle);""",
        "catalog workers: inline on wasm, a thread natively",
    ),
]


# ---------------------------------------------------------------------------
# use trees


def parse_use_tree(s: str):
    """Parse a use tree into leaves: list of (path segments, alias|None).

    A glob is the segment '*'; `self` inside braces names the group's path."""
    pos = 0

    def ws():
        nonlocal pos
        while pos < len(s) and s[pos].isspace():
            pos += 1

    def ident():
        nonlocal pos
        ws()
        m = re.match(r"(?:r#)?[A-Za-z_][A-Za-z0-9_]*|\*", s[pos:])
        if not m:
            raise ValueError(f"bad use tree at {s[pos:pos+20]!r}")
        pos += m.end()
        return m.group(0)

    def tree(prefix):
        nonlocal pos
        ws()
        segs = list(prefix)
        if s.startswith("::", pos):  # leading `::std`
            pos += 2
            segs.append("")
        while True:
            ws()
            if s.startswith("{", pos):
                pos += 1
                out = []
                while True:
                    ws()
                    if s.startswith("}", pos):
                        pos += 1
                        break
                    out += tree(segs)
                    ws()
                    if s.startswith(",", pos):
                        pos += 1
                return out
            name = ident()
            if name == "self":
                name = None
            elif name != "*":
                segs.append(name)
            ws()
            if s.startswith("::", pos):
                pos += 2
                continue
            alias = None
            m = re.match(r"\s+as\s+([A-Za-z_][A-Za-z0-9_]*)", s[pos:])
            if m:
                alias = m.group(1)
                pos += m.end()
            if name == "*":
                return [(segs + ["*"], None)]
            return [(segs, alias)]

    leaves = tree([])
    ws()
    if pos != len(s):
        raise ValueError(f"trailing text in use tree: {s[pos:]!r}")
    return leaves


def render_leaves(leaves) -> str:
    """Render leaves (all sharing a first segment) as one nested use tree."""

    def group(items, depth):
        # items: list of (segs, alias); returns tree string for segs[depth:]
        here = [it for it in items if len(it[0]) == depth]
        rest = [it for it in items if len(it[0]) > depth]
        by: dict[str, list] = {}
        for it in rest:
            by.setdefault(it[0][depth], []).append(it)
        parts = []
        for segs, alias in here:
            parts.append("self" + (f" as {alias}" if alias else ""))
        for name, its in by.items():
            if len(its) == 1 and len(its[0][0]) == depth + 1:
                alias = its[0][1]
                parts.append(name + (f" as {alias}" if alias else ""))
            else:
                sub = group(its, depth + 1)
                parts.append(f"{name}::{sub}")
        if len(parts) == 1 and not here:
            return parts[0]
        return "{" + ", ".join(parts) + "}"

    first = leaves[0][0][0]
    lead = "::" if first == "" else ""
    if first == "":
        leaves = [(segs[1:], a) for segs, a in leaves]
    return lead + group(leaves, 0)


USE_STMT = re.compile(r"\b(pub(?:\([^)]*\))?\s+)?use\s+([^;]*?);", re.S)

STD_TIME = (["std", "time"], ["", "std", "time"])


def is_std_time_leaf(segs) -> bool:
    return any(segs[: len(p)] == p for p in STD_TIME)


def rewrite_uses(src: str, mask, skip, stats: Counter) -> str:
    out = []
    last = 0
    for m in USE_STMT.finditer(src):
        if not mask[m.start()] or skip(m.start()):
            continue
        body = m.group(2)
        vis = m.group(1) or ""
        try:
            leaves = parse_use_tree(body)
        except ValueError:
            continue
        new_stmts = None
        # tokio::time::X -> dbt_vfs::time::X (every tokio::time leaf moves)
        if any(segs[:2] == ["tokio", "time"] for segs, _ in leaves):
            moved = [(["dbt_vfs", "time"] + segs[2:], a) for segs, a in leaves if segs[:2] == ["tokio", "time"]]
            kept = [(segs, a) for segs, a in leaves if segs[:2] != ["tokio", "time"]]
            new_stmts = ([kept] if kept else []) + [moved]
            stats["use tokio::time"] += 1
        # std::time leaves move together when one of them is Instant / the module
        elif any(
            is_std_time_leaf(segs) and (segs[-1] in ("Instant", "*") or segs in STD_TIME)
            for segs, _ in leaves
        ):
            def mv(segs):
                k = 3 if segs[0] == "" else 2
                return ["dbt_vfs", "time"] + segs[k:]

            moved = [(mv(segs), a) for segs, a in leaves if is_std_time_leaf(segs)]
            kept = [(segs, a) for segs, a in leaves if not is_std_time_leaf(segs)]
            new_stmts = ([kept] if kept else []) + [moved]
            stats["use std::time"] += 1
        if new_stmts is None:
            continue
        indent = src[src.rfind("\n", 0, m.start()) + 1 : m.start()]
        text = ("\n" + indent).join(f"{vis}use {render_leaves(ls)};" for ls in new_stmts)
        out.append(src[last : m.start()])
        out.append(text)
        last = m.end()
    out.append(src[last:])
    return "".join(out)


STD_ENV = (["std", "env"], ["", "std", "env"])


def is_std_env_leaf(segs) -> bool:
    return any(segs[: len(p)] == p for p in STD_ENV)


def rewrite_env_uses(src: str, mask, skip, stats: Counter) -> str:
    """Every `std::env` leaf of a `use` moves to `dbt_vfs::env` (which
    re-exports all of std::env), the other leaves stay where they are."""
    out = []
    last = 0
    for m in USE_STMT.finditer(src):
        if not mask[m.start()] or skip(m.start()):
            continue
        try:
            leaves = parse_use_tree(m.group(2))
        except ValueError:
            continue
        if not any(is_std_env_leaf(segs) for segs, _ in leaves):
            continue

        def mv(segs):
            k = 3 if segs[0] == "" else 2
            return ["dbt_vfs", "env"] + segs[k:]

        moved = [(mv(segs), a) for segs, a in leaves if is_std_env_leaf(segs)]
        kept = [(segs, a) for segs, a in leaves if not is_std_env_leaf(segs)]
        vis = m.group(1) or ""
        indent = src[src.rfind("\n", 0, m.start()) + 1 : m.start()]
        stmts = ([kept] if kept else []) + [moved]
        text = ("\n" + indent).join(f"{vis}use {render_leaves(ls)};" for ls in stmts)
        out.append(src[last : m.start()])
        out.append(text)
        last = m.end()
        stats["use std::env"] += 1
    out.append(src[last:])
    return "".join(out)


def prune_orphans(src: str, mask, skip, stats: Counter) -> str:
    """Drop `SystemTime` / `thread` leaves no other code in the file names."""
    uses = [m for m in USE_STMT.finditer(src) if mask[m.start()]]
    in_use = [False] * len(src)
    for m in uses:
        in_use[m.start() : m.end()] = [True] * (m.end() - m.start())
    regions = vfs.test_regions(src, mask)

    def in_tests(pos: int) -> bool:
        return any(a <= pos < b for a, b in regions)

    needs: list[tuple[str, str]] = []
    out = []
    last = 0
    for m in uses:
        if skip(m.start()):
            continue
        try:
            leaves = parse_use_tree(m.group(2))
        except ValueError:
            continue
        drop = []
        for segs, alias in leaves:
            name = alias or (segs[-1] if segs else None)
            std_like = segs[:1] == ["std"] or segs[:2] == ["", "std"] or segs[:2] == ["dbt_vfs", "time"]
            if not std_like or name not in ("SystemTime", "thread") or alias:
                continue
            if name == "thread" and segs[-1] != "thread":
                continue
            hits = [
                o.start()
                for o in re.finditer(r"(?<![\w:])" + name + r"\b", src)
                if mask[o.start()] and not in_use[o.start()]
            ]
            if any(not in_tests(h) for h in hits):
                continue
            drop.append((segs, alias))
            if hits:
                # only #[cfg(test)] code still names it (through `use super::*`)
                needs.append((name, render_leaves([(segs, alias)])))
        if not drop:
            continue
        kept = [lf for lf in leaves if lf not in drop]
        stats["pruned import"] += len(drop)
        out.append(src[last : m.start()])
        if kept:
            out.append(f"{m.group(1) or ''}use {render_leaves(kept)};")
            last = m.end()
        else:
            last = m.end()
            if src.startswith("\n", last):
                last += 1
    out.append(src[last:])
    text = "".join(out)
    # give each test module that still names a dropped leaf its own import
    for name, path in needs:
        tmask = vfs.code_mask(text)
        for a, b in sorted(vfs.test_regions(text, tmask), reverse=True):
            body = text[a:b]
            if not re.search(r"(?<![\w:])" + name + r"\b", body) or f"use {path};" in body:
                continue
            brace = text.index("{", a)
            text = text[: brace + 1] + f"\n    use {path};" + text[brace + 1 :]
            stats["test import"] += 1
    return text


CALL_SUBS = [
    ("SystemTime::now", re.compile(r"(?<![\w:])(?:(?:::)?std::time::|time::)?SystemTime::now\(\)"), "dbt_vfs::time::system_now()"),
    ("Instant path", re.compile(r"(?<![\w:])(?:::)?std::time::Instant\b"), "dbt_vfs::time::Instant"),
    ("tokio::time", re.compile(r"(?<![\w:])tokio::time::(?=(?:sleep|timeout|interval|MissedTickBehavior|Duration|error)\b)"), "dbt_vfs::time::"),
    ("thread::sleep", re.compile(r"(?<![\w:])(?:(?:::)?std::)?thread::sleep\("), "dbt_vfs::thread::sleep("),
    ("spawn_blocking", re.compile(r"(?<![\w:])(?:tokio::)?task::spawn_blocking\("), "dbt_vfs::thread::spawn_blocking("),
    ("thread::scope", re.compile(r"(?<![\w:])(?:::)?std::thread::scope\("), "dbt_vfs::thread::scope("),
    ("std::env", re.compile(r"(?<![\w:])(?:::)?std::env::(?=[A-Za-z_])"), "dbt_vfs::env::"),
    ("process::id", re.compile(r"(?<![\w:])(?:::)?std::process::id\(\)"), "dbt_vfs::env::process_id()"),
    ("dirs::home_dir", re.compile(r"(?<![\w:])(?:::)?dirs::home_dir\(\)"), "dbt_vfs::env::home_dir()"),
]


def rewrite_file(path: Path, rel: str, stats: Counter) -> bool:
    src = vfs.read(path)
    mask = vfs.code_mask(src)
    regions = vfs.test_regions(src, mask)
    gated = scan.gated_spans(src, mask)

    def skip_at(src_, regions_, gated_):
        return lambda pos: any(a <= pos < b for a, b in regions_) or any(a <= pos < b for a, b in gated_)

    text = rewrite_uses(src, mask, skip_at(src, regions, gated), stats)
    mask = vfs.code_mask(text)
    text = rewrite_env_uses(
        text, mask, skip_at(text, vfs.test_regions(text, mask), scan.gated_spans(text, mask)), stats
    )
    # recompute spans on the new text
    mask = vfs.code_mask(text)
    skip = skip_at(text, vfs.test_regions(text, mask), scan.gated_spans(text, mask))
    for key, pat, repl in CALL_SUBS:
        def sub(m, key=key, repl=repl):
            if not mask[m.start()] or skip(m.start()):
                return m.group(0)
            stats[key] += 1
            return repl
        text = pat.sub(sub, text)
        mask = vfs.code_mask(text)
        skip = skip_at(text, vfs.test_regions(text, mask), scan.gated_spans(text, mask))
    if text != src:
        text = prune_orphans(text, mask, skip, stats)
    if text == src:
        return False
    vfs.FILES[path] = text
    return True


def crates() -> list[tuple[str, Path]]:
    return [(n, d) for n, d in scan.closure() if n not in SKIP_CRATES]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="exit 1 if anything would change")
    ap.add_argument("--stats", action="store_true", help="print per-crate counts")
    args = ap.parse_args()

    per_crate: dict[str, Counter] = defaultdict(Counter)
    changed: list[str] = []
    crates_root = ROOT / "crates"
    for crate, cdir in crates():
        src_dir = cdir / "src"
        if not src_dir.is_dir():
            continue
        touched = False
        tests = vfs.test_module_files(src_dir)
        for path in sorted(src_dir.rglob("*.rs")):
            relp = path.relative_to(src_dir)
            if vfs.is_test_code(path, tests):
                continue
            rel = path.relative_to(crates_root).as_posix()
            if any(rel == f for f, _why in SKIP_FILES):
                continue
            if rewrite_file(path, rel, per_crate[crate]):
                touched = True
                changed.append(rel)
        crate_dir = cdir.relative_to(crates_root).as_posix()
        if touched or any(p[0].startswith(crate_dir + "/") for p in PATCHES):
            if add_dep(cdir):
                changed.append(f"{crate_dir}/Cargo.toml")
    patch_stats: Counter = Counter()
    for rel, old, new, _why in PATCHES:
        if vfs.apply_patch(rel, old, new, patch_stats):
            changed.append(f"{rel} (patch)")

    if args.stats:
        total: Counter = Counter()
        for crate, c in sorted(per_crate.items()):
            if c:
                total.update(c)
                print(f"{crate:22} " + " ".join(f"{k}={v}" for k, v in sorted(c.items())))
        print(f"{'TOTAL':22} " + " ".join(f"{k}={v}" for k, v in sorted(total.items())))
        print(f"patches applied: {patch_stats['patches']}")
    if args.check:
        for c in changed:
            print("pending:", c)
        return 1 if changed else 0
    for path, text in vfs.FILES.items():
        if path.read_text() != text:
            path.write_text(text)
    print(f"wasm-runtime-rewrite: {len(changed)} change(s)")
    return 0


def add_dep(cdir: Path) -> bool:
    toml = cdir / "Cargo.toml"
    text = vfs.read(toml)
    if re.search(r"^dbt-vfs\s*=", text, re.M):
        return False
    m = re.search(r"^\[dependencies\]\n", text, re.M)
    if not m:
        sys.exit(f"wasm-runtime-rewrite: no [dependencies] in {toml}")
    vfs.FILES[toml] = text[: m.end()] + "dbt-vfs = { workspace = true }\n" + text[m.end() :]
    return True


if __name__ == "__main__":
    sys.exit(main())
