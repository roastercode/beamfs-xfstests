#!/usr/bin/env python3
"""Join evictwrite.bt's maps with the inodes fsck still finds on the frozen image.

usage: evictwrite.py <case dir> <fsck.beamfs>

For each inode the trace saw deleted (a block freed under evict_inode)
that fsck still walks on the test device image: its table block, the
time of its last beamfs_write_inode, its last mmb event, and the last
write bio of that table block: before or after the evict, submitted by
whom. Then the same for the deleted inodes fsck no longer finds, as
the control group.
"""
import re, subprocess, sys, glob, os, collections

case, fsck = sys.argv[1], sys.argv[2]
tf = os.path.join(case, 'bpf-evictwrite.txt')
trace = open(tf, errors='replace').read()

def scalar_map(name):
    d = {}
    for m in re.finditer(r'^@%s\[(\d+)\]: (-?\d+)$' % name, trace, flags=re.M):
        d[int(m.group(1))] = int(m.group(2))
    return d

def str_map(name):
    d = {}
    for m in re.finditer(r'^@%s\[(\d+)\]: (.*)$' % name, trace, flags=re.M):
        d[int(m.group(1))] = m.group(2).strip()
    return d

def stack_map(name):
    d = {}
    for m in re.finditer(r'^@%s\[(\d+)\]:[ \t]*\n((?:[ \t]+\S[^\n]*\n)+)' % name, trace, flags=re.M):
        d[int(m.group(1))] = [l.strip() for l in m.group(2).splitlines() if l.strip()]
    return d

wi_n = scalar_map('wi_n'); wi_ns = scalar_map('wi_ns'); wi_sync = scalar_map('wi_sync'); wi_blk = scalar_map('wi_blk'); wi_stack = stack_map('wi_stack')
mmb_ns = scalar_map('mmb_ns'); mmb_how = str_map('mmb_how'); mmb_n = scalar_map('mmb_n'); mmb_err = scalar_map('mmb_err')
alloc = scalar_map('alloc'); free_ns = scalar_map('free_ns'); free_ino = scalar_map('free_ino'); free_stack = stack_map('free_stack')
w_n = scalar_map('w_n'); w_ns = scalar_map('w_ns'); w_size = scalar_map('w_size'); w_stack = stack_map('w_stack')
t0 = min([v for m in (wi_ns, free_ns, w_ns) for v in m.values()] or [0])
def ms(ns): return f"{(ns - t0)/1e6:9.1f} ms" if ns else "   jamais"
def who(st): return " <- ".join(f.split('+')[0] for f in st[:7])

print(f"trace : {sum(wi_n.values())} beamfs_write_inode sur {len(wi_n)} inodes ; {sum(w_n.values())} bios d'ecriture sur {len(w_n)} blocs distincts")
print("        ecritures par pile (4 niveaux) :")
for m in re.finditer(r'^@writes_by\[\n((?:[ \t]+\S[^\n]*\n)+)\]: (\d+)$', trace, flags=re.M):
    print(f"        {int(m.group(2)):6d}  {who([l.strip() for l in m.group(1).splitlines()])}")
for m in re.finditer(r'^@mmb_events\[(.*)\]: (\d+)$', trace, flags=re.M):
    print(f"        mmb {m.group(2):>6} x '{m.group(1)}'")

# the evict, dated by the frees it made
evict_ns = {}
for b, st in free_stack.items():
    if any('evict_inode' in f for f in st):
        # block_free carries ino 0 under evict (the module frees with a
        # NULL owner there): the owner is the last inode the block was
        # allocated to.
        i = free_ino.get(b, 0) or alloc.get(b, 0)
        if i:
            evict_ns[i] = max(evict_ns.get(i, 0), free_ns.get(b, 0))
print(f"        {len(evict_ns)} inodes supprimes (blocs liberes sous evict_inode)")

