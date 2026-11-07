#!/usr/bin/env python3
"""Compare an official LeanExport file with a lev-export directory.

    python3 -I scripts/compare_exports.py OFFICIAL.ndjson LEVDIR [--jobs N] [--examples K]

N is the number of processes, including the one that reads the official file.

Both sides get canonical per-declaration hashes: SHA-256 Merkle hashes that ignore binder names,
binder info, mdata, the letE nondep flag, file indices, record order and key order, and keep
universe parameter names and order, constant names, literals and every declaration field.
Inductive blocks and the quotient package are hashed as units, and each of their constants gets
the unit's hash.

The report counts declarations only in the official export, only in lev-export, and in both with
different hashes. It also checks the lev-export files: dense indices, the first and last lines,
the hidden lists, that each declaration comes after the declarations of the same file it
references, and that every referenced constant is declared by some file. Exit status 1 means a
hash mismatch or a failed check.
"""

import argparse
import hashlib
import json
import os
import sys
import time
from multiprocessing import get_context

sha256 = hashlib.sha256
ANON = sha256(b'N').digest()
ZERO = sha256(b'L0').digest()


def H(*parts):
    h = sha256()
    for p in parts:
        h.update(len(p).to_bytes(8, 'little'))
        h.update(p)
    return h.digest()


def B(b):
    return b'1' if b else b'0'


def D(n):
    return str(n).encode()


def utf8(s):
    return s.encode('utf-8', 'surrogatepass')


def short(h):
    return int.from_bytes(h[:8], 'little')


