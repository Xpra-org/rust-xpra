# rust-xpra dependency graph

### [Open the interactive graph →](dependency-graph.html)

An interactive map of every crate the [rust-xpra](https://github.com/Xpra-org/rust-xpra) client pulls
in, for both of its targets. Keeping that set small is a deliberate constraint on the project — the
WebSocket layer and the SHA1/HMAC-SHA256 implementations are hand-rolled against `std` rather than
taken from crates, and `ssh` shells out to the system binary instead of linking a client — so this
page exists to make the real cost of each dependency visible rather than assumed.

It is a single self-contained file: no external stylesheets, scripts or fonts, so it renders the same
from this site, from a local checkout, or offline.

## What it shows

Three views over the same data, switched at the top of the page, plus a rail down the left.

**Graph** — a radial map. Rings are hops from `Cargo.toml`, so the crates you declare yourself sit on
the innermost ring and everything else falls outward. Each direct dependency owns an angular wedge,
sized by the number of crates *only it* reaches and named on the rim; a crate that several
dependencies share sits between their wedges. Colour carries the distinction that matters most:

| | |
|---|---|
| **orange** | linked into the binary |
| **grey** | build-time only — a build script or a proc-macro, which never ships |
| **hollow** | declared by us in `Cargo.toml` |
| **teal halo** | present in the graph at more than one version |

Drag to pan, scroll to zoom, click any crate for its license, description, who pulls it in and what
it pulls in itself.

**Tree** — the shape `cargo tree` prints, including its `(*)` marker for a subtree already expanded
above, annotated with the same build / proc-macro / duplicate-version distinctions.

**Table** — sortable: crate, version, whether it ships, hops from `Cargo.toml`, which direct
dependencies reach it, how many crates depend on it, and its license.

**The left rail** is the cost chart. Each direct dependency gets a bar whose length is its full
transitive closure and whose solid part is the crates it is the *only* route to — what dropping it
would actually remove, as opposed to what it merely also uses. Clicking one narrows every view to its
subtree.

## Regenerating

After any change to `Cargo.toml`'s dependencies:

```shell
python3 docs/dependency-graph.py
```

That rewrites `dependency-graph.html` from `dependency-graph.template.html`, inlining a freshly read
graph — so edit the template, never the generated file. It needs nothing but python 3 and a working
`cargo`: no third-party python packages, and no toolchain installed for the Windows target, since
`cargo tree --target` only has to *resolve* the graph, not build it. Output is byte-stable for a given
resolution, so re-running it with nothing changed produces no diff.

The counts quoted in the project's [main README](https://github.com/Xpra-org/rust-xpra#dependencies)
are the one thing the script cannot update; check them when the numbers move.

## How the data is derived

From `cargo tree`, not from `cargo metadata`: metadata's `resolve` graph is not feature-resolved, so
it reports optional dependencies that nothing actually enables (`indexmap` and `toml_writer` under
`toml`, for two). `cargo metadata` is still called once, unfiltered, for the per-package
license/description/repository text, which does not vary by target.

Six `cargo tree` runs feed the graph — for each target:

1. the full `normal,build` edge set, with deduplication off so no subtree is elided;
2. the same minus build edges, since subtracting the two sets is how an edge is known to be a build
   edge;
3. a `no-proc-macro` pass, which is exactly the set of crates that survive into the binary.

Dev-dependencies are excluded, and there are none to exclude. Adding a target means adding it to
`TARGETS` in the script *and* adding a matching `<button data-target=…>` to the template's target
switch.

One caveat on reproducibility: `Cargo.lock` is not committed (see the packaging notes in
`CLAUDE.md`), so cargo re-resolves against live crates.io on every run. Regenerating on a different
day can legitimately shift versions and counts without anything having changed in `Cargo.toml`.

## Publishing

This folder is the source for <https://xpra-org.github.io/rust-xpra/>, configured under **Settings →
Pages → Build and deployment → Source: "Deploy from a branch"**, branch `main`, folder `/docs`. Every
push to `main` republishes it; `_config.yml` holds the site settings.
