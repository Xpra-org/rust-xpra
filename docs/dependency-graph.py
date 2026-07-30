#!/usr/bin/env python3
"""Regenerate `docs/dependency-graph.html`, the interactive dependency graph.

    python3 docs/dependency-graph.py

Reads the crate graph straight out of `cargo tree` and inlines it into
`dependency-graph.template.html`, producing one self-contained page (no network
access at view time, no external assets).

`cargo tree` rather than `cargo metadata` is the source of truth for the edges on
purpose: metadata's `resolve` graph is *not* feature-resolved, so it reports
optional dependencies that no enabled feature actually turns on (`indexmap` and
`toml_writer` under `toml`, for instance). `cargo metadata` is still used, once
and unfiltered, for the per-package prose - license, description, repository -
which is the same whatever the target.

Adding a target: put it in TARGETS. The keys become the page's target switch, so
the template's `<button data-target=...>` list has to match.
"""

import collections
import datetime
import json
import pathlib
import re
import subprocess
import sys

DOCS = pathlib.Path(__file__).resolve().parent
ROOT = DOCS.parent

TARGETS = {
    "linux": "x86_64-unknown-linux-gnu",
    "windows": "x86_64-pc-windows-msvc",
}

# `cargo tree --prefix depth` emits "<depth><name> v<version>[ (proc-macro)][ (*)]"
LINE = re.compile(r"^(\d+)(\S.*?)(?: \(proc-macro\))?(?: \(\*\))?$")


def cargo(*args):
    out = subprocess.run(("cargo",) + args, cwd=ROOT, capture_output=True, text=True)
    if out.returncode:
        sys.exit(f"cargo {' '.join(args)} failed:\n{out.stderr}")
    return out.stdout


def tree(target, edges, dedupe=True):
    """One `cargo tree` run, as (depth, name, version, is_proc_macro) rows."""
    args = ["tree", "--target", target, "--edges", edges,
            "--prefix", "depth", "--format", "{p}"]
    if not dedupe:
        args.append("--no-dedupe")

    rows = []
    for line in cargo(*args).splitlines():
        m = LINE.match(line)
        if not m:
            sys.exit(f"unparsed cargo tree line: {line!r}")
        depth, pkg = int(m.group(1)), m.group(2)
        # "{p}" is "name vX.Y.Z", plus " (/path)" for a workspace member
        name, _, rest = pkg.partition(" v")
        rows.append((depth, name, rest.split(" ")[0], line.endswith("(proc-macro)")))
    return rows


def edges_of(rows):
    """Parent -> child edges, recovered from the depth column."""
    edges, stack = set(), {}
    for depth, name, version, _ in rows:
        stack[depth] = (name, version)
        if depth:
            edges.add((stack[depth - 1], (name, version)))
    return edges


def build(target, prose):
    # Three passes, because each answers a different question:
    #   normal+build  every edge, deduped off so no subtree is elided
    #   normal        the same minus build-dependency edges, so subtracting the
    #                 two sets tells us which edges are build edges
    #   no-proc-macro what is left after pruning proc-macro crates, i.e. exactly
    #                 the crates that end up linked into the binary
    full = tree(target, "normal,build", dedupe=False)
    normal_edges = edges_of(tree(target, "normal", dedupe=False))
    linked = {(n, v) for _, n, v, _ in tree(target, "normal,no-proc-macro")}

    all_edges = edges_of(full)
    root = (full[0][1], full[0][2])
    procmacro = {(n, v) for _, n, v, pm in full if pm}
    nodes = {(n, v) for _, n, v, _ in full}

    out = collections.defaultdict(set)
    for a, b in all_edges:
        out[a].add(b)

    direct = sorted(out[root])

    # every crate each direct dependency can reach, itself included
    closure = {}
    for d in direct:
        seen, stack = {d}, [d]
        while stack:
            for nxt in out[stack.pop()]:
                if nxt not in seen:
                    seen.add(nxt)
                    stack.append(nxt)
        closure[d] = seen

    # ...inverted: which direct dependencies reach this crate. One owner means
    # dropping that dependency would drop this crate with it.
    owners = collections.defaultdict(set)
    for d, reached in closure.items():
        for c in reached:
            owners[c].add(d)

    key = lambda nv: f"{nv[0]} {nv[1]}"

    dupes = collections.defaultdict(list)
    for name, version in nodes:
        dupes[name].append(version)

    return {
        "root": key(root),
        "direct": [key(d) for d in direct],
        "nodes": {
            key(nv): dict(
                n=nv[0], v=nv[1],
                pm=nv in procmacro,
                linked=nv in linked,
                direct=nv in set(direct),
                owners=sorted(key(o) for o in owners.get(nv, ())),
                **prose.get(nv, {"lic": "", "desc": "", "repo": ""}),
            )
            for nv in sorted(nodes)
        },
        "edges": [
            {"a": key(a), "b": key(b), "k": "normal" if (a, b) in normal_edges else "build"}
            for a, b in sorted(all_edges, key=lambda e: (e[0], e[1]))
        ],
        "dupes": {n: sorted(v) for n, v in sorted(dupes.items()) if len(v) > 1},
        "stats": {
            "total": len(nodes) - 1,
            "linked": len(linked) - 1,
            "buildonly": len(nodes) - len(linked),
        },
    }


def main():
    # unfiltered: one call covers every platform's packages
    meta = json.loads(cargo("metadata", "--format-version", "1"))
    prose = {
        (p["name"], p["version"]): {
            "lic": p.get("license") or "",
            "desc": (p.get("description") or "").strip().replace("\n", " ")[:200],
            "repo": p.get("repository") or "",
        }
        for p in meta["packages"]
    }
    version = next(p["version"] for p in meta["packages"]
                   if p["id"] == meta["resolve"]["root"])

    data = {label: build(target, prose) for label, target in TARGETS.items()}
    blob = json.dumps(data, separators=(",", ":"), sort_keys=True)
    if "</script" in blob:
        sys.exit("refusing to inline data that would close the <script> element")

    stamp = f"{version} on {datetime.date.today():%-d %b %Y}"
    page = (DOCS / "dependency-graph.template.html").read_text()
    for placeholder, value in (("__DATA__", blob), ("__STAMP__", stamp)):
        if placeholder not in page:
            sys.exit(f"template is missing {placeholder}")
        page = page.replace(placeholder, value, 1)

    target_file = DOCS / "dependency-graph.html"
    target_file.write_text(page)

    print(f"wrote {target_file.relative_to(ROOT)} ({len(page) // 1024} KiB)")
    for label, g in data.items():
        s = g["stats"]
        print(f"  {label:8s} {s['total']:4d} crates  "
              f"{s['linked']:4d} linked  {s['buildonly']:3d} build-time only  "
              f"{len(g['direct']):3d} direct")
        for name, versions in g["dupes"].items():
            print(f"             duplicate: {name} " + " ".join("v" + v for v in versions))


if __name__ == "__main__":
    main()