class Reader:
    """Canonical hashes of the records of one export file."""

    def __init__(self, lev=False):
        self.lev = lev
        self.names = [ANON]
        self.text = ['']
        self.internal = [False]
        self.levels = [ZERO]
        self.exprs = bytearray()
        self.decls = []  # (name index, kind, unit hash, type hash)
        self.quot = []  # (name index, member hash, type hash)
        self.ndecl = 0  # declaration records so far
        self.declared = {}  # name index -> declaration ordinal
        self.first_ref = {}  # name index -> ordinal when first referenced by a const node
        self.lit_ref = {}  # 'nat' / 'str' -> ordinal at the first literal of that kind
        self.meta = None
        self.hidden = None
        self.lines = 0
        self.errors = []

    def e(self, i):
        if not 0 <= i < len(self.exprs) // 32:
            raise ValueError(f'expression {i} is not defined yet')
        return bytes(self.exprs[32 * i:32 * i + 32])

    def lp(self, idxs):
        return b''.join(self.names[i] for i in idxs)

    def read(self, f):
        loads = json.loads
        names, levels, X = self.names, self.levels, self.exprs
        for line in f:
            self.lines += 1
            o = loads(line)
            if 'ie' in o:
                i = o.pop('ie')
                (k, v), = o.items()
                n = len(X) // 32
                if i != n:
                    raise ValueError(f'line {self.lines}: expression index {i}, expected {n}')
                if k == 'app':
                    a, b = 32 * v['fn'], 32 * v['arg']
                    if max(a, b) >= len(X):
                        raise ValueError(f'line {self.lines}: forward reference')
                    d = sha256(b'Ea' + X[a:a + 32] + X[b:b + 32]).digest()
                elif k == 'const':
                    c = v['name']
                    d = sha256(b'Ec' + names[c] + b''.join(levels[u] for u in v['us'])).digest()
                    if self.lev and c not in self.declared and c not in self.first_ref:
                        self.first_ref[c] = self.ndecl
                elif k == 'forallE' or k == 'lam':
                    a, b = 32 * v['type'], 32 * v['body']
                    if max(a, b) >= len(X):
                        raise ValueError(f'line {self.lines}: forward reference')
                    d = sha256((b'Ep' if k == 'forallE' else b'El') + X[a:a + 32] + X[b:b + 32]).digest()
                elif k == 'bvar':
                    d = sha256(b'Eb' + D(v)).digest()
                elif k == 'sort':
                    d = sha256(b'Es' + levels[v]).digest()
                elif k == 'letE':
                    d = sha256(b'Ez' + self.e(v['type']) + self.e(v['value']) + self.e(v['body'])).digest()
                elif k == 'proj':
                    d = sha256(b'Ej' + names[v['typeName']] + self.e(v['struct']) + D(v['idx'])).digest()
                elif k == 'natVal':
                    d = sha256(b'En' + v.encode()).digest()
                    self.lit_ref.setdefault('nat', self.ndecl)
                elif k == 'strVal':
                    d = sha256(b'Et' + utf8(v)).digest()
                    self.lit_ref.setdefault('str', self.ndecl)
                elif k == 'mdata':
                    d = self.e(v['expr'])
                else:
                    raise ValueError(f'line {self.lines}: unknown expression {k}')
                X += d
            elif 'in' in o:
                i = o.pop('in')
                (k, v), = o.items()
                if i != len(names):
                    raise ValueError(f'line {self.lines}: name index {i}, expected {len(names)}')
                pre = v['pre']
                if k == 'str':
                    s = v['str']
                    names.append(sha256(b'Ns' + names[pre] + utf8(s)).digest())
                    self.internal.append(self.internal[pre] or s.startswith('_'))
                else:
                    s = str(v['i'])
                    names.append(sha256(b'Nn' + names[pre] + s.encode()).digest())
                    self.internal.append(self.internal[pre])
                self.text.append(s if pre == 0 else self.text[pre] + '.' + s)
            elif 'il' in o:
                i = o.pop('il')
                (k, v), = o.items()
                if i != len(levels):
                    raise ValueError(f'line {self.lines}: level index {i}, expected {len(levels)}')
                if k == 'succ':
                    levels.append(sha256(b'Ls' + levels[v]).digest())
                elif k == 'max' or k == 'imax':
                    levels.append(sha256((b'Lm' if k == 'max' else b'Li') + levels[v[0]] + levels[v[1]]).digest())
                elif k == 'param':
                    levels.append(sha256(b'Lp' + names[v]).digest())
                else:
                    raise ValueError(f'line {self.lines}: unknown level {k}')
            else:
                self.record(o)
        self.finish()

    def th(self, c):
        return H(b'T', self.names[c['name']], self.lp(c['levelParams']), self.e(c['type']))

    def declare(self, members):
        for name, kind, unit, th in members:
            if name in self.declared:
                self.errors.append(f'{self.text[name]} declared twice')
            self.declared[name] = self.ndecl
            self.decls.append((name, kind, unit, th))
        self.ndecl += 1

    def record(self, o):
        if self.lines == 1 and 'meta' in o:
            self.meta = o
            return
        if 'lev' in o:
            self.hidden = o['lev']['hidden']
            return
        (k, v), = o.items()
        N, E, lp = self.names, self.e, self.lp
        if k == 'thm':
            unit = H(b'thm', N[v['name']], lp(v['levelParams']), E(v['type']), E(v['value']), lp(v['all']))
            self.declare([(v['name'], k, unit, self.th(v))])
        elif k == 'def':
            hints = v['hints']
            hints = utf8(hints) if isinstance(hints, str) else b'regular:' + D(hints['regular'])
            unit = H(b'def', N[v['name']], lp(v['levelParams']), E(v['type']), E(v['value']), hints,
                     utf8(v['safety']), lp(v['all']))
            kind = 'def' if v['safety'] == 'safe' else 'def:' + v['safety']
            self.declare([(v['name'], kind, unit, self.th(v))])
        elif k == 'axiom':
            unit = H(b'axiom', N[v['name']], lp(v['levelParams']), E(v['type']), B(v['isUnsafe']))
            self.declare([(v['name'], k, unit, self.th(v))])
        elif k == 'opaque':
            unit = H(b'opaque', N[v['name']], lp(v['levelParams']), E(v['type']), E(v['value']),
                     lp(v['all']), B(v['isUnsafe']))
            self.declare([(v['name'], k, unit, self.th(v))])
        elif k == 'quot':
            member = H(b'quot', N[v['name']], lp(v['levelParams']), E(v['type']), utf8(v['kind']))
            self.quot.append((v['name'], member, self.th(v)))
            self.declared[v['name']] = self.ndecl
            self.ndecl += 1
        elif k == 'inductive':
            types, ctors, recs, members = [], [], [], []
            for t in v['types']:
                types.append(H(b'type', N[t['name']], lp(t['levelParams']), E(t['type']), D(t['numParams']),
                               D(t['numIndices']), lp(t['all']), lp(t['ctors']), D(t['numNested']),
                               B(t['isRec']), B(t['isReflexive']), B(t['isUnsafe'])))
                members.append((t['name'], 'inductive', self.th(t)))
            for c in v['ctors']:
                ctors.append(H(b'ctor', N[c['name']], lp(c['levelParams']), E(c['type']), N[c['induct']],
                               D(c['cidx']), D(c['numParams']), D(c['numFields']), B(c['isUnsafe'])))
                members.append((c['name'], 'ctor', self.th(c)))
            for r in v['recs']:
                rules = b''.join(H(N[x['ctor']], D(x['nfields']), E(x['rhs'])) for x in r['rules'])
                recs.append(H(b'rec', N[r['name']], lp(r['levelParams']), E(r['type']), lp(r['all']),
                              D(r['numParams']), D(r['numIndices']), D(r['numMotives']), D(r['numMinors']),
                              rules, B(r['k']), B(r['isUnsafe'])))
                members.append((r['name'], 'rec', self.th(r)))
            unit = H(b'inductive', H(*sorted(types)), H(*sorted(ctors)), H(*sorted(recs)))
            self.declare([(n, kind, unit, th) for n, kind, th in members])
        else:
            raise ValueError(f'line {self.lines}: unknown record {k}')

    def finish(self):
        if self.quot:
            unit = H(b'quot', *sorted(m for _, m, _ in self.quot))
            for name, _, th in self.quot:
                self.decls.append((name, 'quot', unit, th))
        if not self.lev:
            return
        for c, first in self.first_ref.items():
            if c in self.declared and self.declared[c] > first:
                self.errors.append(f'{self.text[c]} is referenced before its declaration')
        top = {self.text[c]: c for c in self.declared}
        for kind, deps in (('nat', ['Nat']), ('str', ['Char.ofNat', 'String.ofList'])):
            for dep in deps:
                if kind in self.lit_ref and dep in top and self.declared[top[dep]] >= self.lit_ref[kind]:
                    self.errors.append(f'a {kind} literal comes before {dep}')

    def table(self):
        """(name hash, text, kind, unit hash, type hash, internal) for every declared constant."""
        return [(self.names[n], self.text[n], kind, unit, th, self.internal[n])
                for n, kind, unit, th in self.decls]


