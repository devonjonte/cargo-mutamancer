# cargo-causal-mutation

Mutation testing for Rust that checks each commit for exactly what it
changed, in seconds, and remembers why.

A test suite's strength changes one commit at a time: a commit deletes the
only test that reached some branch, narrows an assertion, or adds code no
test exercises. `cargo causal-mutation` checks each commit as it is made,
and reports each loss beside its cause:

```text
src/many.rs: hash_four_at_a_time, branch 3 (true side): reached by no test
  since this commit: it removes many::test::test_hash_blocks_without_plans,
  the only test that reached it
```

It keeps, for every test, the branch sides it reaches (the *reach map*),
and, from version 0.2, for every mutant, the tests that catch it (the
*kill map*). A commit updates them only where it can change them: a commit
that reformats code, or moves or deletes tests, costs a lookup; one that
changes code costs one incremental build and the runs of the tests that
reach the changed code.

## Status

Version 0.1 is in development: the reach map, causal coverage at each
commit. Version 0.2 adds mutants (the kill map), the tool's reason for
being. Version 1.0 freezes the interface.

## Use

It needs a nightly toolchain, which builds the tests with LLVM's branch
coverage:

```sh
cargo install --git https://github.com/johnservil/cargo-causal-mutation
cargo causal-mutation init                 # the first map: every test, once
cargo causal-mutation install-hook         # check each commit before it is made
cargo causal-mutation check                # the same, by hand
```

`--toolchain T` names the nightly (`nightly` by default), `-j N` the
workers, and arguments after `--` go to `cargo test` (features, for
example). A check that finds nothing saves the new map; one that finds
something keeps the old one, so its findings stay until they are fixed,
or accepted with `check --accept`.

## How it works

- **Fingerprints.** Each function is fingerprinted by its tokens (BLAKE3,
  derive-key mode, context `"cargo-causal-mutation 2026-10-18 Rust fn
  token fingerprint v1"`), so layout, comments, and formatting change
  nothing. A commit's work is decided by which fingerprints it changes.
- **Reach.** Each test runs alone, in its own process, under LLVM's source
  coverage with branch coverage (nightly Rust); its counters are read in
  process (`llvm_profparser`), and the map records, for each test, which
  branch sides and regions of which functions it reached: whether, never
  how often.
- **Determinism.** The maps are a function of the source, the build
  (toolchain, `Cargo.lock`, flags, target), and the machine's context
  (CPU features, CPU count), and are keyed by all of them. The tests the
  tool uses are the tests you promise are deterministic: all of them, but
  those you list as excluded:

  ```toml
  [package.metadata.causal-mutation]
  exclude = ["lanes::test::*"]
  ```

  `cargo causal-mutation look-for-nondeterminism` measures every included
  test again and reports each whose reach changed with nothing it depends
  on changed. It can find nondeterminism, never prove its absence.
- **Every core.** Work runs on `-j` workers (by default one per CPU),
  taking cargo's jobserver tokens when there is one; progress is shown
  as cargo shows it.

For the best results: seed hashing deterministically in tests, keep the
tests' environment fixed, and give concurrent code a deterministic schedule
in its tests (for example with [Loom](https://github.com/tokio-rs/loom)),
so the excluded list is empty.

## License

You may use `cargo-causal-mutation` under the terms of any of these four
Free and Open Source Software licences:

* MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
* Transitive Grace Period Public License 1.0 ([LICENSE-TGPPL](LICENSE-TGPPL) or https://spdx.org/licenses/TGPPL-1.0.html)
* Bootstrap Open Source License v1.0 ([LICENSE-BOSL.txt](LICENSE-BOSL.txt))
