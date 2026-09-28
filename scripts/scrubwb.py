#!/usr/bin/env python3
"""What the scrubber wrote back, against who owned the block by then.

usage: scrubwb.py <case dir>

Since 2.3.57 every check is in the trace, and every confirming read (CR)
with the state bits of the buffer it overwrote; a confirming read of a
dirty buffer that had changed hands is reported like a write-back.

Reads <case>/bpf-scrubwb.txt (scripts/scrubwb.bt) and, when present, the
case's dmesg (treecheck LOST POINTER lines) and tree-detail.txt. For every
check_block that corrected -- and therefore wrote its copy back -- or found
a subblock uncorrectable: the leaf it was, the indirect block the walk
took it from and when that walk began, whether the block was freed or
allocated again between the start of the walk and the write, and whether
pointers were stored into it (it was an indirect block) before it was
written over.
"""
import collections, os, re, sys

case = sys.argv[1]
tf = os.path.join(case, 'bpf-scrubwb.txt')
lines = open(tf, errors='replace').read().split('\n')
m = re.search(r'devs test (\d+) scratch (\d+)', '\n'.join(lines[:40]))
if not m:
    sys.exit(f"{tf}: no 'devs' line; the script never started")
test_dev, scratch_dev = int(m.group(1)), int(m.group(2))
lost = [l for l in lines if 'Lost' in l and 'events' in l]

sw = collections.defaultdict(list)     # tid -> [(ns, dev, blk, depth)]
al = collections.defaultdict(list)     # (dev, blk) -> [(ns, ino, level)]
fr = collections.defaultdict(list)     # (dev, blk) -> [(ns, ino)]
ts = collections.defaultdict(list)     # (dev, parent) -> [(ns, slot, old, new, ino)]
sc = []
cr = []
n = collections.Counter()
for l in lines:
    f = l.split()
    if not f:
        continue
    try:
        t = f[0]
        if t == 'SW':
            sw[int(f[2])].append((int(f[1]), int(f[3]), int(f[4]), int(f[5])))
        elif t == 'AL':
            al[(int(f[2]), int(f[3]))].append((int(f[1]), int(f[4]), int(f[5])))
        elif t == 'FR':
            fr[(int(f[2]), int(f[3]))].append((int(f[1]), int(f[4])))
        elif t == 'TS':
            ts[(int(f[2]), int(f[3]))].append((int(f[1]), int(f[4]), int(f[5]), int(f[6]), int(f[7])))
        elif t == 'CR':
            cr.append(dict(t0=int(f[1]), t1=int(f[1]), tid=int(f[2]), dev=int(f[3]),
                           blk=int(f[4]), state=int(f[5]), corr=0, rc=0, cr=True))
        elif t == 'SC':
            sc.append(dict(t0=int(f[1]), t1=int(f[2]), tid=int(f[3]), dev=int(f[4]),
                           blk=int(f[5]), corr=int(f[6]), rc=int(f[7])))
        else:
            continue
        n[t] += 1
    except (IndexError, ValueError):
        pass

allns = [x[0] for v in sw.values() for x in v] + [x['t0'] for x in sc]
t0 = min(allns) if allns else 0
def s(ns): return f"{(ns - t0) / 1e9:9.3f}s"

print(f"trace : " + ", ".join(f"{k} {v}" for k, v in sorted(n.items())) +
      f" ; test dev {test_dev}, scratch dev {scratch_dev}")
if lost:
    print("        PERTES bpftrace : " + " | ".join(lost))

lost_parents = {}
dm = os.path.join(case, 'dmesg')
if os.path.exists(dm):
    for mm in re.finditer(r'LOST POINTER parent=(\d+) slot=(\d+) held (\d+)', open(dm, errors='replace').read()):
        lost_parents.setdefault(int(mm.group(1)), []).append((int(mm.group(2)), int(mm.group(3))))
unwritten = set()
td = os.path.join(case, 'tree-detail.txt')
if os.path.exists(td):
    for mm in re.finditer(r'\[([\d, ]+)\]', open(td).read()):
        unwritten.update(int(x) for x in mm.group(1).replace(',', ' ').split())
