# Changelog

Every change to beamfs-xfstests, newest first.

The entries below were rebuilt on 2026-10-14 from the commit history:
one line per commit, its subject and its short hash, under the version
its Cargo.toml carried after each commit; a subject that begins with
that version is given without it, and one that names nothing but the
version gives way to the first paragraph of its message. Tags are named
on the commit they point to. From the next version on, each version adds
its entry, written with the change, and the README carries the latest
entries.

## [2.5.4] - 2026-10-12

- README: 2.5.4, and what fails a test or a sweep (`3d01499`)
- what a test provokes on purpose, and a budget of its own (`ab69fdd`)

## [2.5.3] - 2026-10-10

- an incident is a level, not a word (`921b31e`)

## [2.5.2] - 2026-10-10

- a sweep that can fail (`b8b9e3e`)

## [2.5.1] - 2026-10-08

- COPYING: the full text of the GPL-2.0 (`89d3499`)
- README: records, time budget, and the beamfs v3 report (`7e5e79b`)
- the record names the budget and the recoveries (`b22ecb4`)

## [2.5.0] - 2026-10-08

- what BX knows of beamfs, brought to 0.1.26 (`c814ca1`)

## [2.4.1] - 2026-10-08

- the record names the beamfs module the node runs (`4e52c5d`)

## [2.4.0] - 2026-10-08

- not run is not a pass; each sweep keeps its record (`00af34a`)

## [2.3.61] - 2026-10-08

- each node keeps its own evidence (`9773209`)

## [2.3.60] - 2026-10-01

- a bench is compared on its own node; no verdict without a reason (`4aae36b`)

## [2.3.59] - 2026-09-29

- a run is compared only under the same load; pruning spares kept-* (`2d340f5`)

## [2.3.58] - 2026-09-28

- scrubwb follows beamfs 0.1.23 (`1a4fe27`)

## [2.3.57] - 2026-09-28

- scrubwb prints every check and every confirming read (`d365cbb`)

## [2.3.56] - 2026-09-28

- scripts/scrubwb.bt, what the scrubber writes back (`ba7d153`)

## [2.3.55] - 2026-09-28

- the seed reaches fsstress and fsx; random seeds; a bench keeps the previous case (`e181924`)

## [2.3.54] - 2026-09-28

- scripts/nlink.bt, link counts against directory entries (`35de082`)

## [2.3.53] - 2026-09-28

- 2.3.52, compiled (`1b08073`)

## [2.3.52] - 2026-09-28

- the listing's output survives check (`adb9128`)

## [2.3.51] - 2026-09-28

- prepare writes the devices; the listing's header is checked; nodes exec; history put back (`6283acc`)

## [2.3.50] - 2026-09-28

- the listing is watched, and its output kept (`3abd946`)

## [2.3.49] - 2026-09-27

- the listing gets the trial budget; the wedged domain is the node's (`39893cb`)

## [2.3.48] - 2026-09-27

- deploy declares the node ready when its root takes a write (`78b5a1b`)

## [2.3.47] - 2026-09-26

- a probe is attached when BEGIN has spoken; undeposited.bt (`14ff79e`)

## [2.3.46] - 2026-09-26

- deploy pushes the tools bitbake built for the machine's architecture (`dd090d6`)

## [2.3.45] - 2026-09-26

- the frozen image is the one check kept, and the runner log is read (`1171e11`)

## [2.3.44] - 2026-09-26

- the host's side of the scratch device, and the key from the configuration (`6d71b4a`)

## [2.3.43] - 2026-09-25

- the checker reads each device a third time, past the page cache (`05d26c2`)

## [2.3.42] - 2026-09-25

- the checker reads each device twice (`8ecae43`)

## [2.3.41] - 2026-09-25

- soak runs the script as root (`b1a9255`)

## [2.3.40] - 2026-09-25

- soak opens the scratch device by path (`105edb9`)

## [2.3.39] - 2026-09-25

- soak, random writes on the bare scratch device (`93b228e`)

## [2.3.38] - 2026-09-25

- parity.bt has one BEGIN probe again (`476a59b`)

## [2.3.37] - 2026-09-25

- parity.bt prints every event as it happens (`3bbc85b`)

## [2.3.36] - 2026-09-25

- parity.bt counts every lookup of a region block by number (`5458674`)

## [2.3.35] - 2026-09-25

- parity.bt counts every bio that covers a region block (`0f8af53`)

## [2.3.34] - 2026-09-24

- parity.bt keeps the region read timestamps (`b086786`)

