#!/usr/bin/env python3
"""What the scrubber read and wrote back, against who owned the block.

usage: scrubwb.py <case dir>

Reads <case>/bpf-scrubwb.txt (scripts/scrubwb.bt) and, when present, the
case's dmesg and tree-detail.txt.

For a trace of beamfs 0.1.23 and later (SC lines carry the owner inode
and the logical block): every check the scrubber made, sorted into
skipped (-ESTALE: the owner no longer maps that logical block to that
block), decoded, corrected, written back and uncorrectable; and for every
block decoded and every block written back, the last allocation or free
of that block before that moment. A block decoded or written while its
last allocation named another inode, or after it was freed, is what
known-limitations 3.39 is about, and is reported.

For a trace of 0.1.22 and earlier (seven-field SC lines), the analysis
of 2.3.57: every check against the start of the walk it came from, with
the blocks that changed hands during the walk.
"""
import bisect, collections, os, re, sys

ESTALE, EUCLEAN = -116, -117

case = sys.argv[1]
tf = os.path.join(case, 'bpf-scrubwb.txt')
lines = open(tf, errors='replace').read().split('\n')
m = re.search(r'devs test (\d+) scratch (\d+)', '\n'.join(lines[:40]))
if not m:
    sys.exit(f"{tf}: no 'devs' line; the script never started")
test_dev, scratch_dev = int(m.group(1)), int(m.group(2))
lost = [l for l in lines if 'Lost' in l and 'events' in l]

sw = collections.defaultdict(list)     # tid -> [(ns, dev, blk)]
ev = collections.defaultdict(list)     # (dev, blk) -> [(ns, 'AL'|'FR', ino)]
ts = collections.defaultdict(list)     # (dev, parent) -> [(ns, slot, old, new, ino)]
sc, wb, cr = [], [], []
n = collections.Counter()
v3 = False
for l in lines:
    f = l.split()
    if not f:
        continue
    try:
        t = f[0]
        if t == 'SW':
            sw[int(f[2])].append((int(f[1]), int(f[3]), int(f[4])))
        elif t == 'AL':
            ev[(int(f[2]), int(f[3]))].append((int(f[1]), 'AL', int(f[4])))
        elif t == 'FR':
            ev[(int(f[2]), int(f[3]))].append((int(f[1]), 'FR', int(f[4])))
        elif t == 'TS':
            ts[(int(f[2]), int(f[3]))].append((int(f[1]), int(f[4]), int(f[5]), int(f[6]), int(f[7])))
        elif t == 'SC':
            x = dict(t0=int(f[1]), t1=int(f[2]), tid=int(f[3]), dev=int(f[4]),
                     blk=int(f[5]), corr=int(f[6]), rc=int(f[7]), ino=None, ib=None)
            if len(f) >= 10:
                x['ino'], x['ib'] = int(f[8]), int(f[9])
                v3 = True
            sc.append(x)
        elif t == 'WB':
            wb.append(dict(ns=int(f[1]), tid=int(f[2]), dev=int(f[3]), blk=int(f[4])))
        elif t == 'CR':
            cr.append(dict(ns=int(f[1]), tid=int(f[2]), dev=int(f[3]), blk=int(f[4]),
                           state=int(f[5]) if len(f) > 5 else None))
        else:
            continue
        n[t] += 1
    except (IndexError, ValueError):
        pass
for k in ev:
    ev[k].sort()

allns = [x['t0'] for x in sc] + [e[0] for v in ev.values() for e in v] + [w[0] for v in sw.values() for w in v]
T0 = min(allns) if allns else 0
def s(ns): return f"{(ns - T0) / 1e9:9.3f}s"
def ss(ns): return s(ns).strip()

print("trace : " + ", ".join(f"{k} {v}" for k, v in sorted(n.items())) +
      f" ; test dev {test_dev}, scratch dev {scratch_dev}" +
      (" ; module 0.1.23 ou plus (SC avec proprietaire)" if v3 else " ; module 0.1.22 ou moins"))
if lost:
    print("        PERTES bpftrace : " + " | ".join(lost))

