#!/usr/bin/env python3
"""Submit/check an Apple notarization request without logging credentials.

Supports the existing ZenX notary.env key names without executing that file.
The private key is opened only by Apple's notarytool. Submission is asynchronous;
the saved request ID is reused for status checks, never resubmitted on timeout.
"""
import argparse
import json
from pathlib import Path
import shlex
import subprocess


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("action", choices=("submit", "status", "staple"))
    p.add_argument("artifact", type=Path)
    p.add_argument("--request", type=Path)
    p.add_argument("--credentials-env", type=Path)
    p.add_argument("--profile", default="zenx-notary")
    args = p.parse_args()
    if args.action == "staple":
        subprocess.run(["xcrun", "stapler", "staple", str(args.artifact)], check=True)
        subprocess.run(["xcrun", "stapler", "validate", str(args.artifact)], check=True)
        return
    if args.request is None:
        p.error("submit/status require a persistent --request file")
    values = {}
    if args.credentials_env:
        for line in args.credentials_env.read_text().splitlines():
            line = line.strip()
            if line.startswith("export "):
                line = line[7:]
            if line.startswith("#") or "=" not in line:
                continue
            name, value = line.split("=", 1)
            if name in ("ZENX_NOTARY_KEY_ID", "ZENX_NOTARY_ISSUER", "ZENX_NOTARY_KEY"):
                words = shlex.split(value, comments=True)
                if len(words) != 1:
                    p.error("notary configuration requires literal single-value assignments")
                values[name] = words[0]
    if values.get("ZENX_NOTARY_KEY_ID") and values.get("ZENX_NOTARY_ISSUER"):
        key = values.get("ZENX_NOTARY_KEY", str(Path.home()/".appstoreconnect/private_keys"/
                           ("AuthKey_"+values["ZENX_NOTARY_KEY_ID"]+".p8")))
        key = key.replace("${HOME}", str(Path.home())).replace("$HOME", str(Path.home()))
        auth = ["--key", str(Path(key).expanduser()), "--key-id", values["ZENX_NOTARY_KEY_ID"],
                "--issuer", values["ZENX_NOTARY_ISSUER"]]
    else:
        auth = ["--keychain-profile", args.profile]
    if args.action == "submit":
        if args.request.exists():
            p.error("request file exists; check that request instead of submitting twice")
        command = ["submit", str(args.artifact)]
    else:
        command = ["info", json.loads(args.request.read_text())["id"]]
    result = subprocess.run(["xcrun", "notarytool", *command, *auth,
                             "--output-format", "json"], capture_output=True, text=True)
    if result.returncode:
        raise SystemExit("Apple notarization command failed: " + result.stderr.strip())
    report = json.loads(result.stdout)
    args.request.parent.mkdir(parents=True, exist_ok=True)
    if args.action == "submit":
        with args.request.open("x") as file:
            json.dump(report, file, indent=2)
    else:
        args.request.write_text(json.dumps(report, indent=2)+"\n")
    print(json.dumps({key:report.get(key) for key in ("id", "status", "message")}, indent=2))


if __name__ == "__main__":
    main()
