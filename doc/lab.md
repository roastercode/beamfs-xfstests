# The lab, as it actually is

Written because each of these cost an hour or more to rediscover.

## Host

Gentoo, OpenRC: `rc-service libvirtd status`, never `systemctl`.
`~/yocto` is a bind mount of `/mnt/data/yocto` on `/dev/nvme0n1p5`.
Absolute paths must stay under `~/yocto/...` or sstate signatures break.

## x86 node

    domain      beamfs-x86-01
    network     default, 192.168.122.99
    user        hpcadmin, key ~/.ssh/hpclab_admin, NOPASSWD sudo
    ssh config  no entry; connect by IP

    vda   6.1G  rootfs, beamfs   sudo virsh domblklist beamfs-x86-01 (moved 2026-09-21
                                 to a chattr +C directory; deploy asks virsh)
    vdb   1G    TEST_DEV
    vdc   1G    SCRATCH_DEV

The host XML calls the third disk `vdh`; the guest calls it `vdc`.
BX takes the guest's names.

## aarch64 nodes

beamfs-master and beamfs-compute01/02/03 on 192.168.56.0/24, master at
.10. These are BX's defaults when XFSTESTS_NODES is unset, which is how
a probe meant for x86 booted an aarch64 node.

## What survives a reboot inside a VM

    /            beamfs on /dev/vda      persists
    /var/lib     same                    persists
    /home        same                    persists
    /tmp         tmpfs on /var/volatile  GONE
    /var/tmp     tmpfs                   GONE

Recovery restarts the domain. Anything a campaign must keep across a
recovery -- the results file above all -- cannot live in /tmp. Two
hours of a run were repeated twice before this was written down.

## Observing a node that has stopped answering

No qemu-guest-agent is installed and no org.qemu.guest_agent channel is
declared in the XML. On a saturated VM sshd cannot negotiate a session
and there is no way in.

What still works, at no cost to the VM:

    /var/log/libvirt/qemu/<domain>-serial.log   and .0, .1 (2 MiB each)

That log holds every kernel message since boot. `virsh console --force`
can trigger a rotation and lose the current file, so read the file, do
not open the console. `virsh console` also needs a controlling TTY --
wrap it in `script -q -c` -- and zombie `virsh console` processes can
hold it.

## Yocto

    build       ~/yocto/poky/build-qemux86
    machine     qemux86-64
    distro      poky-beamfs
    image       beamfs-research-image
    kernel      linux-mainline 7.3-rc5 (PREFERRED_VERSION in conf/local.conf)
    layer       ~/git/yocto-beamfs          (outside ~/yocto)

The kernel takes beamfs sources from
`~/git/yocto-beamfs/recipes-kernel/beamfs/files/beamfs-<version>/` (0.1.26 at
1bf151d), not from
`~/git/beamfs`. rsync the repo into it before bitbake or the change is
simply not in the build.

vmlinux with symbols:

    ~/yocto/poky/build-qemux86/tmp/work/qemux86_64-poky-linux/\
      linux-mainline/7.3-rc5/build/vmlinux

kallsyms rounds a function's size up (0x20cc prints as 0x20d0), which is
how to tell which build an offset in a stack trace belongs to.

## Running a campaign on the x86 node

    export XFSTESTS_NODES="x86-01:192.168.122.99:vdb:vdc"
    export XFSTESTS_MACHINE="qemux86-64"
    export XFSTESTS_IMAGE="beamfs-research-image"
    export XFSTESTS_POKY_DIR="$HOME/yocto/poky"

The node name derives the domain name: `x86-01` gives `beamfs-x86-01`,
`x86` gives `beamfs-x86`, which does not exist. POKY_DIR is the poky
root; BX appends the build directory itself.

737 generic tests. One shard gets SHARD=0 NSHARD=1.

Verdicts are archived as they land under
`~/.local/share/beamfs-xfstests/archive/<commit>/`, so an interrupted
campaign keeps what it proved.