def scan_lev(job):
    path, module, is_module = job
    r = Reader(lev=True)
    try:
        with open(path, encoding='utf-8') as f:
            r.read(f)
    except Exception as ex:  # report and keep going
        return {'module': module, 'fatal': f'{type(ex).__name__}: {ex}'}
    errors = list(r.errors)
    lev = (r.meta or {}).get('lev', {})
    if r.meta is None or 'meta' not in r.meta or lev.get('exporter') != 'lev-export/1' or lev.get('module') != module:
        errors.append('bad first line')
    hidden_kinds = {}
    kind_of = {n: kind for n, kind, _, _ in r.decls}
    if r.hidden is None:
        errors.append('no hidden line')
    else:
        for n in r.hidden:
            if n not in kind_of:
                errors.append(f'hidden name {n} is not declared')
            else:
                hidden_kinds[kind_of[n]] = hidden_kinds.get(kind_of[n], 0) + 1
        hidden = set(r.hidden)
        if not is_module and hidden:
            errors.append('hidden constants in a file that is not a module')
        if is_module:
            for n, kind, _, _ in r.decls:
                if kind == 'thm' and n not in hidden:
                    errors.append(f'theorem {r.text[n]} is not hidden')
    foreign = {short(r.names[c]) for c in r.first_ref if c not in r.declared}
    return {'module': module, 'decls': r.table(), 'errors': errors, 'lines': r.lines,
            'bytes': os.path.getsize(path), 'hidden': hidden_kinds, 'foreign': foreign}


