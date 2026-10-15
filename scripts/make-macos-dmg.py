#!/usr/bin/env python3
"""Create a Finder drag-to-Applications installer from a signed .app.

Build-time dependencies: ds_store and mac_alias (MIT licensed).
"""
import argparse
from pathlib import Path
import shutil
import subprocess
import tempfile
from ds_store import DSStore
from mac_alias import Alias


def run(*args):
    result = subprocess.run(args, capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(f"{args[0]} failed: {result.stderr.strip()}")
    return result.stdout


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--app", type=Path, required=True)
    p.add_argument("--background", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--identity", required=True)
    args = p.parse_args()
    if args.output.exists() or args.output.suffix != ".dmg":
        p.error("output must be a new .dmg file")
    run("codesign", "--verify", "--deep", "--strict", str(args.app))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="entangled-dmg-") as tmp:
        work = Path(tmp).resolve()
        stage = work/"stage"
        stage.mkdir()
        run("ditto", str(args.app), str(stage/args.app.name))
        (stage/"Applications").symlink_to("/Applications")
        (stage/".background").mkdir()
        shutil.copy2(args.background, stage/".background/background.png")
        writable = work/"installer.dmg"
        run("hdiutil", "create", "-quiet", "-srcfolder", str(stage), "-volname",
            "Entangled Desktop", "-fs", "HFS+", "-format", "UDRW", str(writable))
        mount = work/"mounted"
        mount.mkdir()
        run("hdiutil", "attach", "-quiet", "-nobrowse", "-mountpoint", str(mount), str(writable))
        try:
            with DSStore.open(str(mount/".DS_Store"), "w+") as layout:
                layout["."]["bwsp"] = {
                    "ShowToolbar": False, "ShowStatusBar": False,
                    "ShowSidebar": False, "ShowTabView": False,
                    "ContainerShowSidebar": False,
                    "SidebarWidth": 0,
                    "WindowBounds": "{{160, 120}, {800, 548}}",
                }
                layout["."]["icvp"] = {
                    "viewOptionsVersion": 1, "backgroundType": 2,
                    "backgroundColorRed": 0.0, "backgroundColorGreen": 0.0,
                    "backgroundColorBlue": 0.0,
                    "backgroundImageAlias": Alias.for_file(str(mount/".background/background.png")).to_bytes(),
                    "iconSize": 128.0, "textSize": 12.0, "arrangeBy": "none",
                    "labelOnBottom": True, "showIconPreview": True,
                    "showItemInfo": False, "gridSpacing": 100.0,
                    "gridOffsetX": 0.0, "gridOffsetY": 0.0,
                }
                layout["."]["vSrn"] = ("long", 1)
                layout["."]["vstl"] = ("type", "icnv")
                layout["."]["icvl"] = ("bool", True)
                layout["."]["icvo"] = ("bool", True)
                layout[args.app.name]["Iloc"] = (220,306)
                layout["Applications"]["Iloc"] = (580,306)
        finally:
            run("hdiutil", "detach", "-quiet", str(mount))
        run("hdiutil", "convert", "-quiet", str(writable), "-format", "UDZO",
            "-imagekey", "zlib-level=9", "-o", str(args.output.absolute()))
    run("codesign", "--sign", args.identity, "--timestamp", str(args.output))
    run("codesign", "--verify", "--strict", str(args.output))
    print(args.output)


if __name__ == "__main__":
    main()
