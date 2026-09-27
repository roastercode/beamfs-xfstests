#!/usr/bin/env python3
"""Join undeposited.bt's maps with the blocks fsck names on a frozen image.

usage: undeposited.py <case dir> <fsck.beamfs>

Reads <case dir>/undeposited.trace (bpftrace's maps at exit) and every
<case dir>/*.img.zst; runs fsck --check-only -v on each image; for each
indirect block fsck says no parity describes, prints whether the trace
saw it allocated (level, inode, stack), deposited (parity_slot events),
passed to beamfs_ind_parity_update, freed. Then the wider count: every
indirect allocation the trace saw with no deposit at all.
"""
import re, subprocess, sys, glob, os, collections

case, fsck = sys.argv[1], sys.argv[2]
tf = next((os.path.join(case, n) for n in ('bpf-undeposited.txt', 'undeposited.trace') if os.path.exists(os.path.join(case, n))), None)
assert tf, f"{case}: ni bpf-undeposited.txt ni undeposited.trace"
trace = open(tf, errors='replace').read()

def scalar_map(name):
    d = {}
    for m in re.finditer(r'^@%s\[(\d+)\]: (\d+)$' % name, trace, flags=re.M):
        d[int(m.group(1))] = int(m.group(2))
    return d

def stack_map(name):
    # bpftrace prints "@name[key]: " then one frame per indented line,
    # then a blank line; the exact spacing varies between versions, so
    # the key line is matched loosely and the frames are what follows.
    d = {}
    for m in re.finditer(r'^@%s\[(\d+)\]:[ \t]*\n((?:[ \t]+\S[^\n]*\n)+)' % name, trace, flags=re.M):
        d[int(m.group(1))] = [l.strip() for l in m.group(2).splitlines() if l.strip()]
    if not d and re.search(r'^@%s\[' % name, trace, flags=re.M):
        i = trace.index('@%s[' % name)
        print(f"  (piles {name} : format non lu, extrait :)\n" + trace[i:i+400])
    return d

alloc = scalar_map('alloc'); level = scalar_map('alloc_level'); stack = stack_map('alloc_stack')
dep = scalar_map('dep'); dep_nz = scalar_map('dep_nz'); dep_region = scalar_map('dep_region')
upd = scalar_map('upd'); upd_stack = stack_map('upd_stack'); freed = scalar_map('freed')
alloc_ns = scalar_map('alloc_ns'); alloc_n = scalar_map('alloc_n'); dep_ns = scalar_map('dep_ns'); free_ns = scalar_map('free_ns')
store_ns = scalar_map('store_ns'); store_n = scalar_map('store_n'); store_stack = stack_map('store_stack'); free_stack = stack_map('free_stack')
t0 = min([v for m in (alloc_ns, dep_ns, free_ns, store_ns) for v in m.values()] or [0])
def ms(ns): return f"{(ns - t0)/1e6:9.1f} ms" if ns else "    jamais"
tot = {k: int(v) for k, v in re.findall(r'^@(allocs|deps|upds): (\d+)$', trace, flags=re.M)}
bylevel = re.findall(r'^@allocs_by_level\[(\d+)\]: (\d+)$', trace, flags=re.M)
print(f"trace : {tot.get('allocs',0)} allocations ({', '.join(f'niveau {l}: {n}' for l, n in bylevel)}), "
      f"{tot.get('deps',0)} depots (parity_slot), {tot.get('upds',0)} appels ind_parity_update ; "
      f"{len(alloc)} blocs alloues distincts, {len(dep)} blocs deposes distincts")
undep_ok = [b for b in dep if b not in alloc]
print(f"        blocs deposes jamais vus alloues dans la trace : {len(undep_ok)} (alloues avant l'attache, ou par un chemin sans point de trace)")

