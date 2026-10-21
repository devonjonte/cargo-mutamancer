# Next steps (for the maintainers: Zooko and John Servil)

Where the work stands, what is decided, what is proposed, and what to do
next. Read this first, then DESIGN.md.

## Where it stands (October 19, 2026)

Built and pushed (main, bd61828):

- Maps in git, bound to commits by tree id (notes ref per context).
- Compiled fingerprints from rustc's own MIR: the tool is its own
  `RUSTC_WORKSPACE_WRAPPER`. Constants, statics and the native symbols a
  function calls are part of its fingerprint, and a reformat changes none.
  On BLAKE3's large Arm commit, 54 functions changed, all real (LLVM IR
  gave about 180 false ones). Fingerprints are kept per artifact, so a
  build cargo finds fresh gives the same answer.
- Each test's reads, recorded by a preload library (Linux only; macOS is
  issue #6) and hashed. A commit touching only documents runs no test.
- The rerun rule: a test reruns when it changed, ran a changed function,
  or read a changed file.
- The tool must be built with the same nightly it checks
  (`nightly-2026-09-17`), and stops with a message if the two differ.
- `exclude` matches the ids the tool prints. `init` reports unprotected
  code: functions no test the tool runs reaches.
- Before reporting a lost side, `check` reruns once the tests that reached
  it. If one reaches it now, the check stops and names that test.

Issues: #1 one process per test, #2 sharing maps, #3 warm server, #4
naming causes, #5 v0.2 checklist, #6 reads on macOS, #7 on-save runs, #8
redundant tests, #9 `harness = false` binaries are skipped silently.

## The open question: nondeterminism (to settle first)

Real multithreaded code reaches different code on different runs. In
BLAKE3, which worker takes a job and the after-pause clock
(`code_may_be_cold`) change the route, never the answer.
`look-for-nondeterminism` found 27 to 33 of 117 tests varying, and a
different set each run. Excluding them took 9 audit rounds and 46 tests,
and left 189 of about 900 functions unprotected, among them all of
`stream`, `outboard`, and the multithreaded hashing: the streaming APIs,
our first priority. So "reach must be deterministic" makes the tool
useless on code like BLAKE3.

**Lesson recorded in AGENTS.md (October 18).** Running every test two or
three times to tolerate varying reach was a degraded mode, which Design By
Contract forbids. Repetition can find nondeterminism, never prove its
absence. AGENTS.md now says the contract covers what a user hands a tool.

**Proposed contract, explained to Zooko and awaiting his decision:**
"Run on the same code, each of your tests either always passes or always
fails." Nothing more. It asks nothing new: a flaky test is already a bug
to its owner. A test may take a different route each run; coping with
that is the tool's job. A test that passed and then failed on unchanged
code gets a plain message naming it as flaky, with how to fix it.

**Proposed way to cope with varying routes, with no repetition and no
exclusion list:**

1. *Which tests to rerun:* a sound over-estimate from the static graph.
   The wrapper already reads each function's MIR. It also records what
   each function refers to: calls, closures passed along, function
   pointers, and for `dyn`, the trait implementations of every type
   coerced. A test reruns when a changed function is reachable from it in
   that graph. This holds under any schedule. It costs precision; BLAKE3's
   whole suite takes 4 s.
2. *Findings, by cause:* a commit can change a test's reach only through
   what it changed (the test, a file it reads, a changed function on a
   static path from the test to the side). A side lost with nothing
   changed on any path to it is the schedule's doing: no finding, and the
   side is recorded as varying. What is left (changed code on the path
   and a schedule-dependent side) gets the confirming rerun, as built
   now. The same rule serves the kill map.
3. Where exact verdicts are wanted inside concurrent code, Loom and
   Shuttle make it deterministic. The tool's message points there.

**The decisive experiment:** replay `servil`'s last 200 or so commits
through `check` and count every false finding. Zooko's bar is "almost no
false alarms". Plan for about 15 s per commit, so under an hour, after a
day's work on items 1 and 2. If false alarms stay frequent, the idea
fails for concurrent code, and we will know why.

Rejected, with reasons:

- Run each test three times, or only those "known to vary": a degraded
  mode, and it cannot prove anything.
- One CPU (`taskset`): removes only part of the variation and tests a
  different program. It also found the fork's `collect()` hang
  (johnservil/BLAKE3 issue, below).
- A deterministic OS scheduler (Hermit, rr): heavy, x86-64 or
  PMU-bound. It is also chaotic under any rebuild, since the schedule
  then depends on the whole binary.
