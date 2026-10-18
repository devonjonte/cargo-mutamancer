# Design

## The promise

Install it in CI and in your git hooks, and it stops you every time a
commit weakens your tests, and tells you why; quickly, and without asking
you to learn how it works.

Precisely: for each commit, every mutant of the commit's own code that no
test catches, and every mutant elsewhere that a test caught before and no
test catches now, is reported, each beside its cause. The mutants are
cargo-mutants' (which models many kinds of bug, never all); the tests are
those the user promises are deterministic.

## Three rules

- **Causal.** Each fact is read where it arises (the two trees, git's
  diff, the compiled code, each test's run) and kept with its cause. No
  heuristic recovers later what could have been read at the source.
- **Exact.** Every decision to reuse a result is proved: the result
  cannot differ. Where the tool cannot prove it, it does the work, and
  says why; there is no guessing either way.
- **Fast.** The unit is one commit, checked by a hook as it is made. Each
  second matters, so the work done is the work the commit makes necessary,
  on every core.

## What a check starts from

- **The parent commit and the new tree**: every file before and after,
  with git's object ids, which prove a file unchanged without reading it.
- **Git's diff of the two trees**: renames and copies (a moved file keeps
  its identity and its map entries), the exact correspondence of every
  unchanged line (for reporting causes), modes, binary files. Never git's
  guesses (the function names in hunk headers): the tool parses the
  source itself.
- **The parent's map**, bound to the parent (below).
- **For a merge, every parent.** A merge is checked against each, since
  two branches each sound can merge into a loss.

## The maps

For each test (in its own process): the functions it ran, and in them
which code regions and branch sides; the files it read, each with its
BLAKE3 hash; and the mutants it catches. For each function: its token
fingerprint, its compiled fingerprint, and its mutants. Reach is stored as
yes or no, never as a count: counts vary with timing when reach does not.

A map is a deterministic function of the source tree and the context (the
toolchain, `Cargo.lock`, flags, target, and the context the crate declares,
such as its CPU features). It records the commit id, the tree id, and the
context it was made from, and a BLAKE3 hash of its content; it is stored
in git, keyed by the commit id, so git's content addressing binds it to
its commit. A map whose tree is not the parent's, or whose context is not
the current one, is refused, never used. Anyone can verify a map by
computing it again: the self-audit (below).

## Deciding what to redo: exactly

- **Token fingerprints** (BLAKE3, derive-key mode, context
  `"cargo-causal-mutation 2026-10-18 Rust fn token fingerprint v1"`, over
  each function's tokens with doc comments left out) decide the instant
  tier: a commit that changes only tests, formatting, or comments compiles
  nothing else differently and needs no build.
- **Compiled fingerprints** decide the rest. Each function is fingerprinted
  by its compiled code, its references to other symbols written by name.
  Whatever changes behavior changes that code: a constant it uses, a
  type's layout, a macro's expansion, a `cfg`, an inlined callee, a
  dependency, the toolchain; assembly and C by their object code.
- **The rule.** A deterministic test is run again if and only if it is
  itself a changed function, or it ran a function whose compiled code
  changed, or it read a file whose contents changed. Otherwise its run is
  the same as before: the same code on the same data, deterministically.
  A mutant is judged again if and only if a test that reaches it is run
  again, or it lies in changed code.

## Changed functions: an explicit list, never an inference

In a function the commit changes, every mutant must be caught by a test or
named in the crate's list of explained survivors (with its reason). A
survivor already there becomes a list entry, which the commit's author sees
and confirms; nothing is matched or inferred between a function's versions.

## Determinism: the user's promise

The tests the tool uses are those the user promises are deterministic: all
but those listed in `[package.metadata.causal-mutation] exclude`. The tool
trusts the promise; `look-for-nondeterminism` looks for breaks in it,
measuring tests again with nothing they depend on changed. It can find
nondeterminism, never prove its absence.

- **Data.** Each test's reads are recorded by a small preload library
  (`LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`): files opened for reading and
  directories listed, inside the repository, each hashed. Reads elsewhere
  (`/proc`, `/etc`, the network) make a test machine-dependent, and it is
  reported as nondeterministic unless they are declared.
- **Hangs, without clocks.** Rust code, the standard library included
  (built with coverage), advances the coverage counters; a mutant's run
  past a fixed multiple of the unmutated run's counter sum is a hang. A
  run whose every thread is blocked, none running, is a deadlock: a
  deterministic test has no timer to wake it. Inside assembly and C, the
  measure is the CPU's count of retired instructions where the system
  gives it (Linux); where it does not (macOS), that case is a stated
  limit.
- **Unprotected code.** Code only excluded tests reach is reported as
  unprotected, at `init` and whenever it grows.

## Running it

- **The hook** checks each commit against its parent, before it is made.
- **CI**, in either of two modes: **per commit** (each pushed commit in
  order, each against its parent: exact, and each finding names its
  commit), or **per push** (the push's newest commit against the commit it
  started from: exact for the push as a whole, its findings naming the
  push; a loss a later commit in the same push repaired is not reported).
  Starting again from scratch is no third mode: it discards every cause.
- **Every core**, with cargo's jobserver honored; progress as cargo shows
  it; findings in a fixed order.

## The self-audit

On sampled commits, the tool computes the commit's verdict from scratch
and requires it to equal the incremental verdict. A difference is a bug in
the tool, with the commit that shows it. It covers whole classes of
failure at once, the ones not yet imagined among them.

## Failure modes, and their answers

| failure | answer |
|---|---|
| a change outside functions (constants, types, macros, `cfg`, `build.rs`, dependencies, assembly) | compiled fingerprints |
| a test's data changes | its reads recorded and hashed |
| a change makes a mutant elsewhere uncaught (action at a distance) | the exact rule: judged again when a test reaching it runs again |
| a dependency or toolchain upgrade | the same source measured under both contexts, the difference reported against the upgrade |
| tests and functions a macro generates | fingerprinted by the macro's invocation; a test the tool cannot place is reported at `init` |
| matching a changed function to its earlier version | none needed: the explicit list |
| a test that is not deterministic, unexcluded | `look-for-nondeterminism`; the self-audit |
| code only excluded tests reach | reported as unprotected |
| another machine reaches other code (CPU, OS) | CI on each platform; the hook checks its own machine's paths, as the README says |
| tests in another crate, doctests, benches | out of scope for now, stated |
| code cargo-mutants does not mutate (assembly, C, inside macros) | out of scope, stated |
| the wrong baseline (branch switches, rebases) | maps keyed by commit, checked against the commit's own parent |
| a merge loses what its branches kept | checked against every parent |
| a lost cache | maps stored in git |
| a commit bypassing the hook | CI, per commit or per push |
| hangs | counter budgets, deadlock by state, retired instructions in assembly and C |
| equivalent mutants in new code | named in the list with their reason: the main cost to "painless", to measure |
| bugs no mutation operator models | outside the promise, stated |
