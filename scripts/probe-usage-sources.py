#!/usr/bin/env python3
"""Read-only probe of the local stores that coding-agent CLIs leave on disk.

Purpose: Settings > Usage (issue #53) needs to know, per agent, where usage
data can be read locally and what shape it has. This script only INSPECTS:

  * it never writes outside the report file you choose;
  * it never prints a value of a key that looks like a credential
    (token/secret/key/auth/cookie/password/bearer/credential/session);
  * it never prints message text, prompts, or file contents - only key
    names, JSON types, counts, sizes, mtimes, and numeric ranges for
    usage-like counters;
  * maps keyed by ids/paths/projects (more than 25 keys, or keys that look
    like paths) are reported as "{*}" instead of listing their keys;
  * home-directory prefixes are replaced by "~", uuid-like path parts by
    "<uuid>", long digit runs by "<n>".

Usage:
    python3 scripts/probe-usage-sources.py                 # all agents
    python3 scripts/probe-usage-sources.py muse pi grok    # a subset
    python3 scripts/probe-usage-sources.py --out report.json

Then paste the printed summary (or report.json) back into the chat.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
from collections import Counter, defaultdict
from pathlib import Path

AGENTS = [
    "claude", "codex", "cursor", "antigravity", "copilot", "qoder",
    "muse", "pi", "opencode", "grok", "devin", "cline", "hermes",
]

# Extra directory names an agent is known to use besides ~/.<name>.
ALIASES = {
    "claude": ["claude"],
    "codex": ["codex"],
    "cursor": ["cursor"],
    "antigravity": ["antigravity", "antigravity-cli", "gemini"],
    "copilot": ["copilot", "github-copilot"],
    "qoder": ["qoder"],
    "muse": ["muse"],
    "pi": ["pi", "pi-agent"],
    "opencode": ["opencode"],
    "grok": ["grok", "grok-cli"],
    "devin": ["devin", "devin-cli"],
    "cline": ["cline"],
    "hermes": ["hermes", "hermes-agent"],
}

# "token" alone is a credential ("access_token"); "tokens" is a counter
# ("input_tokens", "total_tokens") and must stay readable.
SECRET_KEY = re.compile(
    r"(token(?!s|_count|_usage|_limit)|secret|api[_-]?key|auth|cookie|password|passwd|bearer|"
    r"credential|session[_-]?id|jwt|refresh)",
    re.I,
)
USAGE_KEY = re.compile(
    r"(token|usage|quota|limit|credit|cost|price|input|output|cache|cached|reason|total|model|"
    r"remaining|used|reset|window|plan|tier)",
    re.I,
)
IDENTITY_KEY = re.compile(r"(account|email|user(name|_?id)?|org(anization)?|workspace|team|tenant|login)", re.I)
UUID = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
LONG_DIGITS = re.compile(r"\d{6,}")

MAX_WALK_ENTRIES = 4000
MAX_DEPTH = 6
MAX_SAMPLE_FILES = 3
MAX_SAMPLE_BYTES = 8 * 1024 * 1024
MAX_LINES = 400
BIG_MAP = 25
JSON_EXT = {".json", ".jsonl", ".ndjson"}


def home() -> Path:
    return Path(os.environ.get("HOME") or os.path.expanduser("~"))


def scrub(path: Path) -> str:
    s = str(path)
    h = str(home())
    if s.startswith(h):
        s = "~" + s[len(h):]
    s = UUID.sub("<uuid>", s)
    return LONG_DIGITS.sub("<n>", s)


def candidate_roots(agent: str) -> list[Path]:
    h = home()
    xdg_config = Path(os.environ.get("XDG_CONFIG_HOME") or h / ".config")
    xdg_data = Path(os.environ.get("XDG_DATA_HOME") or h / ".local" / "share")
    xdg_cache = Path(os.environ.get("XDG_CACHE_HOME") or h / ".cache")
    xdg_state = Path(os.environ.get("XDG_STATE_HOME") or h / ".local" / "state")
    out: list[Path] = []
    for name in ALIASES.get(agent, [agent]):
        for p in (
            h / f".{name}",
            xdg_config / name,
            xdg_data / name,
            xdg_cache / name,
            xdg_state / name,
        ):
            if p.exists() and p not in out:
                out.append(p)
    return out


def walk(root: Path):
    """Bounded walk. Yields (path, is_dir, size, mtime)."""
    count = 0
    stack = [(root, 0)]
    while stack:
        cur, depth = stack.pop()
        try:
            entries = list(os.scandir(cur))
        except OSError:
            continue
        for e in entries:
            count += 1
            if count > MAX_WALK_ENTRIES:
                return
            try:
                st = e.stat(follow_symlinks=False)
            except OSError:
                continue
            is_dir = e.is_dir(follow_symlinks=False)
            yield Path(e.path), is_dir, st.st_size, st.st_mtime
            if is_dir and depth + 1 < MAX_DEPTH:
                stack.append((Path(e.path), depth + 1))


def safe_key(k) -> str:
    k = str(k)
    if "/" in k or "\\" in k or len(k) > 48:
        return "<path-or-long-key>"
    return k


def dict_items(value: dict):
    """Items to descend into; a big map keeps its shape, not its keys."""
    items = list(value.items())
    if len(items) > BIG_MAP:
        return [("{*}", v) for _, v in items[:3]]
    return [(safe_key(k), v) for k, v in items]


def shape_of(value, prefix="", out=None, depth=0):
    """Collect key paths -> set of JSON type names. Values are never kept."""
    if out is None:
        out = defaultdict(set)
    if depth > 6:
        return out
    if isinstance(value, dict):
        for k, v in dict_items(value):
            p = f"{prefix}.{k}" if prefix else k
            shape_of(v, p, out, depth + 1)
    elif isinstance(value, list):
        out[prefix + "[]"].add("array")
        for v in value[:3]:
            shape_of(v, prefix + "[]", out, depth + 1)
    else:
        out[prefix].add(type(value).__name__)
    return out


def flat_ints(value, prefix="", out=None, depth=0):
    """Numeric leaves under usage-like, non-secret keys -> list of ints."""
    if out is None:
        out = defaultdict(list)
    if depth > 6:
        return out
    if isinstance(value, dict):
        for k, v in dict_items(value):
            if SECRET_KEY.search(k):
                continue
            p = f"{prefix}.{k}" if prefix else k
            flat_ints(v, p, out, depth + 1)
    elif isinstance(value, list):
        for v in value[:3]:
            flat_ints(v, prefix + "[]", out, depth + 1)
    elif isinstance(value, (int, float)) and not isinstance(value, bool):
        last = prefix.rsplit(".", 1)[-1]
        if USAGE_KEY.search(last):
            out[prefix].append(value)
    return out


def leaf(key_path: str) -> str:
    return key_path.rsplit(".", 1)[-1].replace("[]", "")


def read_records(path: Path):
    try:
        if path.stat().st_size > MAX_SAMPLE_BYTES:
            return [], "skipped: larger than sample cap"
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError as e:
        return [], f"unreadable: {type(e).__name__}"
    recs = []
    if path.suffix == ".json":
        try:
            recs = [json.loads(text)]
        except json.JSONDecodeError:
            return [], "invalid json"
    else:
        for i, line in enumerate(text.splitlines()):
            if i >= MAX_LINES:
                break
            line = line.strip()
            if not line:
                continue
            try:
                recs.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    return recs, None


def analyse_file(path: Path) -> dict:
    recs, note = read_records(path)
    info: dict = {"file": scrub(path), "records_sampled": len(recs)}
    if note:
        info["note"] = note
    if not recs:
        return info
    shapes: dict[str, set] = defaultdict(set)
    ints: dict[str, list] = defaultdict(list)
    identity_keys: set[str] = set()
    secret_keys_present: set[str] = set()
    for r in recs:
        for k, t in shape_of(r).items():
            shapes[k] |= t
            lf = leaf(k)
            if SECRET_KEY.search(lf):
                secret_keys_present.add(k)
            elif IDENTITY_KEY.search(lf):
                identity_keys.add(k)
        for k, vals in flat_ints(r).items():
            ints[k].extend(vals)
    info["keys"] = {
        k: sorted(t) for k, t in sorted(shapes.items())
        if not SECRET_KEY.search(leaf(k))
    }
    info["credential_like_keys_present_not_read"] = sorted(secret_keys_present)
    info["identity_like_keys"] = sorted(identity_keys)
    info["numeric_usage_keys"] = {
        k: {"n": len(v), "min": min(v), "max": max(v)} for k, v in sorted(ints.items())
    }
    info["token_semantics"] = token_semantics(recs)
    return info


def token_semantics(recs) -> dict:
    """Check how token counters relate, on the user's own logs."""
    rows = []
    for r in recs:
        flat = flat_ints(r)
        by_leaf: dict[str, list] = defaultdict(list)
        for k, v in flat.items():
            by_leaf[leaf(k).lower()].extend(v)

        def pick(*names):
            for n in names:
                if n in by_leaf and by_leaf[n]:
                    return by_leaf[n][0]
            return None

        rows.append({
            "input": pick("input_tokens", "inputtokens", "prompt_tokens", "input"),
            "output": pick("output_tokens", "outputtokens", "completion_tokens", "output"),
            "cached": pick("cached_tokens", "cache_read_tokens", "cached_input_tokens", "cache_read_input_tokens", "cachedtokens"),
            "reasoning": pick("reasoning_tokens", "reasoningtokens", "reasoning_output_tokens"),
            "total": pick("total_tokens", "totaltokens", "total"),
        })
    rows = [r for r in rows if r["input"] is not None and r["output"] is not None]
    if not rows:
        return {"records_with_input_and_output": 0}

    def frac(pred, subset):
        s = [r for r in subset if pred(r)]
        return f"{len(s)}/{len(subset)}"

    with_cached = [r for r in rows if r["cached"] is not None]
    with_reason = [r for r in rows if r["reasoning"] is not None]
    with_total = [r for r in rows if r["total"] is not None]
    out = {"records_with_input_and_output": len(rows)}
    if with_cached:
        out["cached_le_input"] = frac(lambda r: r["cached"] <= r["input"], with_cached)
    if with_reason:
        out["reasoning_le_output"] = frac(lambda r: r["reasoning"] <= r["output"], with_reason)
    if with_total:
        out["total_eq_input_plus_output"] = frac(lambda r: r["total"] == r["input"] + r["output"], with_total)
        out["total_eq_input_plus_output_plus_cached"] = frac(
            lambda r: r["cached"] is not None and r["total"] == r["input"] + r["output"] + r["cached"], with_total)
    return out


