# beamfs-xfstests

Runs the kernel's filesystem test suite against beamfs, split across the
lab's nodes, one test per invocation, and keeps a record of every run.

Version 2.5.4. Version 2.5.1, from commit `b22ecb4`, produced the
xfstests results of the beamfs v3 technical report,
[10.5281/zenodo.23253350](https://doi.org/10.5281/zenodo.23253350).

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

A test can have a budget of its own when the flat one is too short for
it on this lab: `generic/476` gets 3600 s. It passed in 2653 s in one
x86 sweep and was killed at 1876 s in another. The longer of the two
budgets applies; the budget line of the sweep names the test, and
`meta.txt` carries `test_budgets=`.

## Result classes

| Class | Meaning |
|---|---|
| PASS | the test ran and matched its expected output |
| FAIL | the test ran and did not, or the kernel reported an incident while it ran |
| NOTRUN | the suite declined the test, for the reason it gave, most often a feature beamfs does not implement |
| HANG | the test exceeded its timeout and was killed |
| MOUNTFAIL | the device could not be mounted before the test began |

NOTRUN is not failure and belongs in the paper's scope section rather
than in a bug list: no filesystem passes the whole suite, and the
reasons -- O_DIRECT, fallocate, xattrs -- are design decisions.

## What fails a test, and a sweep

A test passes when `check` says it passed and the kernel said nothing,
while it ran, that is an incident. `check`'s own `_check_dmesg` catches
the generic reports (BUG, WARNING, Oops, lockdep, RCU, UBSAN), not what
beamfs says when it finds damage. Here an incident is any message of
the kernel facility at warn or above, read with `dmesg -x`; the tree
checker's `LOST POINTER`, at whatever level; and any line in which
beamfs says it corrected something, at whatever level.

Some tests provoke a kernel message on purpose: dm-error under the
volume, a loop device on a full volume, a mount without a type. Each
such message is listed with its test, the text it contains and why in
`tests/expected-kernel-messages` of the beamfs tree (`BEAMFS_TREE`,
else `~/git/beamfs`), or in the file `XFSTESTS_EXPECTED_MESSAGES`
names, and is set aside for that test alone. A line without a text or
without a reason is refused. The tree checker's `LOST POINTER` and a
correction by beamfs are never set aside. `sweep` and `bench` read the
list; `run` does not yet.

A sweep ends with a verdict on itself, and its exit status says it: 0
when the verdicts are clean, 4 when they are not, 1 when the sweep
could not run. They are not clean when a test failed and is not a known
failure, listed with its reason in `tests/known-failures` of the beamfs
tree or in the file `XFSTESTS_KNOWN_FAILURES` names; when a test listed
there passes, since the line of a test goes with the fix of its defect;
when a test has no verdict or was never started; when the kernel was
tainted before the sweep by a machine check, a bad page, a death, a
warning or a soft lockup, or its taint changed during the sweep; when
the node had to be recovered; when lockdep was not on at the start or
switched itself off; or when the apparatus failed.

## Records

Every sweep leaves a record under
`~/.local/share/beamfs-xfstests/sweeps/sweep-<start>/`:

- `meta.txt`, written at the start and at the end: harness version,
  beamfs commit, node, kernel, taint, `debug_locks` and `kmemleak`,
  module, and whether beamfs is built into the kernel or loaded as a
  module, `check` and `local.config`, devices, image seal, budget and
  the tests with a budget of their own, the lists of known failures and
  of expected messages with their sha256; at the end, the recoveries of
  the node and the verdict of the sweep with its reasons;
- `verdicts.txt`, written as the verdicts come;
- the output of `check` for each test.

The trace keeps the kernel log of each test.

`compare` sets two records side by side and, when they were judged by
different versions of this tool, names each: a verdict that changed
between them may be the rule's and not the code's.

## Documentation

`doc/beamfs-xfstests.1` is the manual (`man -l doc/beamfs-xfstests.1`):
commands, environment, the trace, the files. `doc/lab.md` is about the
lab.

## License

GPL-2.0-only, see `COPYING`.
