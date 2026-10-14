# beamfs-xfstests

Runs the kernel's filesystem test suite against beamfs, split across the
lab's nodes, one test per invocation, and keeps a record of every run.

Version 2.6.0. Version 2.5.1, from commit `b22ecb4`, produced the
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

## The plan

612 of the 740 tests of the x86 sweep `sweep-1791798230` were declined
by xfstests -- reflink, O_DIRECT, quotas, a dm target -- each after the
preparation, the trial and the evidence of a test that runs: 13 of the
19 hours of that sweep. Whether xfstests declines a test does not change
while the node does not, so a sweep asks once.

Each test of the selection is cut before its first line of work. What
comes before, its preamble, is sourcing, `_begin_fstest`, function
definitions, assignments, `_require_*`, `_exclude_*`, `_supported_*`
and the guards that end in `_notrun`. The cut test ends with a
`_notrun` of its own, and all of them run through one `./check`, from a
copy of `check`, `common/` and `tests/` under `/var/tmp/bx-plan` on the
node; `/usr/xfstests` is not written. A test that reaches the end of its
preamble runs. One that xfstests declines on the way is set aside with
the reason xfstests gave, in the test's own context. One that leaves no
answer runs. A requirement written after the preamble is not seen: its
test runs and is declined as before, a test run for nothing and never
one hidden.

The plan is kept in `~/.local/share/beamfs-xfstests/plans/<node>/`,
under a key: the sha256 of the image sealed at deploy, the kernel,
`check`, `common/` and `tests/`, the module, `mkfs.beamfs` and
`fsck.beamfs`, `local.config`, the devices and the mkfs options. A sweep
under a known key reuses its plan; under a new one it makes it first,
and says how many tests it has asked and how many xfstests declined.
The declined tests are recorded `NOTRUN`, their reason prefixed with
`planned:`, and are not run. Ten of them, chosen anew each sweep, run
anyway as a control: one that runs makes the sweep not clean.
`XFSTESTS_PLAN=0` runs every test, `XFSTESTS_PLAN_CONTROL` sets the
number of controls.

`beamfs-xfstests plan --against <sweep>` makes the plan again and sets
it beside the verdicts of a sweep, by its tag or its directory; it exits
4 when a test that ran there is declined by the plan. Made by hand on
x86-01 on 2026-10-14 and set beside `sweep-1791798230`, the plan took
923 s: no test that ran in that sweep was declined, 563 were declined
with the reason the sweep gave, and 49 that the sweep saw declined were
run, their requirement being written after the preamble.

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
- the plan it ran under, `<key>.plan`, named by `plan=` in `meta.txt`;
- `verdicts.txt`, written as the verdicts come;
- the output of `check` for each test.

The trace keeps the kernel log of each test.

`compare` sets two records side by side and, when they were judged by
different versions of this tool, names each: a verdict that changed
between them may be the rule's and not the code's.

## Before a measurement

Before it measures, beamfs-xfstests checks the chain from the
repositories to the node, and `beamfs-xfstests checkpoint` prints it:
the sources of beamfs and of its checker against their copies in the
Yocto layer, the versions they declare, the commits against the image,
the deployed image against the newest built, and that no other run
holds the node. A broken link stops the measurement, which would
describe other code than it names; `XFSTESTS_FORCE=1` takes it anyway.

The checkpoint also compares the newest mainline tag on kernel.org,
read with `git ls-remote` within 20 seconds, with the kernel the build
directory of the machine prefers, `PREFERRED_VERSION_linux-mainline` in
its `conf/local.conf`. A build behind kernel.org is a warning, as is
kernel.org out of reach: the measurement still describes the kernel it
ran. The warnings are printed before the measurement starts.

## Documentation

`doc/beamfs-xfstests.1` is the manual (`man -l doc/beamfs-xfstests.1`):
commands, environment, the trace, the files. `doc/lab.md` is about the
lab.

## Changelog

The full record is in [CHANGELOG.md](CHANGELOG.md); its latest entries
follow.

### [2.6.0] - 2026-10-14

- A sweep runs only the tests xfstests runs on its node. Each test of
  the selection is cut before its first line of work and ended by a
  `_notrun` of its own, and all of them go through one `./check`, from
  a copy under `/var/tmp/bx-plan`: xfstests answers, in each test's own
  context, with its own reason. The answer, the plan, is kept under a
  key made of the sealed image, the kernel, `check`, `common/` and
  `tests/`, the module, `mkfs.beamfs` and `fsck.beamfs`, `local.config`,
  the devices and the mkfs options, and reused while the key holds. The
  tests it declines are recorded `NOTRUN` with `planned:` and their
  reason, and are not run; ten of them, chosen anew each sweep, run
  anyway, and one that runs makes the sweep not clean. `XFSTESTS_PLAN=0`
  runs every test; `XFSTESTS_PLAN_CONTROL` sets the number of controls.
- `beamfs-xfstests plan [--against <sweep>] [selection]` makes the plan
  again and, with `--against`, sets it beside the verdicts of a sweep:
  it exits 4 when a test that ran there is declined by the plan.

### [2.5.6] - 2026-10-14

- A test that xfstests passed and the kernel did not gives in its
  reason every message the kernel said, in the order it came, a repeat
  counted, where it gave the first three: a list of expected messages
  made from those reasons missed the rest.
- A fsck or mkfs that `deploy` could not build shows the whole of what
  the compiler said, where it showed its last three lines.
- The checkpoint no longer reports the deployed image as the newest
  built when there is no image, or no deploy recorded, to compare: it
  is a warning, as it already was for the commits.
- The manual says what generic/067 provokes: a mount without a type,
  which tries every block filesystem the kernel knows.

## License

GPL-2.0-only, see `COPYING`.
