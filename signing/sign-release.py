#!/usr/bin/env python3
"""Sign the two Linux binaries and verify the delegated release key."""

import argparse
import datetime as dt
import hashlib
import json
import pathlib
import re
import subprocess
import tempfile


def openssl(*args):
    subprocess.run(["openssl", "pkeyutl", *map(str, args)], check=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("version")
    parser.add_argument("dist", type=pathlib.Path)
    parser.add_argument("signing_key", type=pathlib.Path)
    args = parser.parse_args()

    if not re.fullmatch(r"(0|[1-9][0-9]{0,5})\.(0|[1-9][0-9]{0,5})\.(0|[1-9][0-9]{0,5})", args.version):
        parser.error("expected a stable MAJOR.MINOR.PATCH")
    major, minor, patch = map(int, args.version.split("."))
    revision = major * 10**12 + minor * 10**6 + patch
    if not revision:
        parser.error("expected a nonzero release version")

    signing = pathlib.Path(__file__).resolve().parent
    keyring_path = signing / "keyring.json"
    keyring_sig = signing / "keyring.sig"
    openssl("-verify", "-pubin", "-rawin", "-inkey", signing / "root.pem", "-in", keyring_path, "-sigfile", keyring_sig)
    keyring = json.loads(keyring_path.read_text())
    public_key = subprocess.check_output(["openssl", "pkey", "-pubout", "-in", args.signing_key], text=True)
    matches = [key for key in keyring["keys"] if key["public_key"].strip() == public_key.strip()]
    if len(matches) != 1:
        parser.error("release signing key is not uniquely authorized by the root")
    key = matches[0]
    now = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
    expires_at = min(now + dt.timedelta(days=180), dt.datetime.fromisoformat(key["expires_at"]))
    if expires_at <= now + dt.timedelta(days=7):
        parser.error("rotate the release signing key before publication")

    artifacts = []
    for arch in ("arm64", "amd64"):
        platform = f"linux-{arch}"
        name = f"foreman-{platform}"
        with (args.dist / name).open("rb") as binary:
            digest = hashlib.file_digest(binary, "sha256").hexdigest()
        artifacts.append({"platform": platform, "file": name, "sha256": digest})
    manifest = {
        "schema_version": 1,
        "key_id": key["id"],
        "keyring_revision": keyring["revision"],
        "version": args.version,
        "revision": revision,
        "expires_at": expires_at.isoformat().replace("+00:00", "Z"),
        "artifacts": artifacts,
    }
    metadata = args.dist / "manifest.json"
    signature = args.dist / "manifest.sig"
    metadata.write_text(json.dumps(manifest, indent=2) + "\n")
    openssl("-sign", "-rawin", "-inkey", args.signing_key, "-in", metadata, "-out", signature)
    with tempfile.TemporaryDirectory() as directory:
        pub = pathlib.Path(directory) / "release.pem"
        pub.write_text(key["public_key"])
        openssl("-verify", "-pubin", "-rawin", "-inkey", pub, "-in", metadata, "-sigfile", signature)
    (args.dist / "keyring.json").write_bytes(keyring_path.read_bytes())
    (args.dist / "keyring.sig").write_bytes(keyring_sig.read_bytes())


if __name__ == "__main__":
    main()
