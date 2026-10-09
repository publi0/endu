import base64
import os
from pathlib import Path
import plistlib
import secrets
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import code_signing as signing


class SigningTests(unittest.TestCase):
    def test_security_errors_stay_private_but_codesign_diagnostics_are_available(self):
        result = subprocess.CompletedProcess([], 1, "", "sensitive import details")
        with patch("code_signing.subprocess.run", return_value=result):
            with self.assertRaises(RuntimeError) as error:
                signing.security("import", "test.p12")
            self.assertNotIn("sensitive import details", str(error.exception))
        result = subprocess.CompletedProcess([], 1, "", "a sealed resource is missing")
        with patch("code_signing.subprocess.run", return_value=result):
            with self.assertRaisesRegex(RuntimeError, "a sealed resource is missing"):
                signing.run("/usr/bin/codesign", "--verify", "test.app")

    def test_passwords_use_stdin_and_cannot_inject_security_commands(self):
        with patch("code_signing.run") as command:
            signing.security("unlock-keychain", "-p", 'secret \\"quoted', "temporary.keychain")
            self.assertEqual(command.call_args.args, ("/usr/bin/security", "-i"))
            self.assertIn("secret", command.call_args.kwargs["input"])
        with self.assertRaises(ValueError):
            signing.security("unlock-keychain", "-p", "secret\nnew-command")

    def test_requirements_reject_adhoc_other_signers_and_broader_rules(self):
        identity = "a" * 40
        expected = signing.expected_requirement(identity)
        signing.check_requirement("designated => " + expected, identity)
        for requirement in (
            'cdhash H"' + identity + '"',
            expected.replace(identity, "b" * 40),
            expected.replace(signing.BUNDLE_ID, "dev.example.other"),
            expected + ' or identifier "anything"',
        ):
            with self.assertRaises(ValueError):
                signing.check_requirement("designated => " + requirement, identity)

    def test_secrets_must_be_complete_and_failures_do_not_echo_them(self):
        for environment in ({}, {"HEX_SIGNING_P12_BASE64": "sensitive-value"},
                            {"HEX_SIGNING_P12_BASE64": "sensitive-value", "HEX_SIGNING_P12_PASSWORD": "password"}):
            with self.assertRaises(ValueError) as error:
                signing.read_secrets(environment)
            self.assertNotIn("sensitive-value", str(error.exception))
            self.assertNotIn("password", str(error.exception))
        self.assertEqual(signing.read_secrets({
            "HEX_SIGNING_P12_BASE64": base64.b64encode(b"fixture").decode(),
            "HEX_SIGNING_P12_PASSWORD": "secret",
        }), (b"fixture", "secret"))

    def test_actions_refuses_adhoc_before_building(self):
        script = Path(__file__).resolve().parent / "build-app.sh"
        for actions in ("true", "false"):
            environment = dict(os.environ, GITHUB_ACTIONS=actions, HEX_CODESIGN_IDENTITY="-")
            result = subprocess.run([str(script)], env=environment, text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("refusing ad hoc signing", result.stderr)


@unittest.skipUnless(sys.platform == "darwin", "requires native macOS signing tools")
class NativeSigningTests(unittest.TestCase):
    def test_two_builds_keep_the_same_identity_and_a_different_key_is_rejected(self):
        before = signing.run("/usr/bin/security", "list-keychains", "-d", "user")
        with tempfile.TemporaryDirectory(prefix="hex-signing-test-") as folder:
            root = Path(folder)
            password = secrets.token_urlsafe(32)
            password_file = root / "password"
            password_file.write_text(password + "\n" + password + "\n")
            identities = []
            archives = []
            for name in ("first", "other"):
                config = root / f"{name}.cnf"
                config.write_text(f"""[req]
distinguished_name=dn
x509_extensions=extensions
prompt=no
[dn]
CN=Hex disposable test {name}
[extensions]
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
""")
                key, cert, p12 = (root / f"{name}.{suffix}" for suffix in ("key", "pem", "p12"))
                signing.run("/usr/bin/openssl", "req", "-new", "-x509", "-newkey", "rsa:2048",
                            "-days", "1", "-config", str(config), "-keyout", str(key),
                            "-out", str(cert), "-passout", f"file:{password_file}")
                signing.run("/usr/bin/openssl", "pkcs12", "-export", "-inkey", str(key),
                            "-in", str(cert), "-out", str(p12), "-passin", f"file:{password_file}",
                            "-passout", f"file:{password_file}")
                identities.append(signing.certificate_identity(cert))
                archives.append(p12.read_bytes())
            hashes = []
            for version, signer in enumerate((0, 0, 1)):
                bundle = root / f"Build{version}" / "Endu.app"
                (bundle / "Contents/MacOS").mkdir(parents=True)
                (bundle / "Contents/Info.plist").write_bytes(plistlib.dumps({
                    "CFBundleIdentifier": signing.BUNDLE_ID,
                    "CFBundleExecutable": "probe", "CFBundlePackageType": "APPL",
                    "CFBundleVersion": str(version),
                }))
                source = root / "main.c"
                source.write_text(f"int main(void) {{ return {version}; }}\n")
                signing.run("/usr/bin/clang", str(source), "-o", str(bundle / "Contents/MacOS/probe"))
                # Each build imports into a new keychain, like separate Actions runs.
                with signing.signing_keychain(archives[signer], password) as keychain:
                    signing.require_identity(keychain, identities[signer])
                    with self.assertRaisesRegex(ValueError, "does not match"):
                        signing.require_identity(keychain, "0" * 40)
                    signing.run("/usr/bin/codesign", "--force", "--sign", identities[signer].upper(),
                                "--keychain", str(keychain), "--timestamp=none", str(bundle))
                self.assertFalse(keychain.exists())
                if signer == 0:
                    signing.verify_bundle(bundle, identities[0])
                    packed = root / f"build{version}.zip"
                    signing.run("/usr/bin/ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(bundle), str(packed))
                    signing.verify_archive(packed, root / "first.pem")
                else:
                    with self.assertRaises(RuntimeError):
                        signing.verify_bundle(bundle, identities[0])
                info = signing.run("/usr/bin/codesign", "-d", "--verbose=4", str(bundle))
                hashes.append(next(line for line in info.splitlines() if line.startswith("CDHash=")))
            self.assertNotEqual(hashes[0], hashes[1])
            with self.assertRaisesRegex(RuntimeError, "failed build"):
                with signing.signing_keychain(archives[0], password) as keychain:
                    raise RuntimeError("failed build")
            self.assertFalse(keychain.exists(), "failed builds must also remove the keychain")
        self.assertEqual(signing.run("/usr/bin/security", "list-keychains", "-d", "user"), before)


if __name__ == "__main__":
    unittest.main()
