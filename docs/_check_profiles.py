#!/usr/bin/env python3
"""Self-check for reference-architecture.md §4: flags → workers → live MCP tools must agree."""
FACT_TOOLS = {"memory.write", "memory.get", "memory.traverse"}      # always live (keyed + graph)
SEMANTIC   = {"memory.search"}                                       # fact-tier semantic
DIGEST     = {"memory.recall"}                                       # digest tier only

def profile(semantic: bool, digest: bool):
    assert not (digest and not semantic), "digest sub-flag requires memory.semantic on"
    workers = {"5a"}
    tools = set(FACT_TOOLS)
    if semantic:
        workers |= {"5b", "5d"}          # fact embed + categorize
        tools  |= SEMANTIC
    if digest:
        workers |= {"5c"}                # digest rollup/summarize
        tools  |= DIGEST
    disabled = (SEMANTIC | DIGEST) - tools   # must return 501, not silent empty
    return workers, tools, disabled

# minimal: keyed+graph only
w, t, d = profile(False, False)
assert w == {"5a"},                               w
assert t == FACT_TOOLS,                           t
assert d == SEMANTIC | DIGEST,                    d      # search+recall both 501

# full fact-tier: semantic on, digest off
w, t, d = profile(True, False)
assert w == {"5a", "5b", "5d"},                   w
assert t == FACT_TOOLS | SEMANTIC,                t
assert d == DIGEST,                               d      # recall still 501

# full + digest
w, t, d = profile(True, True)
assert w == {"5a", "5b", "5c", "5d"},             w
assert t == FACT_TOOLS | SEMANTIC | DIGEST,       t
assert d == set(),                                d      # nothing disabled

# invalid combo caught
try:
    profile(False, True); raise SystemExit("FAIL: digest-without-semantic not rejected")
except AssertionError:
    pass

print("OK: all three profiles + flag gating consistent with §4 table")
