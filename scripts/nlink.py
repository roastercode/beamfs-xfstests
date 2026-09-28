#!/usr/bin/env python3
"""Replay nlink.bt for the inodes whose link count fsck disputes.

usage: nlink.py <case dir> <fsck.beamfs>

For each frozen image of the case (vdb.img.zst, the test device;
scratch.img.zst, the scratch device), fsck names the inodes whose
recorded link count differs from the entries naming them. For each,
from the last mount of its device: every event of the trace that
touches it, in time order -- namespace operations, entries added and
removed with their result, link count changes with the operation they
happened in, and the count each write of the inode carried -- then the
first operation after which the names and the in-memory count parted,
and whether the count written last is the one fsck read.
"""
import collections, os, re, subprocess, sys

case, fsck = sys.argv[1], sys.argv[2]
tf = os.path.join(case, 'bpf-nlink.txt')
lines = open(tf, errors='replace').read().split('\n')

m = re.search(r'devs test (\d+) scratch (\d+)', '\n'.join(lines[:40]))
if not m:
    sys.exit(f"{tf}: no 'devs' line; the script never started")
test_dev, scratch_dev = int(m.group(1)), int(m.group(2))
lost = [l for l in lines if 'Lost' in l and 'events' in l]
CTX = {0: '-', 1: 'link', 2: 'unlink', 3: 'rename', 4: 'create',
       5: 'mkdir', 6: 'rmdir', 7: 'symlink'}

ev = []
for l in lines:
    f = l.split(' ')
    t = f[0]
    try:
        if t == 'MOUNT':
            ev.append(dict(t=t, ns=int(f[1]), dev=int(f[2])))
        elif t == 'OP':
            ev.append(dict(t=t, ns=int(f[1]), tid=int(f[2]), dev=int(f[3]), op=f[4],
                           dir=int(f[5]), ino=int(f[6]), len=int(f[7]), name=' '.join(f[8:])))
        elif t == 'RN':
            ev.append(dict(t=t, ns=int(f[1]), tid=int(f[2]), dev=int(f[3]), odir=int(f[4]),
                           oino=int(f[5]), ndir=int(f[6]), nino=int(f[7]), olen=int(f[8]),
                           oname=f[9], nlen=int(f[10]), nname=' '.join(f[11:])))
        elif t == 'OX':
            ev.append(dict(t=t, ns=int(f[1]), tid=int(f[2]), op=f[3], rc=int(f[4])))
        elif t == 'AD':
            ev.append(dict(t=t, ns=int(f[1]), tid=int(f[2]), dev=int(f[3]), dir=int(f[4]),
                           ino=int(f[5]), rc=int(f[6]), len=int(f[7]), name=' '.join(f[8:])))
        elif t == 'DD':
            ev.append(dict(t=t, ns=int(f[1]), tid=int(f[2]), dev=int(f[3]), dir=int(f[4]),
                           rc=int(f[5]), len=int(f[6]), name=' '.join(f[7:])))
        elif t == 'NL':
            ev.append(dict(t=t, ns=int(f[1]), tid=int(f[2]), dev=int(f[3]), ino=int(f[4]),
                           how=f[5], before=int(f[6]), arg=int(f[7]), ctx=int(f[8])))
        elif t == 'WI':
            ev.append(dict(t=t, ns=int(f[1]), tid=int(f[2]), dev=int(f[3]), ino=int(f[4]),
                           nlink=int(f[5]), ctx=int(f[6])))
    except (IndexError, ValueError):
        pass
ev.sort(key=lambda e: e['ns'])
t0 = ev[0]['ns'] if ev else 0
def ts(ns): return f"{(ns - t0) / 1e9:10.3f}s"

print(f"trace : {len(ev)} evenements ; " + ", ".join(
    f"{k} {v}" for k, v in sorted(collections.Counter(e['t'] for e in ev).items())))
print(f"        test dev {test_dev}, scratch dev {scratch_dev}; montages : " +
      ", ".join(f"{ts(e['ns'])} dev {e['dev']}" for e in ev if e['t'] == 'MOUNT'))
if lost:
    print("        PERTES bpftrace : " + " | ".join(lost))

targets = []
for img, dev in (('vdb.img.zst', test_dev), ('scratch.img.zst', scratch_dev)):
    p = os.path.join(case, img)
    if not os.path.exists(p):
        continue
    raw = '/tmp/nlink-' + img[:-4]
    subprocess.run(['zstd', '-dqf', p, '-o', raw], check=True)
    out = subprocess.run([fsck, '--check-only', raw], capture_output=True, text=True)
    s = out.stdout + out.stderr
    found = [(int(a), int(b), int(c)) for a, b, c in
             re.findall(r'inode (\d+) records (\d+) link\(s\), (\d+) entr', s)]
    print(f"\n{img} : fsck rc={out.returncode}, {len(found)} inode(s) dont le compteur ne correspond pas aux entrees")
    for ino, rec, ent in found:
        targets.append((dev, ino, rec, ent, img))