img = os.path.join(case, 'vdb.img.zst')
raw = '/tmp/evictwrite-vdb.img'
subprocess.run(['zstd', '-dqf', img, '-o', raw], check=True)
out = subprocess.run([fsck, '--check-only', '-v', raw], capture_output=True, text=True)
txt = out.stdout + out.stderr
alive = set(int(x) for x in re.findall(r'inode (\d+)', txt))
still = sorted(i for i in evict_ns if i in alive)
gone = sorted(i for i in evict_ns if i not in alive)
print(f"\nfsck rc={out.returncode} ; supprimes encore vivants sur l'image : {len(still)} ; supprimes disparus : {len(gone)}")

def show(i):
    blk = wi_blk.get(i)
    print(f"  inode {i:5d} : evict {ms(evict_ns[i])} ; dernier write_inode {ms(wi_ns.get(i,0))} (x{wi_n.get(i,0)}, sync={wi_sync.get(i,'?')}) sur bloc de table {blk}")
    if i in mmb_ns:
        print(f"              dernier mmb {ms(mmb_ns[i])} : {mmb_how.get(i)} n={mmb_n.get(i)} err={mmb_err.get(i)}")
    if blk is not None:
        if blk in w_ns:
            when = "APRES" if w_ns[blk] > evict_ns[i] else "AVANT"
            print(f"              derniere ecriture du bloc {blk} : {ms(w_ns[blk])} ({when} l'evict), x{w_n.get(blk,0)}, {w_size.get(blk,0)} octets, par {who(w_stack.get(blk, []))}")
        else:
            print(f"              bloc {blk} : AUCUNE ecriture vue dans la trace")
    if i in wi_stack:
        print(f"              write_inode par : {who(wi_stack[i])}")

print("\n=== supprimes encore vivants (le defaut) :")
for i in still[:12]:
    show(i)
print("\n=== supprimes disparus (temoins) :")
for i in gone[:5]:
    show(i)

# the history of the metadata zone: every write of the table blocks of
# a few wrongly alive inodes, in order, with mount and unmount marks
marks = sorted((int(m.group(2)), m.group(1), m.group(3)) for m in re.finditer(r'^([UMP]) (\d+) (\S+)$', trace, flags=re.M))
hist = collections.defaultdict(list)
for m in re.finditer(r'^W (\d+) (\d+) (\d+) \n((?:[ \t]+\S[^\n]*\n)+)', trace, flags=re.M):
    hist[int(m.group(2))].append((int(m.group(1)), int(m.group(3)), who([l.strip() for l in m.group(4).splitlines()])))
print(f"\n=== reperes : " + " ; ".join(f"{ms(t)} {w}" for t, k, w in marks))
print(f"=== historique des ecritures, zone de metadonnees : {sum(len(v) for v in hist.values())} ecritures sur {len(hist)} blocs")
shown = 0
for blk in sorted(hist):
    inos = [i for i in still if wi_blk.get(i) == blk]
    if not inos or shown >= 3:
        continue
    shown += 1
    last_evict = max(evict_ns[i] for i in inos)
    print(f"  bloc {blk} (inodes vivants a tort {inos[:6]}..., dernier evict {ms(last_evict)}) :")
    for t, sz, st in hist[blk]:
        flag = "APRES evict" if t > last_evict else "avant"
        print(f"      {ms(t)} {sz:5d} o {flag:12s} {st}")
print(f"=== ecritures de la zone apres le dernier demontage :")
last_umount = max([t for t, k, w in marks if k == 'U'] or [0])
after = [(t, blk, sz, st) for blk, v in hist.items() for t, sz, st in v if t > last_umount]
print(f"    {len(after)} ecriture(s) apres {ms(last_umount)}")
for t, blk, sz, st in sorted(after)[:20]:
    print(f"      {ms(t)} bloc {blk:5d} {sz:5d} o  {st}")

# what each write of the first table blocks carried: the modes of the
# inodes fsck still finds, at each write, against their evict
content = collections.defaultdict(list)
for m in re.finditer(r'^C (\d+) (\d+) ((?:\d+ ?){16})$', trace, flags=re.M):
    content[int(m.group(2))].append((int(m.group(1)), [int(x) for x in m.group(3).split()]))
