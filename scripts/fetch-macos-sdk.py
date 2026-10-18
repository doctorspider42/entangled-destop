#!/usr/bin/env python3
"""Install the pinned Vulkan SDK in a new, task-owned directory, never globally."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile


def download(url, archive):
    subprocess.run(['curl', '--fail', '--location', '--retry', '3',
                    '--connect-timeout', '30', '--max-time', '600',
                    '--proto', '=https', '--proto-redir', '=https',
                    '--output', str(archive), url], check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    if output.exists():
        parser.error('output must be a new directory')
    pin = json.loads((Path(__file__).resolve().parent.parent /
                      'installer/macos/vulkan-sdk.json').read_text())
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='entangled-sdk-', dir=output.parent) as temporary:
        work = Path(temporary)
        archive = work / 'sdk.zip'
        download(pin['url'], archive)
        digest = hashlib.sha256()
        with archive.open('rb') as file:
            while chunk := file.read(1024 * 1024):
                digest.update(chunk)
        if digest.hexdigest() != pin['sha256']:
            raise SystemExit('Vulkan SDK SHA-256 mismatch; installer was not executed')
        subprocess.run(['ditto', '-xk', str(archive), str(work / 'unpacked')], check=True)
        name = 'vulkansdk-macOS-' + pin['version']
        installer = work / 'unpacked' / (name + '.app') / 'Contents/MacOS' / name
        try:
            subprocess.run([str(installer), '--root', str(output), '--accept-licenses',
                            '--default-answer', '--confirm-command', 'install',
                            'com.lunarg.vulkan.core'], check=True)
            for name in ['libvulkan.1.dylib', 'libvulkan_kosmickrisp.dylib']:
                if not (output / 'macOS/lib' / name).is_file():
                    raise RuntimeError('SDK did not install required runtime: ' + name)
            (output / 'entangled-sdk-provenance.json').write_text(json.dumps(pin, indent=2) + '\n')
        except BaseException:
            if output.exists():
                shutil.rmtree(output)
            raise
    print(output / 'macOS')


if __name__ == '__main__':
    main()