## [2.3.33] - 2026-09-23

- let bpftrace finish printing; parity.bt prints what matters first (`ab4684f`)

## [2.3.32] - 2026-09-23

- parity.bt loops instead of unrolling (`d22467a`)

## [2.3.31] - 2026-09-23

- parity.bt tests the whole slot (`6d6d031`)

## [2.3.30] - 2026-09-23

- parity.bt reads the region's bytes (`7dfbf5b`)

## [2.3.29] - 2026-09-23

- parity.bt, and the budget line says the budget (`ff367fd`)

## [2.3.28] - 2026-09-23

- no test formats under the last one's writers (`04e9bc2`)

## [2.3.27] - 2026-09-23

- cpuwho, where the time goes off the disk (`81e9c1e`)

## [2.3.26] - 2026-09-23

- churn, and deploy that stops when it says so (`ec4984a`)

## [2.3.25] - 2026-09-22

- the writers named, the killed asked, the baseline checked (`564a932`)

## [2.3.24] - 2026-09-22

- the spread is judged before the trial count (`ddafce5`)

## [2.3.23] - 2026-09-22

- a difference one trial carries is not a regression (`f068f57`)

## [2.3.22] - 2026-09-22

- what the reports said wrong, seen on 2026-09-22 (`6749ec8`)

## [2.3.21] - 2026-09-22

- the frozen image is inspected whatever device it came from (`9b79635`)

## [2.3.20] - 2026-09-22

- the chain from commits to image is judged on content (`4903a1c`)

## [2.3.19] - 2026-09-22

- speak says the keyed counts, compare names its fixes, state is taken before the dd (`2f0ca79`)

## [2.3.18] - 2026-09-22

- a sweep keeps the stacks of a wedge and restarts before going on (`1bc2fe2`)

## [2.3.17] - 2026-09-22

- the runner's record of a killed test reaches the evidence (`321739c`)

## [2.3.16] - 2026-09-22

- bhbalance prints every caller (`759eaf9`)

## [2.3.15] - 2026-09-22

- bhbalance, buffer_head references by caller (`edbc8b5`)

## [2.3.14] - 2026-09-22

- heldfolio counts the buffers a folio leaves with (`1c77220`)

## [2.3.13] - 2026-09-22

- a node that does not answer is not stopped (`8a24f66`)

## [2.3.12] - 2026-09-22

- heldfolio sits on the tracepoint both paths fire (`bb647dc`)

## [2.3.11] - 2026-09-22

- a probe that died is not one that attached (`4ccf23d`)

## [2.3.10] - 2026-09-22

- heldfolio watches the cache, not the removal (`d971a3d`)

## [2.3.9] - 2026-09-22

- bench and baseline carry the probe too (`b2bbbb9`)

## [2.3.8] - 2026-09-22

- a probe for the folio that outlives its mapping (`3458990`)

## [2.3.7] - 2026-09-22

- the guest is measured, and fsck is read whole (`ad2b77c`)

## [2.3.6] - 2026-09-21

- one stop entry, and one italic that rendered (`53cbab2`)

## [2.3.5] - 2026-09-21

- baseline stores the runs, not a rebuild (`500bac6`)

## [2.3.4] - 2026-09-21

- deploy writes where the domain boots from (`5848f75`)

## [2.3.3] - 2026-09-21

- a guard that does not match itself (`3e4ce85`)

## [2.3.2] - 2026-09-21

- a capture that says what it left out (`8e87000`)

## [2.3.0] - 2026-09-21

- nodes status, a question that changes nothing (`f039587`)

## [2.2.0] - 2026-09-21

- a selection is written, not looped over (`699fc2f`)

## [2.1.1] - 2026-09-21

- only what the image is built from (`9dcf899`)

## [2.1.0] - 2026-09-21

- the shell is checked by the suite (`3752f27`)

## [2.0.0] - 2026-09-21

- a junction before every measurement (`39e9b49`)

## [1.9.0] - 2026-09-21

- find the checker too (`35e8d89`)

## [1.8.0] - 2026-09-21

- read what is already there (`c0fe00b`)

## [1.5.0] - 2026-09-21

- measure what is there, keep what was said (`84443aa`)

## [0.5.1] - 2026-09-18

