import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest


RELEASE_SCRIPT = Path(__file__).resolve().with_name("fzed-release")


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.repository = self.root / "repository"
        self.repository.mkdir()
        self.remote = self.root / "remote.git"
        commands = self.root / "bin"
        commands.mkdir()
        self.environment = {
            **os.environ,
            "PATH": f"{commands}{os.pathsep}{os.environ['PATH']}",
            "TMPDIR": str(self.root),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
        }
        self.write_command(commands / "cargo", "exit 1")
        self.write_command(commands / "gh", r"""
            case "$1 $2" in
              'api --paginate')
                if [ "${API_STATUS:-0}" != 0 ]; then
                  echo 'GitHub unavailable' >&2
                  exit "$API_STATUS"
                fi
                printf '%s\n' "${PUBLISHED_TAGS:-}"
                ;;
              'release view') exit 1 ;;
              'run list') echo 123 ;;
              'run watch') exit "${CI_STATUS:-0}" ;;
              'release create')
                git ls-remote --exit-code --tags origin "refs/tags/$3"
                exit "${PUBLISH_STATUS:-0}"
                ;;
              *) echo "Unexpected gh call: $*" >&2; exit 1 ;;
            esac
        """)
        self.git("init", "-q", "--initial-branch=main")
        self.git("config", "user.name", "Release test")
        self.git("config", "user.email", "test@example.com")
        self.git("init", "-q", "--bare", str(self.remote))
        self.git("remote", "add", "origin", str(self.remote))
        (self.repository / "crates/zed").mkdir(parents=True)
        (self.repository / "script").mkdir()
        (self.repository / ".gitignore").write_text("target/\n")
        self.write_command(self.repository / "script/bundle-mac", """
            exit_status="${BUILD_STATUS:-0}"
            if [ "$exit_status" != 0 ]; then exit "$exit_status"; fi
            mkdir -p "target/$1/release"
            touch "target/$1/release/FZed-aarch64.dmg"
        """)

    def write_command(self, path, body):
        path.write_text("#!/usr/bin/env bash\nset -eu\n" + textwrap.dedent(body))
        path.chmod(0o755)

    def git(self, *arguments):
        return subprocess.check_output(
            ["git", *arguments], cwd=self.repository, env=self.environment, text=True
        ).strip()

    def release(self, version, published_tags="", **environment):
        (self.repository / "crates/zed/Cargo.toml").write_text(
            f'[package]\nname = "zed"\nversion = "{version}"\n'
        )
        self.git("add", ".")
        if self.git("diff", "--cached", "--name-only"):
            self.git("commit", "-qm", "Prepare release")
        return subprocess.run(
            ["bash", str(RELEASE_SCRIPT), "--summary", "Test release",
             "--target", "aarch64-apple-darwin"],
            cwd=self.repository,
            env={**self.environment, "PUBLISHED_TAGS": published_tags, **environment},
            text=True,
            capture_output=True,
        )

    def assert_no_tags(self):
        self.assertEqual(self.git("tag"), "")
        self.assertEqual(self.git("ls-remote", "--tags", "origin"), "")

    def test_new_upstream_requires_zero_before_pushing(self):
        result = self.release("1.18.1-fzed.1", "v1.17.2-fzed.9")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("expected 1.18.1-fzed.0", result.stderr)
        self.assertEqual(self.git("ls-remote", "origin"), "")
        self.assert_no_tags()

    def test_first_release_and_upstream_reset(self):
        for published_tags in ("", "v1.17.2-fzed.9\nv1.18.0-fzed.2"):
            with self.subTest(published_tags=published_tags):
                result = self.release("1.18.1-fzed.0", published_tags)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(self.git("tag"), "v1.18.1-fzed.0")
                self.git("tag", "-d", "v1.18.1-fzed.0")
                self.git("push", "-q", "origin", ":refs/tags/v1.18.1-fzed.0")

    def test_followup_uses_highest_published_suffix(self):
        result = self.release(
            "1.18.1-fzed.11",
            "v1.18.1-fzed.10\nv1.19.0-fzed.0\nv1.18.1-fzed.9\nv1.18.1-fzed.invalid",
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_existing_history_without_zero_can_continue(self):
        result = self.release("1.18.1-fzed.3", "v1.18.1-fzed.2\nv1.18.1-fzed.1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_skipped_and_reused_suffixes_are_rejected(self):
        for revision in (0, 1, 3):
            with self.subTest(revision=revision):
                result = self.release(f"1.18.1-fzed.{revision}", "v1.18.1-fzed.1")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("expected 1.18.1-fzed.2", result.stderr)
                self.assertEqual(self.git("ls-remote", "origin"), "")

    def test_api_failure_stops_before_pushing(self):
        result = self.release("1.18.1-fzed.0", API_STATUS="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("could not read published releases", result.stderr)
        self.assertEqual(self.git("ls-remote", "origin"), "")
        self.assert_no_tags()

    def test_build_and_ci_failures_leave_version_available_for_retry(self):
        for failure in ("BUILD_STATUS", "CI_STATUS"):
            with self.subTest(failure=failure):
                result = self.release("1.18.1-fzed.0", **{failure: "1"})
                self.assertNotEqual(result.returncode, 0)
                self.assert_no_tags()
        (self.repository / "build-fix").write_text("Fix the release build\n")
        result = self.release("1.18.1-fzed.0")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_failed_publication_reuses_the_unpublished_tag(self):
        result = self.release("1.18.1-fzed.0", PUBLISH_STATUS="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.git("tag"), "v1.18.1-fzed.0")
        result = self.release("1.18.1-fzed.0")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_unpublished_tag_does_not_advance_the_suffix(self):
        result = self.release("1.18.1-fzed.0", PUBLISH_STATUS="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.git("tag"), "v1.18.1-fzed.0")
        result = self.release("1.18.1-fzed.1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("expected 1.18.1-fzed.0", result.stderr)


if __name__ == "__main__":
    unittest.main()
