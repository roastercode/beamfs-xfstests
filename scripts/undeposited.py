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

for img in sorted(glob.glob(os.path.join(case, '*.img.zst'))):
    if 'host' in os.path.basename(img):
        continue
    raw = '/tmp/undeposited-' + os.path.basename(img)[:-4]
    subprocess.run(['zstd', '-dqf', img, '-o', raw], check=True)
    out = subprocess.run([fsck, '--check-only', '-v', raw], capture_output=True, text=True)
    bad = [int(x) for x in re.findall(r'indirect block (\d+) of inode \d+ holds \d+ pointer', out.stdout + out.stderr)]
    print(f"\n=== {os.path.basename(img)} : fsck rc={out.returncode}, {len(bad)} bloc(s) indirect(s) sans parite")
    for b in sorted(bad):
        describe(b)

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