if lost_parents:
    print(f"        treecheck LOST POINTER sur les blocs parents : {sorted(lost_parents)}")
if unwritten:
    print(f"        tree-detail, blocs indirects jamais ecrits : {sorted(unwritten)}")

wb = [x for x in sc if x['corr'] > 0]
unc = [x for x in sc if x['rc'] != 0]
dirty_cr = [x for x in cr if (x['state'] >> 1) & 1]
print(f"\nverifications : {len(sc)} ; reecritures : {len(wb)} ; incorrigibles : {len(unc)} ; "
      f"relectures de confirmation : {len(cr)}, dont {len(dirty_cr)} sur un tampon sale")

hits = collections.Counter()
for x in sc + cr:
    key = (x['dev'], x['blk'])
    walk = [w for w in sw.get(x['tid'], []) if w[0] <= x['t0'] and w[3] == 1 and w[1] == x['dev']]
    parent = walk[-1] if walk else None
    since = parent[0] if parent else x['t0']
    moves = [(a[0], 'alloue a', a[1]) for a in al.get(key, []) if since <= a[0] <= x['t1']] + \
            [(f_[0], 'libere par', f_[1]) for f_ in fr.get(key, []) if since <= f_[0] <= x['t1']]
    moves.sort()
    stores = [st for st in ts.get(key, []) if st[0] <= x['t1']]
    last_alloc = [a for a in al.get(key, []) if a[0] <= x['t1']]
    held = [st for st in stores if not last_alloc or st[0] >= last_alloc[-1][0]]
    marks = []
    if moves:
        marks.append("A CHANGE DE MAINS pendant la marche")
        hits['moved'] += 1
    if held:
        marks.append(f"PORTAIT {len(held)} pointeur(s) pose(s) depuis sa derniere allocation")
        hits['held'] += 1
    if x['blk'] in lost_parents:
        marks.append(f"PARENT D'UN LOST POINTER {lost_parents[x['blk']]}")
        hits['lost'] += 1
    if x['blk'] in unwritten:
        marks.append("INDIRECT JAMAIS ECRIT selon tree-detail")
        hits['unwritten'] += 1
    if x.get('cr'):
        dirty = (x['state'] >> 1) & 1
        what = f"RELU DU DISQUE par la confirmation, tampon {'SALE' if dirty else 'propre'} (etat 0x{x['state']:x})"
        if dirty and (moves or held):
            hits['dirty_cr'] += 1
    elif x['corr'] > 0:
        what = f"reecrit ({x['corr']} sous-bloc(s) corrige(s))"
    elif x['rc'] != 0:
        what = f"incorrigible (rc {x['rc']})"
    else:
        what = "verifie, rien a corriger"
    if not marks and x['corr'] == 0 and not x.get('cr'):
        continue
    print(f"\n  {s(x['t1'])} bloc {x['blk']} dev {x['dev']} : {what}")
    if parent:
        print(f"      feuille de l'indirect {parent[2]}, marche commencee a {s(parent[0])} ({(x['t0'] - parent[0]) / 1e9:.1f} s avant)")
    for mv in moves:
        print(f"      {s(mv[0])} {mv[1]} inode {mv[2]}")
    if held:
        for st in held[-4:]:
            print(f"      {s(st[0])} pointeur pose : slot {st[1]} {st[2]} -> {st[3]} (inode {st[4]})")
    for mk in marks:
        print(f"      <== {mk}")

print(f"\nbilan : {len(wb)} reecriture(s), {len(unc)} incorrigible(s), {len(cr)} relecture(s) dont "
      f"{hits['dirty_cr']} d'un tampon sale ayant change de mains ; "
      f"changes de mains pendant la marche : {hits['moved']} ; "
      f"portaient des pointeurs : {hits['held']} ; "
      f"parents d'un LOST POINTER : {hits['lost']} ; indirects jamais ecrits : {hits['unwritten']}")
