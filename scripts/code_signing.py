#!/usr/bin/env python3
"""Sign prepared releases with the pinned Hex identity in a disposable keychain.

Private material comes only from Actions secrets; the repository contains the
public certificate. No trust overrides, login-keychain imports, or TCC changes.
"""

import base64
import argparse
from contextlib import contextmanager
import os
from pathlib import Path
from pathlib import PurePosixPath
import plistlib
import re
import secrets
import stat
import subprocess
import tempfile
import tomllib
import zipfile


BUNDLE_ID = "dev.publio.hex-openrouter"
CERTIFICATE = Path("app/release-signing.pem")


def run(*args, env=None, input=None):
    result = subprocess.run(args, env=env, input=input, text=True, capture_output=True)
    if result.returncode:
        # codesign receives only public fingerprints and bundle/keychain paths.
        # Keep its diagnostics; never print import/keychain output or arguments.
        if args[0] == "/usr/bin/codesign":
            raise RuntimeError(f"codesign failed (exit {result.returncode}): {result.stderr.strip()}")
        raise RuntimeError(f"{Path(args[0]).name} failed (exit {result.returncode})")
    return result.stdout + result.stderr


def security(*args):
    if any(any(character in str(arg) for character in "\0\r\n") for arg in args):
        raise ValueError("Unsupported control character in signing configuration")
    command = " ".join('"' + str(arg).replace('\\', '\\\\').replace('"', '\\"') + '"' for arg in args)
    # Passwords go through stdin, never argv. Output is captured and never logged.
    return run("/usr/bin/security", "-i", input=command + "\n")


def certificate_identity(certificate):
    output = run("/usr/bin/openssl", "x509", "-in", str(certificate),
                 "-noout", "-fingerprint", "-sha1")
    fingerprint = output.strip().split("=")[-1].replace(":", "").lower()
    if not re.fullmatch(r"[0-9a-f]{40}", fingerprint):
        raise ValueError("Cannot read the pinned public signing certificate")
    return fingerprint


def expected_requirement(identity):
    if not re.fullmatch(r"[0-9a-f]{40}", identity):
        raise ValueError("Invalid certificate fingerprint")
    return f'identifier "{BUNDLE_ID}" and certificate leaf = H"{identity}"'


def check_requirement(output, identity):
    match = re.search(r"^(?:# )?designated => (.+)$", output, re.MULTILINE)
    if not match or match[1] != expected_requirement(identity):
        raise ValueError("Release identity changed or is ad hoc; refusing to publish")


def verify_bundle(bundle, identity):
    run("/usr/bin/codesign", "--verify", "--deep", "--strict",
        "-R", "=" + expected_requirement(identity), str(bundle))
    check_requirement(run("/usr/bin/codesign", "-d", "-r-", str(bundle)), identity)


def verify_archive(archive, certificate):
    identity = certificate_identity(certificate)
    with zipfile.ZipFile(archive) as zipped:
        for item in zipped.infolist():
            path = PurePosixPath(item.filename)
            if (path.is_absolute() or ".." in path.parts or "\\" in item.filename or not path.parts
                    or path.parts[0] not in ("Hex.app", "__MACOSX")
                    or stat.S_ISLNK(item.external_attr >> 16)):
                raise ValueError("Unexpected path in release archive")
    with tempfile.TemporaryDirectory(prefix="hex-verify-") as folder:
        run("/usr/bin/ditto", "-x", "-k", str(archive), folder)
        verify_bundle(Path(folder) / "Hex.app", identity)


def read_secrets(environment):
    encoded = environment.get("HEX_SIGNING_P12_BASE64", "")
    password = environment.get("HEX_SIGNING_P12_PASSWORD", "")
    if not encoded or not password:
        raise ValueError("Configure HEX_SIGNING_P12_BASE64 and HEX_SIGNING_P12_PASSWORD in Actions secrets")
    try:
        archive = base64.b64decode(encoded, validate=True)
    except ValueError:
        raise ValueError("The signing archive is not valid base64") from None
    if not archive or len(archive) > 1_000_000:
        raise ValueError("The signing archive is empty or too large")
    return archive, password


@contextmanager
def signing_keychain(archive, password):
    with tempfile.TemporaryDirectory(prefix="hex-signing-", dir=os.environ.get("RUNNER_TEMP")) as folder:
        root = Path(folder)
        keychain = root / "release.keychain-db"
        p12 = root / "identity.p12"
        p12.touch(mode=0o600)
        p12.write_bytes(archive)
        keychain_password = secrets.token_urlsafe(32)
        created = False
        try:
            security("create-keychain", "-p", keychain_password, str(keychain))
            created = True
            security("set-keychain-settings", "-lut", "21600", str(keychain))
            security("unlock-keychain", "-p", keychain_password, str(keychain))
            security("import", str(p12), "-k", str(keychain),
                     "-P", password, "-T", "/usr/bin/codesign")
            p12.unlink()
            security("set-key-partition-list", "-S", "apple-tool:,apple:",
                     "-s", "-k", keychain_password, str(keychain))
            yield keychain
        finally:
            if created:
                security("delete-keychain", str(keychain))


def sign_release(root):
    archive, password = read_secrets(os.environ)
    os.environ.pop("HEX_SIGNING_P12_BASE64", None)
    os.environ.pop("HEX_SIGNING_P12_PASSWORD", None)
    certificate = root / CERTIFICATE
    if not certificate.is_file():
        raise ValueError("Pin the public release certificate in app/release-signing.pem first")
    identity = certificate_identity(certificate)
    bundle = root / "target/app/Hex.app"
    version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        raise ValueError("Invalid package version")
    plist = plistlib.loads((bundle / "Contents/Info.plist").read_bytes())
    if plist.get("CFBundleShortVersionString") != version or plist.get("CFBundleIdentifier") != BUNDLE_ID:
        raise ValueError("Prepare the matching app bundle before signing")
    with signing_keychain(archive, password) as keychain:
        # Compilation has already completed in a separate step without secrets.
        run("/usr/bin/codesign", "--force", "--sign", identity, "--keychain", str(keychain),
            "--timestamp=none", "--entitlements", str(root / "app/VoiceControl.entitlements"), str(bundle))
    verify_bundle(bundle, identity)
    destination = root / f"target/app/Hex-{version}.zip"
    destination.unlink(missing_ok=True)
    run("/usr/bin/ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(bundle), str(destination))
    verify_archive(destination, certificate)
    print("Release signed with the pinned certificate; temporary keychain removed.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify-bundle", type=Path)
    arguments = parser.parse_args()
    try:
        root = Path(__file__).resolve().parent.parent
        if arguments.verify_bundle:
            verify_bundle(arguments.verify_bundle, certificate_identity(root / CERTIFICATE))
        else:
            sign_release(root)
    except (ValueError, RuntimeError) as error:
        raise SystemExit(str(error)) from None