- probe: attach the script before the test, and write straight (`43db189`)
- probe: a pattern that matches its own shell never reaches zero (`26112f6`)
- stop: escalate until the node is actually idle (`b76fbad`)
- chain: --machine names which chain a run belongs to (`453cbe7`)
- chain: one seal per architecture, one variable per chain (`a2f5c57`)
- chain: write down which image the campaign measured (`2355264`)
- bench: a harness that refused to start did not measure a failure (`28b78c6`)
- control: run the same test against a filesystem known to be sound (`67d965f`)
- bench: tell a run in progress from the processes a dead one left (`371feac`)
- state: name the diskstats field we were reading, and read two more (`44956bb`)
- sweep: honour every test named, not just the first (`7ad5aad`)
- fix: report reads the run you name, instead of ignoring it (`24e7e6f`)

## [0.5.0] - 2026-09-17

- bench: a family is not a group, and mkfs got its option twice (`7262b0c`)
- bpf: owner.bt, and a map that says when it has dropped entries (`67269ae`)
- bench: trace the test that fails, since the synthetic load does not (`4baa68b`)
- node, trace: a budget overrun is not an unreachable node (`aff10d1`)
- trace, matrix: measure the node's own scratch, and stop when it is gone (`9d469fe`)
- bench: switch KCSAN on where sweep actually prepares the node (`254beea`)
- runner: turn KCSAN on, and attribute dmesg to the test that produced it (`99c378e`)
- feat: XFSTESTS_FSTYP -- run the same load against another filesystem (`adc8b9c`)

## [0.4.3] - 2026-09-17

- fix(evidence): look for the image under the name check gives it (`95b5ca2`)

## [0.4.2] - 2026-09-17

- fix(evidence): ask check what it does with DUMP_CORRUPT_FS (`e1ab899`)

## [0.4.1] - 2026-09-17

- fix(sweep): say what the preparation found (`d781fda`)

## [0.4.0] - 2026-09-17

- feat: ask check to keep the volume it declares inconsistent (`88faffc`)

## [0.3.4] - 2026-09-17

- fix(bpf): one duration for the attach, and the rest derived from it (`e7526b7`)

## [0.3.3] - 2026-09-17

- fix: count per volume, and compare the one line that tells them apart (`c5b6714`)

## [0.3.2] - 2026-09-17

- fix: i_state is a struct, three helpers are static, and full phrases it differently (`e948de1`)

## [0.3.1] - 2026-09-17

- fix(evidence): say when fsck.verbose describes a volume nobody tested (`840bb81`)

## [0.3.0] - 2026-09-17

- feat(bitmapwrite): does every change to the bitmap reach its block (`d88f96a`)

## [0.2.2] - 2026-09-17

- fix(neverwritten): every probe in the script filters, and nothing overflows (`f8d7b5e`)

## [0.2.1] - 2026-09-17

- fix(bpf): read both device numbers in one call, and retry once (`a24fc62`)

## [0.2.0] - 2026-09-17

- A probe now filters on the volume it measures, which changes what every capture contains. (`d66764e`)

## [0.1.0] - 2026-09-01