def probe_agent(agent: str) -> dict:
    roots = candidate_roots(agent)
    rep: dict = {"agent": agent, "roots_found": [scrub(r) for r in roots]}
    if not roots:
        rep["status"] = "no local store found in the usual places"
        return rep
    files = []
    ext_count: Counter = Counter()
    total_size = 0
    newest = 0.0
    dir_shapes: Counter = Counter()
    n = 0
    for root in roots:
        for p, is_dir, size, mtime in walk(root):
            n += 1
            if is_dir:
                rel = scrub(p.relative_to(root)) if p != root else "."
                dir_shapes[scrub(Path(root.name) / rel)] += 1
                continue
            ext_count[p.suffix or "(none)"] += 1
            total_size += size
            newest = max(newest, mtime)
            if p.suffix in JSON_EXT:
                files.append((mtime, p, size))
    rep["file_extensions"] = dict(ext_count.most_common(12))
    rep["total_size_kb"] = round(total_size / 1024)
    rep["newest_file_age_days"] = round((time.time() - newest) / 86400, 1) if newest else None
    rep["walk_truncated"] = n >= MAX_WALK_ENTRIES
    rep["top_directory_shapes"] = dict(dir_shapes.most_common(12))

    # Prefer session/log-like files over config/credential-like ones.
    def interesting(p: Path) -> int:
        name = scrub(p).lower()
        if SECRET_KEY.search(p.name):
            return 2
        if any(w in name for w in ("session", "history", "log", "usage", "transcript", "rollout", "project")):
            return 0
        return 1

    files.sort(key=lambda t: (interesting(t[1]), -t[0]))
    samples = []
    for mtime, p, size in files:
        if SECRET_KEY.search(p.name):
            samples.append({"file": scrub(p), "note": "credential-like file name: listed, never opened"})
        else:
            samples.append(analyse_file(p))
        if len(samples) >= MAX_SAMPLE_FILES:
            break
    rep["samples"] = samples
    return rep


