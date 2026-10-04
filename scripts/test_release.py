"""Local release regression tests: temporary Git remotes and a fake GitHub API."""

import copy
import hashlib
import os
from pathlib import Path
import plistlib
import tempfile
import unittest
from unittest.mock import patch
import zipfile

from release import CASK, Publisher, archive_checksum, release_plan, render_cask, run


class FakeGitHub:
    def __init__(self):
        self.items = []
        self.archives = {}
        self.uploads = 0
        self.publications = 0
        self.latest = None
        self.stale_listing = False
        self.asset_visibility_delay = 0
        self.release_reads = 0

    def releases(self):
        return [] if self.stale_listing else copy.deepcopy(self.items)

    def create_draft(self, tag, source):
        release = {"id": len(self.items) + 1, "tag_name": tag,
                   "draft": True, "prerelease": False, "assets": []}
        self.items.append(release)
        return copy.deepcopy(release)

    def get_release(self, release_id):
        self.release_reads += 1
        release = copy.deepcopy(next(item for item in self.items if item["id"] == release_id))
        if self.asset_visibility_delay:
            self.asset_visibility_delay -= 1
            release["assets"] = []
        return release

    def upload(self, tag, archive):
        self.uploads += 1
        data = archive.read_bytes()
        self.archives[(tag, archive.name)] = data
        self.find(tag)["assets"].append({
            "name": archive.name,
            "state": "uploaded",
            "size": len(data),
            "digest": "sha256:" + hashlib.sha256(data).hexdigest(),
        })

    def delete_incomplete_asset(self, tag, name):
        self.find(tag)["assets"] = [item for item in self.find(tag)["assets"] if item["name"] != name]

    def download(self, tag, name, directory):
        destination = directory / name
        destination.write_bytes(self.archives[(tag, name)])
        return destination

    def publish(self, tag, latest):
        self.publications += 1
        self.find(tag)["draft"] = False
        if latest:
            self.latest = tag

    def find(self, tag):
        return next(item for item in self.items if item["tag_name"] == tag)


