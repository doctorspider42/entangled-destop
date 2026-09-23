#!/usr/bin/env python3
"""Regenerate the Rust Venus protocol in crates/virtio-gpu/src/venus/protocol/.

Needs Python 3.8+ with Mako (the only dependency of the vendored
venus-protocol generator) and `rustfmt` on PATH. Keep Mako out of the system
Python; a venv outside the repository is enough:

    # Windows
    python -m venv F:\\venvs\\venus-gen
    F:\\venvs\\venus-gen\\Scripts\\python -m pip install mako
    F:\\venvs\\venus-gen\\Scripts\\python scripts\\venus-gen.py

    # Linux / WSL
    python3 -m venv ~/venvs/venus-gen
    ~/venvs/venus-gen/bin/pip install mako
    ~/venvs/venus-gen/bin/python scripts/venus-gen.py

What it does, in order:

1. runs tools/venus-protocol/rust_protocol.py over rust-selection.txt into
   the protocol directory (every file except the hand-written mod.rs and
   tests.rs is overwritten);
2. checks that mod.rs declares exactly the group modules that were
   generated, since that is the one list a selection change can break;
3. formats the generated files with rustfmt (edition from Cargo.toml).

CI (`.github/workflows/ci.yml`, job `venus-protocol`) runs this and fails on
`git diff --exit-code`, so the checked-in output is always what the pinned
generator produces.

`--check` does all of that into a temporary directory and compares instead
of writing, for a quick local "is it up to date?".
"""

import argparse
import filecmp
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TOOLS = ROOT / 'tools' / 'venus-protocol'
SELECTION = TOOLS / 'rust-selection.txt'
OUTDIR = ROOT / 'crates' / 'virtio-gpu' / 'src' / 'venus' / 'protocol'
HAND_WRITTEN = {'mod.rs', 'tests.rs'}


def edition():
    text = (ROOT / 'Cargo.toml').read_text(encoding='utf-8')
    m = re.search(r'^edition\s*=\s*"(\d+)"', text, re.M)
    return m.group(1) if m else '2021'


def generate(outdir):
    sys.path.insert(0, str(TOOLS))
    try:
        import rust_protocol  # noqa: E402  (needs the path above)
    except ImportError as err:
        sys.exit('venus-gen: %s (is Mako installed in this Python? see --help)' % err)
    try:
        _model, names = rust_protocol.generate(SELECTION, outdir)
    except rust_protocol.Unsupported as err:
        sys.exit('venus-gen: unsupported construct: %s' % err)
    return names


def check_mod_rs(names, mod_rs):
    groups = {n[:-3] for n in names}
    declared = set(re.findall(r'^pub mod (\w+);', mod_rs.read_text(encoding='utf-8'), re.M))
    if groups != declared:
        sys.exit('venus-gen: %s declares modules %s but the generator produced %s; '
                 'update its `pub mod` / `pub use` lines' % (
                     mod_rs, sorted(declared), sorted(groups)))


def rustfmt(paths):
    cmd = ['rustfmt', '--edition', edition()] + [str(p) for p in paths]
    try:
        subprocess.run(cmd, check=True)
    except FileNotFoundError:
        sys.exit('venus-gen: rustfmt not found on PATH')
    except subprocess.CalledProcessError as err:
        sys.exit('venus-gen: rustfmt failed (%s): the generated code does not parse' % err)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--check', action='store_true',
                        help='compare against the checked-in output instead of writing it')
    args = parser.parse_args()

    if args.check:
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            names = generate(tmp)
            check_mod_rs(names, OUTDIR / 'mod.rs')
            rustfmt([tmp / n for n in names])
            stale = [n for n in names
                     if not (OUTDIR / n).exists() or not filecmp.cmp(tmp / n, OUTDIR / n, shallow=False)]
            extra = sorted(p.name for p in OUTDIR.glob('*.rs')
                           if p.name not in HAND_WRITTEN and p.name not in names)
            if stale or extra:
                sys.exit('venus-gen: out of date: %s%s' % (
                    ', '.join(stale), (' ; stale files: ' + ', '.join(extra)) if extra else ''))
            print('venus-gen: %d generated files are up to date' % len(names))
        return

    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        names = generate(tmp)
        check_mod_rs(names, OUTDIR / 'mod.rs')
        rustfmt([tmp / n for n in names])
        for p in OUTDIR.glob('*.rs'):
            if p.name not in HAND_WRITTEN and p.name not in names:
                p.unlink()
        for n in names:
            shutil.copyfile(tmp / n, OUTDIR / n)
    for n in names:
        print('generated', (OUTDIR / n).relative_to(ROOT).as_posix())


if __name__ == '__main__':
    main()