print(f"\n=== contenu porte par les ecritures des blocs de table (i_mode des 16 inodes, 256 o/inode) : {sum(len(v) for v in content.values())} ecritures lues")
raw = '/tmp/evictwrite-vdb.img'
f = open(raw, 'rb')
for blk in sorted(content)[:4]:
    inos = [i for i in still if wi_blk.get(i) == blk]
    if not inos: continue
    print(f"  bloc {blk} : inodes vivants a tort {inos}")
    for i in inos[:4]:
        slot = (i - 1) % 16
        f.seek(blk * 4096 + slot * 256); disk_mode, = __import__('struct').unpack('<H', f.read(2))
        line = [f"{ms(t)} mode=0o{modes[slot]:o}{' <' if t > evict_ns[i] else ''}" for t, modes in content[blk]]
        print(f"    inode {i:5d} (evict {ms(evict_ns[i])}) : " + " ; ".join(line[-4:]) + f" ; sur l'image : 0o{disk_mode:o}")
        after = [modes[slot] for t, modes in content[blk] if t > evict_ns[i]]
        if after:
            print(f"        => la derniere ecriture apres l'evict portait mode=0o{after[-1]:o} ; l'image porte 0o{disk_mode:o} : " + ("le DISQUE n'a pas garde ce qui a ete envoye" if after[-1] == 0 and disk_mode != 0 else "le TAMPON portait deja l'ancien inode" if after[-1] == disk_mode else "autre cas"))

# the page at completion, and the reads of the same block around the write
done = collections.defaultdict(list)
for m in re.finditer(r'^D (\d+) (\d+) (-?\d+) ((?:\d+ ?){16})$', trace, flags=re.M):
    done[int(m.group(2))].append((int(m.group(1)), int(m.group(3)), [int(x) for x in m.group(4).split()]))
reads = collections.defaultdict(list)
for m in re.finditer(r'^R (\d+) (\d+) (\d+) \n((?:[ \t]+\S[^\n]*\n)+)', trace, flags=re.M):
    reads[int(m.group(2))].append((int(m.group(1)), int(m.group(3)), who([l.strip() for l in m.group(4).splitlines()])))
print(f"\n=== a l'achevement des ecritures ({sum(len(v) for v in done.values())} lues) et lectures de la zone ({sum(len(v) for v in reads.values())}) :")
for blk in sorted(content)[:3]:
    inos = [i for i in still if wi_blk.get(i) == blk]
    if not inos: continue
    i = inos[0]; slot = (i - 1) % 16
    print(f"  bloc {blk}, inode {i} (evict {ms(evict_ns[i])}) :")
    events = [(t, 'SUBMIT ', f"mode=0o{modes[slot]:o}") for t, modes in content[blk]] + \
             [(t, 'DONE   ', f"mode=0o{modes[slot]:o} status={st}") for t, st, modes in done[blk]] + \
             [(t, 'READ   ', f"{sz} o par {st}") for t, sz, st in reads[blk]]
    for t, k, w in sorted(events):
        if t > evict_ns[i] - 2e9:
            print(f"      {ms(t)} {k} {w}")
print("=== lectures de la zone de metadonnees, par pile :")
rs = collections.Counter(st for v in reads.values() for t, sz, st in v)
for st, n in rs.most_common(8):
    print(f"      {n:6d}  {st}")

# by table block: how many still-alive inodes share a block, and was that block written after the last of their evicts
byblk = collections.defaultdict(list)
for i in still:
    if i in wi_blk: byblk[wi_blk[i]].append(i)
print("\n=== par bloc de table :")
for blk, inos in sorted(byblk.items()):
    last_evict = max(evict_ns[i] for i in inos)
    w = w_ns.get(blk, 0)
    print(f"  bloc {blk} : inodes vivants a tort {inos} ; derniere ecriture {ms(w)} ({'APRES' if w > last_evict else 'AVANT'} le dernier evict a {ms(last_evict)}) par {who(w_stack.get(blk, []))}")