def summarize(rep: dict) -> str:
    lines = [f"== {rep['agent']} =="]
    if "status" in rep:
        lines.append(f"  {rep['status']}")
        return "\n".join(lines)
    lines.append(f"  roots: {', '.join(rep['roots_found'])}")
    lines.append(f"  files: {rep['file_extensions']}  size: {rep['total_size_kb']} KB  newest: {rep['newest_file_age_days']} d ago")
    for s in rep.get("samples", []):
        lines.append(f"  - {s['file']}: {s.get('records_sampled', 0)} records {s.get('note', '')}")
        sem = s.get("token_semantics")
        if sem and sem.get("records_with_input_and_output"):
            lines.append(f"      token semantics: {sem}")
        if s.get("identity_like_keys"):
            lines.append(f"      identity-like keys: {s['identity_like_keys'][:6]}")
    return "\n".join(lines)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("agents", nargs="*", help=f"subset of: {', '.join(AGENTS)}")
    ap.add_argument("--out", default="usage-probe-report.json")
    args = ap.parse_args()
    chosen = args.agents or AGENTS
    unknown = [a for a in chosen if a not in AGENTS]
    if unknown:
        print(f"unknown agent(s): {unknown}", file=sys.stderr)
        return 2
    report = {"probe_version": 1, "generated_at": int(time.time()), "agents": [probe_agent(a) for a in chosen]}
    Path(args.out).write_text(json.dumps(report, indent=2, ensure_ascii=False), encoding="utf-8")
    for rep in report["agents"]:
        print(summarize(rep))
    print(f"\nfull report: {args.out} (no secrets, no message text)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
