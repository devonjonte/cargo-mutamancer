# Lessons learned

What running mutation tests on BLAKE3-servil has taught, for the design
of this tool. Each lesson says what happened, what it teaches, and what
the Mutamancer should do. BLAKE3's check is `tools/mutants/check.py`
(cargo-mutants and nextest, with lists of the mutants known to survive
and to time out, one pair of lists per architecture).

## A list of known survivors describes the code it was written from

**What happened (October 26, 2026).** The AArch64 list held 26 rows naming
mutants that no longer exist: functions renamed or removed since the run
that wrote it (`Pool::wake_for`, `group_cvs`, `Task::run`). Nothing noticed.
Only an AArch64 run reads the AArch64 list, and the last one ran on older
code. The x86-64 machine ran the check every day and never looked.

**What it teaches.** Each row has three parts. *Which mutant* is a
function of the source alone. *Its outcome* depends on the run. *Its
reason* is a person's judgment. The first part can be checked
everywhere, at once, for every architecture: `cargo mutants --list`
reads the source without building it.

**For the Mutamancer.** Key every recorded verdict by the mutated
function's fingerprint, and drop or flag a verdict whose fingerprint is
gone, on every check, for every target's map, on whatever machine runs.
A stale entry should be an immediate, named failure.

## A verdict that should hold everywhere is a test of the tool

**What happened.** BLAKE3 judges the thread pool's handshake files
(`park.rs`, `finish.rs`) under Shuttle, a model checker whose verdicts
don't depend on the machine. On x86-64 it caught all 27 of the mutants
the suite only timed out on. The AArch64 list recorded the same 27 as
unexplained time-outs. Whatever happened on that machine (its model
stage not running, or its results not merged), the outcome looked like
27 rows waiting for reasons. It never looked like an error in the tool.

**What it teaches.** Where a stage is deterministic by construction, its
verdict on one machine predicts its verdict on every other. A
disagreement is a failure of the tool or its setup, and a person writing
reasons can't see that from the rows.

**For the Mutamancer.** Compare deterministic verdicts across the maps of
different targets and machines. Report a disagreement as the tool's own
error, naming both runs, before anyone writes a reason.

## Concurrent code gives each machine its own outcomes

**What happened.** Seven mutants that break how the pool wakes its
workers hang the suite on x86-64, and pass it on the AArch64 VM. There
the caller's thread takes the work nobody woke a worker for, so the
answer stays right and only the speed changes. The same mutant is
"times out" in one list and "survives, reason unknown" in the other.

**What it teaches.** For concurrent code an outcome belongs to the code
and the machine together. A reason written on one machine needs checking
on the other.

**For the Mutamancer.** Record the machine context with every verdict, as
the reach map already does, and show a mutant's outcomes on every machine
side by side when one of them changes.

## The best answer to a surviving mutant can be less code

**What happened.** The pool checked `any_parked()` before every
`wake_one()`. But `wake_one` already does nothing when no worker is
parked, and costs the same one load per word when none is. The lists
explained the survivors there as speed only: "a publisher tries a wake
with nobody parked; wake_one finds no bit". Removing the check, with
`Pool::wake_one` and `Pool::any_parked` (wrappers that only forwarded to
`Park`), deleted three functions. Every mutant they carried went with
them, among them 3 of the 7 unexplained AArch64 survivors, and so did the
rows in both architectures' lists.

**What it teaches.** A survivor whose reason is "only speed" or "an early
exit" often marks a duplicated guard. A forwarding wrapper gives each
mutation two places, and the second copy's verdict rides on the first
one's tests. Code where a mistake can't be written needs no test to catch
it.

**For the Mutamancer.** Next to "the next test to write" and "the tests to
delete" (NEXT-STEPS), report *the code to consider deleting*: a guard
whose mutants survive because another guard repeats it, and a function
whose body only calls another.

## A check that can't run must say so

**What happened.** A change of signatures in BLAKE3's proof tools
(`#171`) left three callers with the old arguments. A word search for
callers missed them: they passed a bare `False`. The proofs' own mutation
controls (`tools/verify/mutants.py`) would have caught them, but that
tool had never run on the machine that made the change. Under Python
3.14 its workers start through a fork server, which can't send its
jobs. It looked like a tool for another machine, and nobody ran it there.

**What it teaches.** After changing a signature, run the callers rather
than search for them. A check that can't run on a machine is a failure to
report on that machine. Treating it as another machine's job leaves the
gap unseen.

**For the Mutamancer.** When the tool can't run a test it would need (a
target, a toolchain, a start method), print that as a finding: what it
couldn't run, and which mutants' verdicts therefore stand unchecked.

## A hand-kept copy of the tool's knowledge goes stale

**What happened.** BLAKE3's tests carried notes such as "Keep: it detects,
by hanging, mutants the clock then stops (timeouts-x86_64.txt:
publish_job, take, wake_one)", naming by hand which listed mutants each
test catches. Several named functions that no longer exist (`Task::run`)
or mutants never listed (`register`, `waiting`). The notes now point at the
list, which the check keeps true, and name nothing.

**What it teaches.** A fact the tool can compute, copied by hand into the
code, is a second record nobody checks.

**For the Mutamancer.** The kill map already says which tests catch each
mutant. Offer that answer where a person would otherwise write it down:
`why-keep TEST` printing the mutants only that test catches.

## Show final verdicts first

**What happened.** BLAKE3's report printed `TIMED OUT (place)` for places
that its model stage then caught, under a final line that said the check
passed. Reading those lines, I took mutants the model had caught for
hangs, until I traced each one through the kept logs.

**What it teaches.** An intermediate outcome printed beside a final one
reads as final.

**For the Mutamancer.** Print each mutant's final verdict. Keep the path
to it (timed out in the suite, caught by the model) behind a clearly
labelled detail.

— Devon Jonte, October 26, 2026