dm = os.path.join(case, 'dmesg')
dmt = open(dm, errors='replace').read() if os.path.exists(dm) else ''
if dmt:
    cnt = [(lbl, len(re.findall(p, dmt))) for lbl, p in (
        ("parity slot empty over", r'parity slot empty over'),
        ("indirect fails its parity (sweep)", r'sweep: indirect block \d+ fails its parity'),
        ("sweep uncorrectable", r'sweep: block \d+ subblock \d+/\d+ uncorrectable'),
        ("leaf under indirect (sweep)", r'is a leaf under indirect'),
        ("LOST POINTER", r'LOST POINTER'))]
    print("dmesg : " + " ; ".join(f"{a} {b}" for a, b in cnt))
lost_parents = {}
for mm in re.finditer(r'LOST POINTER parent=(\d+) slot=(\d+) held (\d+)', dmt):
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


def last_before(key, ns):
    """The last allocation or free of the block strictly before ns."""
    e = ev.get(key, [])
    i = bisect.bisect_left(e, (ns, '', -1))
    return e[i - 1] if i else None


def moves(key, a, b):
    return [e for e in ev.get(key, []) if a <= e[0] <= b]


if v3:
    stale = [x for x in sc if x['rc'] == ESTALE]
    dec = [x for x in sc if x['rc'] != ESTALE]
    corr = [x for x in dec if x['corr'] > 0]
    unc = [x for x in dec if x['rc'] == EUCLEAN]
    other = [x for x in dec if x['rc'] not in (0, EUCLEAN)]
    wear = [x for x in sc if x['ino'] == 0]
    print(f"\nverifications : {len(sc)} ; sautees, plus a ce fichier (-ESTALE) : {len(stale)} ; "
          f"decodees : {len(dec)} (pas d'usure, sans proprietaire : {len(wear)}) ; "
          f"corrigees : {len(corr)} ; reecritures : {len(wb)} ; incorrigibles : {len(unc)} ; "
          f"autres codes : {len(other)} ; relectures du disque : {len(cr)}")

    bad = collections.Counter()
    # Every decode: who held the block when the check began.
    for x in dec:
        if not x['ino']:
            continue
        key = (x['dev'], x['blk'])
        lb = last_before(key, x['t0'])
        why = None
        if lb and lb[1] == 'FR':
            why = f"DECODE D'UN BLOC LIBERE (libere par l'inode {lb[2]} a {ss(lb[0])})"
            bad['dec_free'] += 1
        elif lb and lb[1] == 'AL' and lb[2] != x['ino']:
            why = f"DECODE D'UN BLOC DE L'INODE {lb[2]} (alloue a {ss(lb[0])}), pris pour l'inode {x['ino']}"
            bad['dec_foreign'] += 1
        if why:
            print(f"\n  {s(x['t0'])} bloc {x['blk']} dev {x['dev']}, inode {x['ino']} bloc logique {x['ib']} : rc {x['rc']}, {x['corr']} corrige(s)")
            for e in moves(key, x['t0'] - 30 * 10**9, x['t1']):
                print(f"      {s(e[0])} {'alloue a' if e[1] == 'AL' else 'libere par'} l'inode {e[2]}")
            print(f"      <== {why}")

    # Every write-back: who held the block when it was written.
    by_tid = collections.defaultdict(list)
    for x in sc:
        by_tid[x['tid']].append(x)
    for w in wb:
        key = (w['dev'], w['blk'])
        enc = [x for x in by_tid.get(w['tid'], []) if x['t0'] <= w['ns'] <= x['t1'] and x['blk'] == w['blk']]
        x = enc[0] if enc else None
        lb = last_before(key, w['ns'])
        verdict = "ok"
        if x is None:
            verdict = "HORS D'UNE VERIFICATION"
            bad['wb_orphan'] += 1
        elif lb and lb[1] == 'FR':
            verdict = f"REECRITURE D'UN BLOC LIBERE (par l'inode {lb[2]} a {ss(lb[0])})"
            bad['wb_free'] += 1
        elif lb and lb[1] == 'AL' and lb[2] != x['ino']:
            verdict = f"REECRITURE D'UN BLOC DE L'INODE {lb[2]}, pris pour l'inode {x['ino']}"
            bad['wb_foreign'] += 1
        own = f"inode {x['ino']} bloc logique {x['ib']}, {x['corr']} sous-bloc(s) corrige(s)" if x else "?"
        held = f"dernier evenement : {'alloue a' if lb[1] == 'AL' else 'libere par'} l'inode {lb[2]} a {ss(lb[0])}" if lb else "aucun evenement du bloc dans la trace"
        print(f"\n  {s(w['ns'])} REECRITURE bloc {w['blk']} dev {w['dev']} : {own} ; {held} ; {verdict}")

    for x in unc:
        print(f"\n  {s(x['t1'])} INCORRIGIBLE bloc {x['blk']} dev {x['dev']}, inode {x['ino']} bloc logique {x['ib']}")
        for e in moves((x['dev'], x['blk']), x['t0'] - 30 * 10**9, x['t1']):
            print(f"      {s(e[0])} {'alloue a' if e[1] == 'AL' else 'libere par'} l'inode {e[2]}")
    for x in other:
        print(f"\n  {s(x['t1'])} bloc {x['blk']} dev {x['dev']} : rc {x['rc']} (inode {x['ino']}, bloc logique {x['ib']})")
    for c in cr:
        print(f"\n  {s(c['ns'])} relecture du disque, bloc {c['blk']} dev {c['dev']}")

    corr_nowb = [x for x in corr if not any(w['tid'] == x['tid'] and x['t0'] <= w['ns'] <= x['t1'] for w in wb)]
    print(f"\nbilan : decodages d'un bloc d'un autre inode : {bad['dec_foreign']} ; d'un bloc libere : {bad['dec_free']} ; "
          f"reecritures d'un bloc d'un autre inode : {bad['wb_foreign']} ; d'un bloc libere : {bad['wb_free']} ; "
          f"hors verification : {bad['wb_orphan']} ; "
          f"corrigees sans reecriture (tampon change, proprietaire change, ou sans proprietaire) : {len(corr_nowb)} ; "
          f"sautees (-ESTALE) : {len(stale)}")
    sys.exit(0)

