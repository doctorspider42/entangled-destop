#!/usr/bin/env python3
"""Package already-built native ARM64 binaries for local macOS acceptance.

No downloads, builds or system installation. Public release signing,
notarization and the complete third-party notice audit remain separate gates.
"""
import argparse
import hashlib
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys


def run(*args):
    result = subprocess.run(args, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(f"{args[0]} failed: {result.stderr.strip()}")
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--sdk", type=Path, required=True, help="SDK macOS directory")
    parser.add_argument("--notices", type=Path, required=True)
    parser.add_argument("--identity", default="-", help="codesign identity; '-' for local ad-hoc signing")
    parser.add_argument("--icon", type=Path, help="Application .icns")
    parser.add_argument("--output", type=Path, required=True, help="New .app path")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("packaging and signing require macOS")
    output = args.output.absolute()
    if output.suffix != ".app" or output.exists():
        parser.error("output must be a new .app directory; existing apps are never replaced")
    binaries = {
        "entangled-manager": args.bin_dir / "entangled-manager",
        "entangled-engine": args.bin_dir / "entangled",
    }
    libraries = {
        name: args.sdk / "lib" / name
        for name in ("libvulkan.1.dylib", "libvulkan_kosmickrisp.dylib")
    }
    for source in [*binaries.values(), *libraries.values()]:
        if not source.is_file() or "arm64" not in run("lipo", "-archs", str(source)).split():
            parser.error(f"missing ARM64 Mach-O input: {source}")
        # Refuse a runtime that secretly requires a developer's SDK directory.
        deps = run("otool", "-L", str(source))
        for line in deps.splitlines():
            if not line.startswith("\t"):
                continue
            dependency = line.strip().split(" (", 1)[0]
            if dependency.startswith(("/usr/lib/", "/System/Library/")):
                continue
            if dependency.startswith("@rpath/") and Path(dependency).name in libraries:
                continue
            parser.error(f"unbundled dependency {dependency} in {source}")
    if not args.notices.is_dir() or not any(args.notices.iterdir()):
        parser.error("provide the runtime provenance/license notices directory")
    icd = json.loads((args.sdk / "share/vulkan/icd.d/libkosmickrisp_icd.json").read_text())
    icd["ICD"]["library_path"] = "../../../Frameworks/libvulkan_kosmickrisp.dylib"
    version = run(str(binaries["entangled-engine"]), "--version").strip().split()[-1]
    repository = Path(__file__).resolve().parent.parent
    output.mkdir(parents=True)
    try:
        contents = output / "Contents"
        macos = contents / "MacOS"
        frameworks = contents / "Frameworks"
        resources = contents / "Resources"
        for directory in (macos, frameworks, resources / "vulkan/icd.d"):
            directory.mkdir(parents=True)
        hashes = {}
        for destinations, sources in ((macos, binaries), (frameworks, libraries)):
            for name, source in sources.items():
                shutil.copy2(source, destinations / name)
                hashes[name] = hashlib.sha256(source.read_bytes()).hexdigest()
        # ash opens the unversioned macOS loader name.
        (frameworks / "libvulkan.dylib").symlink_to("libvulkan.1.dylib")
        # Every executable in MacOS is native and signed, including the adapter.
        wrapper = macos / "entangled"
        run("clang", "-arch", "arm64", "-mmacosx-version-min=26.0", "-O2",
            "-Wall", "-Wextra", "-Werror", "-o", str(wrapper),
            str(repository / "installer/macos/engine-launcher.c"))
        (resources / "vulkan/icd.d/kosmickrisp.json").write_text(json.dumps(icd, indent=2) + "\n")
        shutil.copytree(args.notices, resources / "ThirdPartyNotices")
        if args.icon:
            shutil.copy2(args.icon, resources / "Entangled.icns")
        (resources / "build-provenance.json").write_text(json.dumps({
            "version": version, "input_sha256": hashes,
            "signing": args.identity,
        }, indent=2) + "\n")
        with (contents / "Info.plist").open("wb") as file:
            plistlib.dump({
                "CFBundleName": "Entangled Desktop",
                "CFBundleDisplayName": "Entangled Desktop",
                "CFBundleIdentifier": "org.entangled.desktop",
                "CFBundleExecutable": "entangled-manager",
                "CFBundlePackageType": "APPL",
                "CFBundleShortVersionString": version,
                "CFBundleVersion": version,
                "LSMinimumSystemVersion": "26.0",
                "LSArchitecturePriority": ["arm64"],
                "NSHighResolutionCapable": True,
                **({"CFBundleIconFile": "Entangled.icns"} if args.icon else {}),
            }, file)
        def sign(path, entitlements=None):
            command = ["codesign", "--force", "--sign", args.identity]
            if args.identity != "-":
                command += ["--options", "runtime", "--timestamp"]
            if entitlements:
                command += ["--entitlements", str(entitlements)]
            run(*command, str(path))
        for name in libraries:
            sign(frameworks / name)
        sign(macos / "entangled-engine", repository / "installer/macos/engine-entitlements.plist")
        sign(wrapper)
        sign(output)
        run("codesign", "--verify", "--deep", "--strict", str(output))
    except BaseException:
        # This invocation alone created the output, so cleanup cannot touch
        # an earlier app or anyone else's artifacts.
        shutil.rmtree(output)
        raise
    print(output)


if __name__ == "__main__":
    main()