for dev, T, rec, ent, img in targets:
    mounts = [e['ns'] for e in ev if e['t'] == 'MOUNT' and e['dev'] == dev]
    start = mounts[-1] if mounts else 0
    print(f"\n=== {img} : inode {T} enregistre {rec} lien(s), {ent} entree(s) ; "
          f"rejeu depuis le montage a {ts(start)}")
    owner = {}
    names = collections.defaultdict(set)
    nl = {}
    last_wi = None
    open_op = {}
    touched = collections.defaultdict(set)
    story = []
    first_split = None
    for e in ev:
        if e['ns'] < start:
            continue
        if e.get('dev') not in (None, dev):
            continue
        t = e['t']
        if t in ('OP', 'RN'):
            open_op[e['tid']] = e
            touched[e['tid']] = set()
            mine = False
            if t == 'OP':
                mine = e['ino'] == T or owner.get((e['dir'], e['len'], e['name'])) == T
                if mine:
                    story.append(f"{ts(e['ns'])} tid {e['tid']:6d} {e['op']:8s} dir {e['dir']} '{e['name']}' (len {e['len']}) ino {e['ino']}")
            else:
                mine = T in (e['oino'], e['nino'])
                if mine:
                    story.append(f"{ts(e['ns'])} tid {e['tid']:6d} rename   {e['odir']}/'{e['oname']}' (ino {e['oino']}) -> "
                                 f"{e['ndir']}/'{e['nname']}' (ino {e['nino']})")
            if mine:
                touched[e['tid']].add(T)
        elif t == 'AD':
            key = (e['dir'], e['len'], e['name'])
            if e['rc'] == 0:
                owner[key] = e['ino']
                names[e['ino']].add(key)
            if e['ino'] == T:
                touched[e['tid']].add(T)
                story.append(f"{ts(e['ns'])} tid {e['tid']:6d}   add_dirent dir {e['dir']} '{e['name']}' -> {T} rc {e['rc']}"
                             f"  [noms {len(names[T])}]")
        elif t == 'DD':
            key = (e['dir'], e['len'], e['name'])
            who = owner.get(key)
            if e['rc'] == 0 and who is not None:
                owner.pop(key)
                names[who].discard(key)
            if who == T:
                touched[e['tid']].add(T)
                story.append(f"{ts(e['ns'])} tid {e['tid']:6d}   del_dirent dir {e['dir']} '{e['name']}' (nommait {T}) rc {e['rc']}"
                             f"  [noms {len(names[T])}]")
        elif t == 'NL':
            after = {'inc': e['before'] + 1, 'drop': e['before'] - 1,
                     'clear': 0, 'set': e['arg']}[e['how']]
            if e['ino'] == T:
                if e['how'] == 'set' and CTX.get(e['ctx']) in ('create', 'symlink', 'mkdir'):
                    story.append(f"{ts(e['ns'])} tid {e['tid']:6d}   -- inode {T} (re)cree par {CTX[e['ctx']]} --")
                    names[T] = set(k for k in names[T] if owner.get(k) == T)
                note = '' if T not in nl or nl[T] == e['before'] else f"  (attendu avant : {nl[T]})"
                nl[T] = after
                touched[e['tid']].add(T)
                story.append(f"{ts(e['ns'])} tid {e['tid']:6d}   nlink {e['how']:5s} {e['before']} -> {after} dans {CTX.get(e['ctx'], e['ctx'])}{note}")
        elif t == 'WI':
            if e['ino'] == T:
                last_wi = e
                story.append(f"{ts(e['ns'])} tid {e['tid']:6d}   ecrit sur le disque avec nlink {e['nlink']} (dans {CTX.get(e['ctx'], e['ctx'])})")
        elif t == 'OX':
            op = open_op.pop(e['tid'], None)
            if T in touched.pop(e['tid'], set()):
                n_names, n_link = len(names[T]), nl.get(T)
                flag = ''
                if n_link is not None and n_names != n_link:
                    flag = '   <== noms et compteur divergent'
                    if first_split is None:
                        first_split = (e, op, n_names, n_link)
                story.append(f"{ts(e['ns'])} tid {e['tid']:6d} fin {e['op']:8s} rc {e['rc']} : noms {n_names}, nlink {n_link}{flag}")
    for s in story:
        print("  " + s)
    print(f"  --- fin du rejeu : noms {sorted(names[T])} ; nlink en memoire {nl.get(T)} ; "
          f"derniere ecriture {last_wi['nlink'] if last_wi else 'aucune vue'} ; fsck a lu {rec} lien(s), {ent} entree(s)")
    if first_split:
        e, op, a, b = first_split
        print(f"  --- premiere divergence en memoire : fin de {e['op']} (tid {e['tid']}, rc {e['rc']}) a {ts(e['ns'])} : {a} nom(s), nlink {b}")
    elif last_wi and last_wi['nlink'] != rec:
        print(f"  --- en memoire, noms et compteur n'ont jamais diverge ; le disque porte {rec}, la derniere ecriture vue portait {last_wi['nlink']}")
    else:
        print("  --- en memoire, noms et compteur n'ont jamais diverge dans ce qui a ete vu")