# 0.1.22 and earlier: the 2.3.57 analysis, against the start of the walk.
wbv = [x for x in sc if x['corr'] > 0]
unc = [x for x in sc if x['rc'] != 0]
print(f"\nverifications : {len(sc)} ; reecritures : {len(wbv)} ; incorrigibles : {len(unc)} ; "
      f"relectures de confirmation : {len(cr)}")
hits = collections.Counter()
for x in sc:
    key = (x['dev'], x['blk'])
    walk = [w for w in sw.get(x['tid'], []) if w[0] <= x['t0'] and w[1] == x['dev']]
    parent = walk[-1] if walk else None
    since = parent[0] if parent else x['t0']
    mv = moves(key, since, x['t1'])
    last_alloc = [e for e in ev.get(key, []) if e[1] == 'AL' and e[0] <= x['t1']]
    stores = [st for st in ts.get(key, []) if st[0] <= x['t1']]
    held = [st for st in stores if not last_alloc or st[0] >= last_alloc[-1][0]]
    marks = []
    if mv:
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
    if not marks and x['corr'] == 0:
        continue
    what = (f"reecrit ({x['corr']} sous-bloc(s) corrige(s))" if x['corr'] > 0
            else f"incorrigible (rc {x['rc']})" if x['rc'] != 0 else "verifie, rien a corriger")
    print(f"\n  {s(x['t1'])} bloc {x['blk']} dev {x['dev']} : {what}")
    if parent:
        print(f"      feuille de l'indirect {parent[2]}, marche commencee a {ss(parent[0])} ({(x['t0'] - parent[0]) / 1e9:.1f} s avant)")
    for e in mv:
        print(f"      {s(e[0])} {'alloue a' if e[1] == 'AL' else 'libere par'} l'inode {e[2]}")
    for st in held[-4:]:
        print(f"      {s(st[0])} pointeur pose : slot {st[1]} {st[2]} -> {st[3]} (inode {st[4]})")
    for mk in marks:
        print(f"      <== {mk}")
print(f"\nbilan : {len(wbv)} reecriture(s), {len(unc)} incorrigible(s) ; "
      f"changes de mains pendant la marche : {hits['moved']} ; portaient des pointeurs : {hits['held']} ; "
      f"parents d'un LOST POINTER : {hits['lost']} ; indirects jamais ecrits : {hits['unwritten']}")