def describe(b):
    a = b in alloc
    s = f"  bloc {b:7d} : "
    s += f"alloue (niveau {level.get(b,'?')}, inode {alloc[b]})" if a else "JAMAIS alloue dans la trace"
    s += f" ; parity_slot x{dep.get(b,0)}"
    if b in dep_nz: s += f" (nz={dep_nz[b]}, region {dep_region.get(b)})"
    s += f" ; ind_parity_update x{upd.get(b,0)} ; free x{freed.get(b,0)}"
    print(s)
    ev = sorted([(alloc_ns.get(b,0), f"derniere allocation (x{alloc_n.get(b,0)})"), (dep_ns.get(b,0), f"dernier depot (nz={dep_nz.get(b,'?')})"),
                 (free_ns.get(b,0), "derniere liberation"), (store_ns.get(b,0), f"dernier pointeur installe (x{store_n.get(b,0)})")])
    print("    ordre : " + " -> ".join(f"{ms(t)} {w}" for t, w in ev))
    if store_ns.get(b,0) > dep_ns.get(b,0):
        print("    => pointeur(s) installe(s) APRES le dernier depot : le bloc porte des pointeurs que rien ne decrit")
    if b in store_stack:
        print("    dernier pointeur par : " + " <- ".join(f.split('+')[0] for f in store_stack[b][:9]))
    if b in free_stack:
        print("    libere par        : " + " <- ".join(f.split('+')[0] for f in free_stack[b][:9]))
    if b in stack:
        print("    alloue par : " + " <- ".join(f.split('+')[0] for f in stack[b][:8]))
    if b in upd_stack:
        print("    depose par : " + " <- ".join(f.split('+')[0] for f in upd_stack[b][:8]))

named_all = set()
for img in sorted(glob.glob(os.path.join(case, '*.img.zst'))):
    if 'host' in os.path.basename(img):
        continue
    raw = '/tmp/undeposited-' + os.path.basename(img)[:-4]
    subprocess.run(['zstd', '-dqf', img, '-o', raw], check=True)
    out = subprocess.run([fsck, '--check-only', '-v', raw], capture_output=True, text=True)
    bad = [int(x) for x in re.findall(r'indirect block (\d+) of inode \d+ holds \d+ pointer', out.stdout + out.stderr)]
    print(f"\n=== {os.path.basename(img)} : fsck rc={out.returncode}, {len(bad)} bloc(s) indirect(s) sans parite")
    named_all |= set(bad)
    for b in sorted(bad):
        describe(b)

# the check in the middle of the test: the unmount before the copy, dated
marks = sorted((int(m.group(2)), m.group(1), m.group(3)) for m in re.finditer(r'^([UMP]) (\d+) (\S+)$', trace, flags=re.M))
hist = collections.defaultdict(list)
for m in re.finditer(r'^W (\d+) (\d+) (\d+) \n((?:[ \t]+\S[^\n]*\n)+)', trace, flags=re.M):
    hist[int(m.group(2))].append((int(m.group(1)), int(m.group(3)), " <- ".join(f.strip().split('+')[0] for f in m.group(4).splitlines()[:6])))
um = [t for t, k, w in marks if k == 'U']
print(f"\n=== reperes : " + " ; ".join(f"{ms(t)} {w}" for t, k, w in marks))
# the copy was taken after the unmount that is followed by the longest gap before the next mount
gaps = []
mts = [t for t, k, w in marks if k == 'M']
for u in um:
    nxt = [m for m in mts if m > u]
    gaps.append(((nxt[0] - u) if nxt else 0, u))
check_umount = max(gaps)[1] if gaps else 0
print(f"=== demontage suivi du plus long arret (celui de check, avant la copie) : {ms(check_umount)}")
DATA0, REG0, SLOTS = 18443, 1033, 14
def region_of(b): return REG0 + (b - DATA0) // SLOTS
deps_all = collections.defaultdict(list)
for m in re.finditer(r'^S (\d+) (\d+) (\d+) (\d+) \n((?:[ \t]+\S[^\n]*\n)+)', trace, flags=re.M):
    deps_all[int(m.group(2))].append((int(m.group(1)), int(m.group(3)), int(m.group(4)), " <- ".join(f.strip().split('+')[0] for f in m.group(5).splitlines()[1:4])))
