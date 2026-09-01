# beamfs-xfstests

Runs the kernel's filesystem test suite against beamfs, split across the
lab's nodes, one test per invocation.

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
three minutes. The suite is roughly 737 generic tests. Sequentially that
is days; across four nodes it is a night.

Sharding is by index modulo node count rather than by contiguous range,
so the slow tests -- fsstress, fsx, anything that fills the device --
spread evenly instead of landing on one node.

## Result classes

| Class | Meaning |
|---|---|
| PASS | the test ran and matched its expected output |
| FAIL | the test ran and did not |
| NOTRUN | the test declined: a feature beamfs does not implement |
| HANG | the test exceeded its timeout and was killed |
| MOUNTFAIL | the device could not be mounted before the test began |

NOTRUN is not failure and belongs in the paper's scope section rather
than in a bug list: no filesystem passes the whole suite, and the
reasons -- O_DIRECT, fallocate, xattrs -- are design decisions.