- Copying Loom into the tool: Loom works only on code written against its
  types.

## Settled on October 19 (Zooko), to build

The contract: **run on the same code, each test always passes or always
fails.** Nothing more. A test's route may vary; coping with that is the
tool's. When a planted bug goes from caught to not caught, the tool
reruns that one test against that one bug, once (more for a spot it has
seen vary); a catch on the rerun was luck, a miss again is a finding.
Causation through shared memory and raw pointers means no static map can
prove a commit innocent, so the rerun, not reasoning, settles each case.
The static call map keeps its job: choosing which tests to rerun at all,
over-counting on purpose.

**Scope idea (Zooko, October 20):** bench-hashes' `check` (Devon, bench
PR #54) chooses the cells a change can affect by the lines each cell
executes, which is this tool's rule applied to benchmarks. Once the tool
is back in hand, consider one tool for both tests and benchmarks: the same
map of what each measurement reaches, the same rerun rule, one verdict.
Zooko's square (October 20): the tool **tests the tests** (a planted bug
each test must catch), and the same mechanism **benchmarks the
benchmarks** (a planted slowdown each cell must catch: bench-hashes'
calibration plants, run by hand today) and **benchmarks the tests** (each
test's cost, and which tests buy their time: a test that catches no bug of
its own is a candidate to go, as is a cell that no change moves). Devon's
coming patches to the fork show the last of these in practice.

**Tests under nextest (Zooko, October 20):** the servil projects now run
every test under cargo-nextest, each in its own process, and the fork's
mutation check records every test's outcome and time through it. This tool
runs each test in its own process too, by its own runner (`--exact` per
test); when work resumes, weigh handing that to nextest (its process per
test, its time limits that kill and name a hang, its machine-readable
outcomes) and keeping only what is this tool's own: the map and the rerun
rule.

**Equivalent operator swaps (Zooko, October 20):** swapping an arithmetic
operator (`*` for `+`, `%` for `/`) introduces no bug when the operands
make the two agree in practice (`x * 2` and `x + x`; `a % b` and `a / b`
with `a < b`, or `b` a power of two for some pairs; `|` and `^` on bits
that never overlap, as the fork's flag words). The fork's lists carry
dozens of these with the reason "equivalent", each argued by hand. The
tool could recognise the common shapes itself (operands it can see are
equal, constants, or bit-disjoint) and skip the plant, or run the two
sides on recorded inputs and report "no input told them apart" instead of
"survived": a survivor that cannot be a bug is noise for the user.

## Zooko's promises to users (October 18)

These are recorded in DESIGN.md with each one's mechanism. They go in
the README once they are true:

1. No inscrutable error messages.
2. No hanging, no unexplained wait of 30 s.
3. Every problem you need to know about explained simply, with an
   actionable next step.
4. A faster development cycle than before, under typical conditions.
5. No busy work that does not improve the codebase's safety and quality.

Also: tell the user how long a wait will be, whenever it is longer than a
few seconds. That applies to us when reporting to Zooko, too.

## Next, in order

1. Fork: fix `Stream::collect()` (the one-CPU hang, below), through the
   full gate.
2. Zooko's decision on the contract above.
3. The hang check and wait estimates (promise 2). The map records the
   build's and each test's duration. The tool states its expected wait
   and shows progress, and reports a test running far past its recorded
   time at once. Counter budgets give the verdict (DESIGN.md). A test
   hung for 74 minutes without a word on October 18.
4. The static reference graph and causal findings, then the replay
   experiment.
5. Plain messages: a test per stop message, reviewed against the
   promises.
6. The kill map (#5), then on-save runs (#7) and redundant tests (#8).

## Trial setup on BLAKE3

- Worktree: `/workspace/blake3-servil/tmp/wt-ccm`. Trial commits on
  138fcba: ad63fff, a61a7f6 (46 exclusions by full id), and f941346 (the
  five original exclusions).
- The Arm commit (d727f7d) is in `git stash` there; apply it with
  `git stash pop --index`.
- Maps go in `CARGO_TARGET_DIR=/tmp/ccm-blake3`. Features:
  `--features mmap,serde,zeroize,traits-preview`.
- Toy crates go in `/tmp/covt4` (constant, reformat, data file) and
  `/tmp/covt5` (a coin-flip test).
- To build the tool:
  `CARGO_TARGET_DIR=/tmp/ccm-target cargo +nightly-2026-09-17 build`.