zw = collections.defaultdict(dict)
for m in re.finditer(r'^Z (\d+) (\d+) (\d+)$', trace, flags=re.M):
    zw[int(m.group(2))][int(m.group(1))] = int(m.group(3))
print("=== pour chaque bloc nomme par fsck : tous ses depots, et les ecritures de sa region (mots non nuls / 32) avant ce demontage")
for b in sorted(named_all):
    r = dep_region.get(b) or region_of(b)
    deps_b = dep_ns.get(b, 0)
    ws = [(t, sz, st) for t, sz, st in hist.get(r, []) if t <= check_umount + 5e7]
    stores = store_ns.get(b, 0)
    dl = [d for d in deps_all.get(b, []) if d[0] <= check_umount]
    print(f"  bloc {b} (region {r}) : dernier pointeur {ms(stores)} ; {len(dl)} depot(s) avant le demontage de check, {len(ws)} ecriture(s) de la region")
    ev = [(t, f"DEPOT   nz={nz} par {st}") for t, reg, nz, st in dl] + [(t, f"ECRITURE region, {zw.get(r, {}).get(t, '?')}/32 mots non nuls, par {st}") for t, sz, st in ws]
    for t, w in sorted(ev)[-14:]:
        print(f"      {ms(t)} {w}")
    last_dep = max([d[0] for d in dl] or [0]); last_w = max([w[0] for w in ws] or [0])
    if last_dep and last_w < last_dep:
        print(f"      => dernier depot {ms(last_dep)} sans ecriture de region ensuite avant le demontage {ms(check_umount)}")
    elif last_dep:
        print(f"      => la region a ete ecrite apres le dernier depot ({ms(last_w)}) ; ce qu'elle portait est ci-dessus")
allw = sum(len(v) for v in hist.values()); regw = sum(len(v) for k, v in hist.items() if REG0 <= k < DATA0)
print(f"=== ecritures zone metadonnees : {allw}, dont regions : {regw} ; piles des ecritures de regions :")
for st, n in collections.Counter(st for k, v in hist.items() if REG0 <= k < DATA0 for t, sz, st in v).most_common(6):
    print(f"      {n:6d}  {st}")

print("\n=== piles d'allocation des blocs deposes (indirects surs), les plus frequentes :")
sites_ok = collections.Counter(" <- ".join(f.split('+')[0] for f in stack[b][:7]) for b in alloc if dep.get(b,0) > 0 and b in stack)
for s, n in sites_ok.most_common(6):
    print(f"      {n:5d}  {s}")
print("=== piles d'allocation des blocs que fsck nomme :")
named = set()
for img in sorted(glob.glob(os.path.join(case, '*.img.zst'))):
    if 'host' in os.path.basename(img): continue
    raw = '/tmp/undeposited-' + os.path.basename(img)[:-4]
    out = subprocess.run([fsck, '--check-only', '-v', raw], capture_output=True, text=True)
    named |= {int(x) for x in re.findall(r'indirect block (\d+) of inode \d+ holds \d+ pointer', out.stdout + out.stderr)}
sites_bad = collections.Counter(" <- ".join(f.split('+')[0] for f in stack[b][:7]) for b in named if b in stack)
for s, n in sites_bad.most_common(6):
    print(f"      {n:5d}  {s}")
print(f"    ({len([b for b in named if b not in stack])} des {len(named)} blocs nommes n'ont pas de pile : alloues hors trace)")
late = sorted(b for b in store_ns if store_ns[b] > dep_ns.get(b, 0) and freed.get(b, 0) == 0 or (b in store_ns and store_ns[b] > max(dep_ns.get(b,0), free_ns.get(b,0))))
print(f"\n=== plus large : {len(late)} bloc(s) dont le dernier pointeur installe est posterieur au dernier depot (et non liberes depuis) ; fsck en nomme {len(named)}")
print("    " + ", ".join(str(b) for b in late[:40]))