def find_names(job):
    path, wanted = job
    r = Reader(lev=True)
    with open(path, encoding='utf-8') as f:
        r.read(f)
    return [r.text[c] for c in r.first_ref if short(r.names[c]) in wanted]


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('official')
    ap.add_argument('levdir')
    ap.add_argument('--jobs', type=int, default=min(16, os.cpu_count() or 1))
    ap.add_argument('--examples', type=int, default=10)
    args = ap.parse_args()
    t0 = time.time()
    with open(os.path.join(args.levdir, 'manifest.json'), encoding='utf-8') as f:
        manifest = json.load(f)
    jobs, absent = [], []
    for m in manifest['modules']:
        path = os.path.join(args.levdir, m['file'])
        if os.path.exists(path):
            jobs.append((path, m['name'], m['isModule']))
        else:
            absent.append(m['name'])
    jobs.sort(key=lambda j: -os.path.getsize(j[0]))
    ctx = get_context('fork')
    with ctx.Pool(max(1, args.jobs - 1)) as pool:  # the main process reads the official file
        pending = pool.map_async(scan_lev, jobs, chunksize=1)
        official = Reader()
        with open(args.official, encoding='utf-8') as f:
            official.read(f)
        t1 = time.time()
        results = pending.get()
        t2 = time.time()

        off = {}
        for key, text, kind, unit, th, internal in official.table():
            off[key] = (text, kind, unit, th, internal)
        lev = {}
        errors, fatal = [f'official: {e}' for e in official.errors], []
        lines = size = 0
        hidden = {}
        foreign = set()
        for res in results:
            if 'fatal' in res:
                fatal.append(f"{res['module']}: {res['fatal']}")
                continue
            lines += res['lines']
            size += res['bytes']
            errors += [f"{res['module']}: {e}" for e in res['errors']]
            for kind, n in res['hidden'].items():
                hidden[kind] = hidden.get(kind, 0) + n
            foreign |= res['foreign']
            for key, text, kind, unit, th, internal in res['decls']:
                lev.setdefault(key, []).append((res['module'], text, kind, unit, th, internal))
        declared = {short(k) for k in lev}
        missing = foreign - declared
        missing_names = []
        if missing:
            for names in pool.map(find_names, [(j[0], missing) for j in jobs]):
                missing_names += names

    k = args.examples
    only_off = [v for key, v in off.items() if key not in lev]
    only_lev = [copies[0] for key, copies in lev.items() if key not in off]
    differ, dup, dup_differ = [], [], 0
    for key, copies in lev.items():
        if len(copies) > 1:
            dup.append(copies[0])
            if len({c[3] for c in copies}) > 1:
                dup_differ += 1
        if key in off and off[key][2] not in {c[3] for c in copies}:
            differ.append((off[key], copies))

    def kinds(rows, i):
        out = {}
        for r in rows:
            out[r[i]] = out.get(r[i], 0) + 1
        return dict(sorted(out.items()))

    print(f'official: {official.lines} lines, {len(off)} constants, {official.ndecl} declaration records ({t1 - t0:.0f} s)')
    print(f'lev-export: {len(results)} files, {size} bytes, {lines} lines, {sum(len(c) for c in lev.values())} '
          f'constants ({len(lev)} distinct names); {len(absent)} manifest modules without a file ({t2 - t0:.0f} s)')
    print(f'in both with equal hashes: {sum(1 for key in lev if key in off) - len(differ)}')
    print(f'in both with different hashes: {len(differ)}')
    for (text, kind, unit, th, _), copies in differ[:k]:
        same_type = any(c[4] == th for c in copies)
        print(f'  {text} ({kind}) in {", ".join(c[0] for c in copies)}; type hash {"equal" if same_type else "different"}')
    print(f'only in the official export: {len(only_off)} {kinds(only_off, 1)}')
    for text, kind, *_ in only_off[:k]:
        print(f'  {text} ({kind})')
    internal = [c for c in only_lev if c[5]]
    other = [c for c in only_lev if not c[5]]
    print(f'only in lev-export: {len(only_lev)}, internal names {len(internal)}, other {len(other)} {kinds(other, 2)}')
    for module, text, kind, *_ in other[:k]:
        print(f'  {text} ({kind}) in {module}')
    print(f'names declared by several files: {len(dup)} {kinds(dup, 2)}, with different copies: {dup_differ}')
    print(f'hidden: {sum(hidden.values())} {dict(sorted(hidden.items()))}')
    print(f'referenced but declared by no file: {len(missing)}')
    for text in sorted(set(missing_names))[:k]:
        print(f'  {text}')
    print(f'file errors: {len(fatal) + len(errors)}')
    for e in (fatal + errors)[:k]:
        print(f'  {e}')
    return 1 if differ or missing or fatal or errors else 0


if __name__ == '__main__':
    sys.exit(main())