def make_archive(root, version="3.0.0", app_version=None):
    path = root / "target" / "app" / f"Hex-{version}.zip"
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("Hex.app/Contents/Info.plist", plistlib.dumps({
            "CFBundleIdentifier": "dev.publio.hex-openrouter",
            "CFBundleShortVersionString": app_version or version,
        }))
        archive.writestr("Hex.app/Contents/MacOS/hex", b"synthetic executable")
    return path


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        # Tests must never invoke personal signing helpers or credential rewrites.
        self.git_environment = patch.dict(os.environ, {
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_COUNT": "0",
        })
        self.git_environment.start()
        self.addCleanup(self.git_environment.stop)
        self.temporary = tempfile.TemporaryDirectory(prefix="hex-release-test-")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.root = self.directory / "source"
        self.remote = self.directory / "origin.git"
        run("git", "init", "--bare", "--initial-branch=main", str(self.remote))
        run("git", "init", "--initial-branch=main", str(self.root))
        self.git("config", "user.name", "Release Test")
        self.git("config", "user.email", "test@example.invalid")
        (self.root / "Cargo.toml").write_text('[package]\nname="hex"\nversion="3.0.0"\n')
        (self.root / CASK).parent.mkdir()
        (self.root / CASK).write_text('cask "hex-openrouter" do\n  version "2.1.24-4"\nend\n')
        (self.root / ".gitignore").write_text("/target\n")
        self.git("add", ".")
        self.git("commit", "-m", "source")
        self.git("remote", "add", "origin", str(self.remote))
        self.git("push", "origin", "main")
        self.source = self.git("rev-parse", "HEAD").stdout.strip()
        self.archive = make_archive(self.root)
        self.github = FakeGitHub()

    def git(self, *args):
        return run("git", *args, cwd=self.root)

    def publisher(self):
        return Publisher(self.root, "example/hex", self.github)

    def remote_file(self, path):
        return run("git", "--git-dir", str(self.remote), "show", f"main:{path}").stdout

    def advance_main(self, filename, content):
        other = self.directory / "concurrent"
        if not other.exists():
            run("git", "clone", str(self.remote), str(other))
        else:
            run("git", "pull", "--ff-only", cwd=other)
        run("git", "config", "user.name", "Release Test", cwd=other)
        run("git", "config", "user.email", "test@example.invalid", cwd=other)
        path = other / filename
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        run("git", "add", filename, cwd=other)
        run("git", "commit", "-m", "concurrent change", cwd=other)
        run("git", "push", "origin", "main", cwd=other)

    def test_first_release_uses_cargo_version_and_exact_source(self):
        publisher = self.publisher()
        self.assertEqual(publisher.plan(), {"version": "3.0.0", "build": "true", "skip": "false"})
        publisher.publish()
        self.assertEqual(publisher.tag_commit(), self.source)
        self.assertEqual(self.github.latest, "v3.0.0")
        self.assertEqual(self.github.uploads, 1)
        cask = self.remote_file(CASK)
        self.assertIn('version "3.0.0"', cask)
        self.assertIn('/v#{version}/Hex-#{version}.zip', cask)
        self.assertIn(archive_checksum(self.archive, "3.0.0"), cask)

    def test_published_version_skips_build_and_is_not_overwritten(self):
        self.publisher().publish()
        self.archive.unlink()
        self.advance_main("new-code.txt", "same Cargo version, not a new release")
        self.git("fetch", "origin", "main")
        self.git("checkout", "--detach", "FETCH_HEAD")
        before = self.git("rev-parse", "HEAD").stdout
        publisher = self.publisher()
        self.assertEqual(publisher.plan()["build"], "false")
        publisher.publish()
        self.assertEqual(self.github.uploads, 1)
        self.assertEqual(self.github.publications, 1)
        self.assertEqual(self.git("rev-parse", "HEAD").stdout, before)

    def test_rerun_repairs_cask_after_publication_failure(self):
        publisher = self.publisher()
        publisher.update_cask = lambda _: (_ for _ in ()).throw(RuntimeError("push rejected"))
        with self.assertRaisesRegex(RuntimeError, "push rejected"):
            publisher.publish()
        self.archive.unlink()
        self.assertIn('version "2.1.24-4"', self.remote_file(CASK))
        self.publisher().publish()
        self.assertIn('version "3.0.0"', self.remote_file(CASK))
        self.assertEqual(self.github.uploads, 1)
        self.assertEqual(self.github.publications, 1)

    def test_old_rerun_preserves_newer_cask_and_latest(self):
        self.publisher().publish()
        newer = render_cask("3.0.1", "a" * 64, "example/hex")
        self.advance_main(str(CASK), newer)
        self.github.create_draft("v3.0.1", "newer source")
        self.github.publish("v3.0.1", True)
        self.publisher().publish()
        self.assertEqual(self.remote_file(CASK), newer)
        self.assertEqual(self.github.latest, "v3.0.1")
        self.assertEqual(self.github.uploads, 1)

    def test_main_advancing_during_cask_push_keeps_new_source(self):
        publisher = self.publisher()
        original_git = publisher.git
        raced = False

        def git_with_concurrent_push(*args, **kwargs):
            nonlocal raced
            if args == ("push", "origin", "HEAD:refs/heads/main") and not raced:
                raced = True
                self.advance_main("concurrent.txt", "keep this change")
            return original_git(*args, **kwargs)

        publisher.git = git_with_concurrent_push
        publisher.publish()
        self.assertTrue(raced)
        self.assertEqual(self.remote_file("concurrent.txt"), "keep this change")
        self.assertIn('version "3.0.0"', self.remote_file(CASK))
        self.assertEqual(publisher.tag_commit(), self.source)

    def test_draft_resumes_without_replacing_uploaded_asset(self):
        publisher = self.publisher()
        publisher.ensure_source_tag()
        self.github.create_draft("v3.0.0", self.source)
        self.github.upload("v3.0.0", self.archive)
        publisher.publish()
        self.assertEqual(self.github.uploads, 1)
        self.assertEqual(self.github.publications, 1)

    def test_signed_release_rechecks_a_resumed_remote_asset_before_publication(self):
        (self.root / "Cargo.toml").write_text('[package]\nname="hex"\nversion="3.0.5"\n')
        self.git("add", "Cargo.toml")
        self.git("commit", "-m", "signing migration")
        self.git("push", "origin", "main")
        archive = make_archive(self.root, "3.0.5")
        publisher = self.publisher()
        publisher.ensure_source_tag()
        self.github.create_draft("v3.0.5", publisher.source)
        self.github.upload("v3.0.5", archive)
        with patch("release.verify_archive", side_effect=ValueError("wrong signing identity")) as verify:
            with self.assertRaisesRegex(ValueError, "wrong signing identity"):
                publisher.publish()
            verify.assert_called_once()
        self.assertEqual(self.github.publications, 0)
        self.assertTrue(self.github.find("v3.0.5")["draft"])
        self.assertIn('version "2.1.24-4"', self.remote_file(CASK))

    def test_create_response_survives_stale_listing_and_delayed_assets(self):
        self.github.stale_listing = True
        self.github.asset_visibility_delay = 2
        with patch("release.time.sleep") as sleep:
            self.publisher().publish()
        self.assertEqual(self.github.release_reads, 3)
        self.assertEqual(sleep.call_count, 2)
        self.assertEqual(self.github.uploads, 1)
        self.assertEqual(self.github.publications, 1)
        self.assertIn('version "3.0.0"', self.remote_file(CASK))

    def test_asset_visibility_timeout_preserves_draft_and_cask(self):
        self.github.asset_visibility_delay = 10
        with patch("release.time.sleep") as sleep:
            with self.assertRaisesRegex(RuntimeError, "rerun to resume the draft"):
                self.publisher().publish()
        self.assertEqual(self.github.release_reads, 5)
        self.assertEqual(sleep.call_count, 4)
        self.assertEqual(self.github.uploads, 1)
        self.assertEqual(self.github.publications, 0)
        self.assertTrue(self.github.find("v3.0.0")["draft"])
        self.assertIn('version "2.1.24-4"', self.remote_file(CASK))

    def test_tag_collision_rejects_different_source(self):
        self.publisher().ensure_source_tag()
        (self.root / "new-code.txt").write_text("different source")
        self.git("add", ".")
        self.git("commit", "-m", "same version, other source")
        with self.assertRaisesRegex(ValueError, "different source"):
            self.publisher().publish()
        self.assertEqual(self.github.uploads, 0)

    def test_uncommitted_version_cannot_label_an_older_source(self):
        (self.root / "Cargo.toml").write_text('[package]\nname="hex"\nversion="3.0.1"\n')
        make_archive(self.root, version="3.0.1")
        with self.assertRaisesRegex(ValueError, "not committed"):
            self.publisher().publish()
        self.assertEqual(self.github.uploads, 0)

    def test_wrong_bundle_version_never_publishes_or_updates_cask(self):
        make_archive(self.root, app_version="2.9.0")
        with self.assertRaisesRegex(ValueError, "App version"):
            self.publisher().publish()
        self.assertEqual(self.github.publications, 0)
        self.assertIn('version "2.1.24-4"', self.remote_file(CASK))

    def test_published_source_version_must_match_tag(self):
        (self.root / "Cargo.toml").write_text('[package]\nname="hex"\nversion="2.9.0"\n')
        self.git("add", ".")
        self.git("commit", "-m", "wrong version")
        self.git("push", "origin", "HEAD:refs/tags/v3.0.0")
        self.git("checkout", "--detach", self.source)
        self.github.create_draft("v3.0.0", "wrong source")
        self.github.upload("v3.0.0", self.archive)
        self.github.publish("v3.0.0", True)
        with self.assertRaisesRegex(ValueError, "Cargo version"):
            self.publisher().publish()
        self.assertIn('version "2.1.24-4"', self.remote_file(CASK))

    def test_stale_unpublished_version_is_not_allocated_another_number(self):
        newer = {"tag_name": "v3.0.1", "draft": False, "prerelease": False}
        self.assertEqual(release_plan("3.0.0", [newer]),
                         {"version": "3.0.0", "build": "false", "skip": "true"})


if __name__ == "__main__":
    unittest.main()