- feat(bpf): a probe measures one volume, not every beamfs mount (`2605548`)
- feat(neverwritten): a block allocated, placed, and never sealed (`ce203f0`)
- fix: a kernel message belongs to the test that produced it (`ff76e9a`)
- fix(evidence): freeze both volumes when the output does not say which (`5db9a5c`)
- fix(deploy): refuse a node running the previous kernel (`1f84932`)
- fix(deploy): check the rootfs on the node is the one just built (`fca0f11`)
- fix(bpf): a minute to attach, not fifteen seconds (`065f699`)
- fix(orphan): exhaustive, because 867 of tens of thousands cannot be sampled (`6f0cfa2`)
- fix(twobuf): the indirection region only (`c512e2e`)
- fix(twobuf): bdev_getblk, since __getblk_gfp is not traceable (`11792a2`)
- feat(twobuf): two buffers for one block, or one changing under a reader (`3f2f90e`)
- fix(bpf): wait for the probe to attach, rather than guess at it (`74d0c67`)
- feat(iomapend): does iomap_end run, and with what (`c453da8`)
- fix: the mkfs options reach the volume xfstests formats (`e0626f1`)
- feat: a scenario asks one question in ten seconds (`fd28842`)
- fix(bpf): keep the capture even when the start check fails (`42a059e`)
- feat: say what treecheck caught, first and whole (`ac4719c`)
- feat: a test the budget killed is not a test that failed (`05a2e54`)
- fix(orphan): delete only what is being followed (`dd5a42c`)
- fix(bpf): kill whatever is still attached before starting (`97004db`)
- fix(orphan): follow one allocation in 512, not all of them (`b2160b1`)
- feat: write the whole evidence too, nothing dropped (`61a6188`)
- fix: the digest keeps the text, all of it (`5c8c519`)
- feat: the digest names the capture, and mdflush reads beamfs_mmb (`de41b0e`)
- feat: write the evidence as one plain file, beside the archive (`a3a87a1`)
- feat(orphan): a block allocated and never placed in a tree (`49ae0f7`)
- feat(deploy): say when the image is older than the code (`0a049fb`)
- feat(mdflush): a metadata block dirtied and never written (`d096e9e`)
- fix: baseline records what it lost, so trend can see it (`4ff9c73`)
- feat(iomapleak): a short write, or a mapping longer than what is allocated (`4782544`)
- feat(lowio): watch the block layer, not the buffer heads (`3619619`)
- feat(lowblock): who writes below data_start (`c17630d`)
- fix: the volume is judged after it is frozen, not before (`df702bb`)
- fix(bpf): the quotes around the grep pattern closed the sudo sh -c (`05b0219`)
- fix: the quotes around the grep pattern closed the sudo sh -c (`d505235`)
- feat: sweep trusts a deploy from the same boot (`395dc15`)
- feat: test what failed today, and stop ringing for what is not a run (`2d5ba05`)
- feat: an apparatus failure is not a finding (`59a4954`)
- feat: sweep saves its run, so compare has something to read (`9ba6252`)
- feat: trend says where a number moved, and which test to look at first (`420d27b`)
- feat: empty the node between tests, and never read one loss alone (`2638a16`)
- feat: say when the layer's checker sources are behind this repo's (`9ffaf40`)
- feat: collected output is collapsed the same way the journal is (`ecada31`)
- feat: the journal counts what repeats instead of writing it again (`6f443d8`)
- feat: say what led to a recovery, before repairing it (`33b0fb1`)
- fix: a flat budget of 1900 seconds, and the watcher does the judging (`8a1a086`)
- feat: a test is cut when it stops working, not when the clock runs out (`eed3f60`)
- fix: detect the sanitizer by its symbols, not by a config that is not there (`5b1524a`)
- feat: the budget follows the kernel, and silence gets a name (`86393bd`)
- docs: deploy in the usage text (`0e2ae42`)
- feat: deploy is part of the harness, and a wedging test is set aside (`1567c11`)
- feat: a run proves what it claims before it measures (`d0caa5f`)
- fix: a case directory holds one run, not every run that used the name (`6c3acad`)
- feat: refuse a node that is not running this repo's tools (`5ed0870`)
- feat(bitmap): a block handed out that was never freed (`f9c8ae4`)
- feat(whofreed): the stack of the free that hands a live block away (`1fca62e`)
- feat(ptrval): same inode, or a different one (`3bc9cb7`)
- feat(whyzero): the value, not a ninth guess (`8b36452`)
- fix(zeroed): a count_t cannot be printed (`47c4121`)
- feat(zeroed): does a memset land on a block already in use (`63d1de6`)
- fix: keep the harness's own files out of what xfstests deletes (`a8a12ab`)
- fix: the probe's output went where the shell could not read it (`3a3be1e`)
- fix(bpf): say what bpftrace said (`113205c`)
- fix(lostptr): the field is slotval, not val (`bbffc5d`)
- feat: a bpftrace script, attached for the length of a test (`03431e6`)
- fix(evidence): the device path, not the bare name (`2bc28af`)
- fix(runpack): xz, not zstd (`e2b09a7`)
- feat: keep more per failure, read it, and leave it where it can be taken (`bbcc0e5`)
- feat: drain the node before repairing it (`6b1bdda`)
- fix(volume): slots per region block, not bytes per region block (`3f9bc0a`)
- feat(sweep): bring the node back instead of giving up on it (`d23d209`)
- fix: a test gets 900 seconds, not 1800 (`d29a2f7`)
- feat: ask the node what it was doing when it stopped answering (`8ae3974`)
- docs: COPYING, for packaging (`db418ff`)
- fix: the Geometry test builds a complete one (`78bb897`)
- feat: --no-bell, for a run chained into another (`8724c54`)
- fix(evidence): freeze the volume the test broke (`fdf034a`)
- docs: what the bell needs, and how to silence it (`caea796`)
- feat: ring when a run finishes, until someone answers (`97bbdda`)
- fix: the commit a run is attributed to was never read (`0938d22`)
- feat(sweep): stop on request, and stop when the node has gone (`bb5eaba`)
- fix(volume): a short symlink keeps its target where the pointers live (`2ac875c`)
- feat: report the shape of the damage, and compress traces before pruning (`e67d49b`)
- docs: the manual covers what the tool does (`7bb9895`)
- fix(volume): the offset probe had a literal newline in its C source (`9b69dd9`)
- fix(sweep): prepare the node before asking it to enumerate (`e0be908`)
- feat(volume): audit indirect parity where the walk stops, and measure the offsets (`5b646b6`)
- fix(node): a failing command's stdout was thrown away at the ssh layer (`5c66ec4`)
- style(volume): keep data_start, say what it is for (`24d7d83`)
- fix(volume): the root and the canary live below data_start (`ac6aa9a`)
- feat: sweep runs a suite test by test, and the indicator stops crying wolf (`2dea5ff`)
- fix(evidence): dd needs /dev, and an empty capture is a failure (`6c457cd`)
- fix(evidence): the -x trace goes to a file, not to the compared output (`1d2ad64`)
- feat: keep the evidence, seed the load, and find where the trials diverge (`e6efa08`)
- fix(bench): the block count was reading nothing, and a short failure is not a leak (`14e9d1d`)
- fix(bench): grep matching nothing is not a busy node (`f15d2f2`)
- fix(bench): the reason survives the indicator, and one campaign per node (`4fb21c2`)
- fix(bench): a failing trial that lost nothing still has a reason (`adc0ef4`)
- fix(trace): blktrace runs to its own deadline instead of being detached (`eca00ec`)
- fix(trace): the operation is WS, not W (`02dc4bc`)
- fix(trace): blktrace dies with the ssh session unless detached (`1de1281`)
- style(bench): both branches returned one (`05e9364`)
- feat: capture the whole write path around every trial, not a pass count (`3aeccc7`)
- feat(stats): judge a difference against the spread, not against nothing (`32e525d`)
- feat: a bench command, so a regression is seen when it is made (`2dfc940`)
- fix(analyse): read the store value from the arrow, not a new= field (`5540b20`)
- style(indicator): drop an accessor nobody calls (`6e89ba6`)
- feat: the live indicator is a module, used by matrix as well as trace (`ccead94`)
- docs: a literal line starting with ... is a troff macro (`e54f472`)
- fix: an empty condition argument means all of them, and the page renders (`c7342ff`)
- feat: a matrix command, so a condition test is not a throwaway script (`4676bb6`)
- test(load): the fresh flag governs the scratch, not the test volume (`5944b42`)
- fix(load): the test volume stays mounted, as it does under xfstests (`df3594d`)
- docs: the help lists every command, and analyse shows blocks and slots (`df42430`)
- fix: a typo shows the usage, it does not start a run (`6dbb2b6`)
- fix(trace): a capture holds what its own loop lost (`352c771`)
- docs: the analyse command, and how to read what it says (`40a319c`)
- feat: an analyse command, so a capture answers its own questions (`0e3b081`)
- docs: a manual page (`bec1caf`)
- fix(load): a busy unmount is retried, not fatal (`7ffdc77`)
- feat(trace): the indicator follows the work, and says when it stalls (`5ab4e60`)
- style(load): writeln for a trailing newline, and report a fresh loop (`fc47e9a`)
- feat: the load is a module of the tool, not a script pushed at it (`8142ff2`)
- fix(trace): a clean loop is not a failure, and show the work as it runs (`a9758f2`)
- feat: a trace subcommand that keeps what it captures (`56697e5`)
- feat(tools): a capture harness that survives its own session (`d40d53f`)
- feat(runner): a wider budget for tests long by design (`1bb8c70`)
- fix(runner): tasks in D after a killed test are the writeback, not a wedge (`bf50ca7`)
- fix: kill the runner before starting one, always (`8a82a64`)
- fix(stall): a node that is writing is not stuck (`a11d582`)
- fix(archive): not /tmp (`1ba64c0`)
- feat(archive): shelve every verdict as it lands (`a83303a`)
- feat(watch): the node keeps its own account (`3b90729`)
- fix: pull the evidence as it appears, not at the end (`29a2836`)
- fix: answering ssh is not being recovered (`8159a47`)
- fix: sample while the test runs, unmount until it takes (`4d02a3d`)
- fix: a node that only times out is not making progress (`42903e2`)
- feat(probe): watch one test from outside the machine running it (`a293807`)
- feat: recover from a wedged node, and remember what happened (`b6196d0`)
- feat: keep the evidence (`a114f3f`)
- initial: run the filesystem suite without losing the run to one test (`86b0a96`)
