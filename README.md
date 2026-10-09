# beamfs-xfstests

Runs the kernel's filesystem test suite against beamfs, split across the
lab's nodes, one test per invocation, and keeps a record of every run.

Version 2.5.1. It produced the xfstests results of the beamfs v3
technical report,
[10.5281/zenodo.23253350](https://doi.org/10.5281/zenodo.23253350), from
commit `b22ecb4`.

## Why not `check -g generic`

Because a single hung test takes the whole run with it. On 2026-09-01,
`generic/285` sat in `folio_wait_writeback` for nine hours and the other
127 tests in that batch never ran. The suite has no per-test timeout and
no way to resume; a night of machine time bought one result.

Here each test is its own invocation with its own timeout. A hang costs
that test, is recorded as `HANG`, and the run continues. State is written
after every test, so an interrupted run resumes instead of restarting.

## Why shard

Under TCG emulation a test that takes ten seconds natively takes two to
three minutes. A sweep runs the suite's `auto` group, 734 tests (731
generic, 3 shared). Sequentially that is days; across four nodes it is a
night.

Sharding is by index modulo node count rather than by contiguous range,
so the slow tests -- fsstress, fsx, anything that fills the device --
spread evenly instead of landing on one node.

## Time budget

Each test runs under a budget, `XFSTESTS_TRIAL_TIMEOUT` seconds, 1900 by
default. Some tests need more: `generic/476` took 2653 s on x86-64 under
KVM, and the v3 sweeps ran with 14400 s. In a sweep, a test stopped by
its budget is saved as `FAIL`, with the shell's kill report in its check
output; the record names the budget at the start and at the end.

## Result classes

| Class | Meaning |
|---|---|
| PASS | the test ran and matched its expected output |
| FAIL | the test ran and did not |
| NOTRUN | the suite declined the test, for the reason it gave, most often a feature beamfs does not implement |
| HANG | the test exceeded its timeout and was killed |
| MOUNTFAIL | the device could not be mounted before the test began |

NOTRUN is not failure and belongs in the paper's scope section rather
than in a bug list: no filesystem passes the whole suite, and the
reasons -- O_DIRECT, fallocate, xattrs -- are design decisions.

## Records

Every sweep leaves a record under
`~/.local/share/beamfs-xfstests/sweeps/sweep-<start>/`:

- `meta.txt`, written at the start and at the end: harness version,
  beamfs commit, node, kernel, taint, module, `check` and
  `local.config`, devices, image seal, budget; at the end, the
  recoveries of the node;
- `verdicts.txt`, written as the verdicts come;
- the output of `check` for each test.

The trace keeps the kernel log of each test.

## Documentation

`doc/beamfs-xfstests.1` is the manual (`man -l doc/beamfs-xfstests.1`):
commands, environment, the trace, the files. `doc/lab.md` is about the
lab.

## License

GPL-2.0-only, see `COPYING`.
