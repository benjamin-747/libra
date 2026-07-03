# libra deps

`libra deps` manages the **file dependency graph** (lore.md 3.1): typed,
versioned per-file dependency edges. A Libra extension — Git has no
file-dependency concept.

## Compatibility

- Level: `intentionally-different`.

## Design

An edge `(from -> to, kind)` declares that one file depends on another. Edges
are VERSIONED per-commit: the authoritative store is one adjacency document
per commit under the reserved notes ref `refs/notes/deps`, owned solely by
`internal::deps::DependencyStore` (mirroring the `refs/notes/metadata`
pattern — no new SQLite table, honoring the §3.6 "no per-kind table" rule).
Every query loads the revision's (size-bounded) document and computes in
memory, so there is no projection cache to fall out of sync.

Queries are cycle-safe (iterative BFS with a visited set — deep/wide graphs
never overflow the stack) and absence-tolerant (a missing note → an empty
graph). Paths are repo-relative and normalized (`./` stripped, `\`→`/`,
trailing `/` collapsed); absolute paths, `..` escapes, and empty strings are
rejected.

The `transitive_closure` API is the reusable seam that 3.2 (dependency-filtered
clone/sync) and 3.3 (hydrating VFS) call to expand a root file set.

## Wire travel (deferred to 3.2)

The deps note is durable in the object graph, but — like all `refs/notes/*` —
Libra does not auto-fetch/push it. Moving edges to another machine (wiring
`refs/notes/deps` into fetch/push) is a deliverable of lore.md 3.2, not this
item. A fresh clone reads an empty graph until the deps ref is fetched.

## Examples

```bash
libra deps add scene.usd tex/wood.png     # declare a dependency
libra deps list scene.usd                 # direct deps
libra deps list tex/wood.png --reverse    # dependents
libra deps tree scene.usd                 # transitive closure
libra deps tree scene.usd --depth-limit 2 # bounded closure
libra deps why scene.usd tex/wood.png     # shortest dependency path
libra deps rm scene.usd tex/wood.png      # remove an edge
libra deps add a b --revision <commit>    # target a specific commit
```

## Deferred (not v1)

Cross-machine edge travel (3.2), carry-forward of edges onto new commits,
rename-following (path-keyed edges do not auto-migrate), and automatic
dependency inference (v1 edges are author-declared).
