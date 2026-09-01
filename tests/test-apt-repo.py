#!/usr/bin/env python3
import os
import shutil
import subprocess
import tempfile
import unittest


class TestAptRepoGeneration(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.mkdtemp(prefix="agent-meter-apt-test-")
        self.input_dir = os.path.join(self.temp_dir, "dist")
        self.output_dir = os.path.join(self.temp_dir, "apt-repo")
        self.gpg_home = os.path.join(self.temp_dir, "gpghome")
        os.makedirs(self.input_dir, exist_ok=True)
        os.makedirs(self.gpg_home, mode=0o700, exist_ok=True)

        # Create a mock/fixture .deb package
        self.deb_path = os.path.join(self.input_dir, "agent-meter_0.1.0_amd64.deb")
        self._create_mock_deb(self.deb_path, version="0.1.0")

    def tearDown(self):
        shutil.rmtree(self.temp_dir, ignore_errors=True)

    def _create_mock_deb(self, deb_path: str, version: str = "0.1.0"):
        deb_stage = tempfile.mkdtemp(prefix="deb-stage-")
        os.chmod(deb_stage, 0o755)
        try:
            debian_dir = os.path.join(deb_stage, "DEBIAN")
            os.makedirs(debian_dir, mode=0o755, exist_ok=True)
            os.chmod(debian_dir, 0o755)
            control_content = (
                f"Package: agent-meter\n"
                f"Version: {version}\n"
                f"Section: utils\n"
                f"Priority: optional\n"
                f"Architecture: amd64\n"
                f"Maintainer: Agent Meter contributors\n"
                f"Description: Local GNOME usage meter for coding-agent harnesses\n"
            )
            with open(os.path.join(debian_dir, "control"), "w", encoding="utf-8") as f:
                f.write(control_content)

            usr_bin = os.path.join(deb_stage, "usr", "bin")
            os.makedirs(usr_bin, exist_ok=True)
            with open(os.path.join(usr_bin, "agent-meter"), "w", encoding="utf-8") as f:
                f.write("#!/bin/sh\necho 0.1.0\n")
            res = subprocess.run(
                ["dpkg-deb", "--root-owner-group", "--build", deb_stage, deb_path],
                capture_output=True,
                text=True,
            )
            if res.returncode != 0:
                raise RuntimeError(f"dpkg-deb failed: {res.stderr}")
        finally:
            shutil.rmtree(deb_stage, ignore_errors=True)

    def _generate_test_gpg_key(self) -> str:
        batch_config = (
            "Key-Type: RSA\n"
            "Key-Length: 2048\n"
            "Subkey-Type: RSA\n"
            "Subkey-Length: 2048\n"
            "Name-Real: Agent Meter Archive Automatic Signing Key\n"
            "Name-Email: archive@agent-meter.local\n"
            "Expire-Date: 0\n"
            "%no-protection\n"
            "%commit\n"
        )
        env = os.environ.copy()
        env["GNUPGHOME"] = self.gpg_home
        subprocess.run(
            ["gpg", "--batch", "--gen-key"],
            input=batch_config.encode("utf-8"),
            env=env,
            check=True,
            capture_output=True,
        )
        export_proc = subprocess.run(
            ["gpg", "--batch", "--armor", "--export-secret-keys", "archive@agent-meter.local"],
            env=env,
            check=True,
            capture_output=True,
            text=True,
        )
        return export_proc.stdout

    def test_repo_generation_and_signing_verification(self):
        signing_key = self._generate_test_gpg_key()

        script_path = os.path.join(
            os.path.dirname(__file__), "..", "packaging", "generate-apt-repo.sh"
        )
        env = os.environ.copy()
        env["APT_SIGNING_KEY"] = signing_key

        proc = subprocess.run(
            [script_path, self.input_dir, self.output_dir, "amd64"],
            env=env,
            capture_output=True,
            text=True,
        )
        if proc.returncode != 0:
            self.fail(f"generate-apt-repo.sh failed ({proc.returncode}):\nSTDOUT: {proc.stdout}\nSTDERR: {proc.stderr}")

        # Check directory layout
        pool_deb = os.path.join(
            self.output_dir, "pool", "main", "a", "agent-meter", "agent-meter_0.1.0_amd64.deb"
        )
        self.assertTrue(os.path.exists(pool_deb), "deb should be in pool")

        packages_file = os.path.join(
            self.output_dir, "dists", "stable", "main", "binary-amd64", "Packages"
        )
        packages_gz = os.path.join(
            self.output_dir, "dists", "stable", "main", "binary-amd64", "Packages.gz"
        )
        release_file = os.path.join(self.output_dir, "dists", "stable", "Release")
        inrelease_file = os.path.join(self.output_dir, "dists", "stable", "InRelease")
        release_gpg = os.path.join(self.output_dir, "dists", "stable", "Release.gpg")
        keyring_file = os.path.join(self.output_dir, "agent-meter-archive-keyring.gpg")

        self.assertTrue(os.path.exists(packages_file), "Packages file must exist")
        self.assertTrue(os.path.exists(packages_gz), "Packages.gz file must exist")
        self.assertTrue(os.path.exists(release_file), "Release file must exist")
        self.assertTrue(os.path.exists(inrelease_file), "InRelease file must exist")
        self.assertTrue(os.path.exists(release_gpg), "Release.gpg file must exist")
        self.assertTrue(os.path.exists(keyring_file), "Keyring file must exist")

        with open(packages_file, "r", encoding="utf-8") as f:
            pkgs_content = f.read()
        self.assertIn("Package: agent-meter", pkgs_content)
        self.assertIn("Version: 0.1.0", pkgs_content)
        self.assertIn("Filename: pool/main/a/agent-meter/agent-meter_0.1.0_amd64.deb", pkgs_content)

        with open(release_file, "r", encoding="utf-8") as f:
            rel_content = f.read()
        self.assertIn("Origin: Agent Meter", rel_content)
        self.assertIn("Suite: stable", rel_content)
        self.assertIn("Codename: stable", rel_content)
        self.assertIn("Architectures: amd64", rel_content)
        self.assertIn("SHA256:", rel_content)
        self.assertIn("main/binary-amd64/Packages", rel_content)

        # Verify gpgv accepts InRelease and Release.gpg using the exported archive keyring
        gpgv_inrelease = subprocess.run(
            ["gpgv", "--keyring", keyring_file, inrelease_file],
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            gpgv_inrelease.returncode,
            0,
            f"gpgv should accept InRelease: {gpgv_inrelease.stderr}",
        )

        gpgv_release = subprocess.run(
            ["gpgv", "--keyring", keyring_file, release_gpg, release_file],
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            gpgv_release.returncode,
            0,
            f"gpgv should accept Release.gpg: {gpgv_release.stderr}",
        )
    def test_repo_generation_multiple_versions_retention(self):
        self._create_mock_deb(
            os.path.join(self.input_dir, "agent-meter_0.1.1_amd64.deb"),
            version="0.1.1",
        )
        signing_key = self._generate_test_gpg_key()
        script_path = os.path.join(
            os.path.dirname(__file__), "..", "packaging", "generate-apt-repo.sh"
        )
        env = os.environ.copy()
        env["APT_SIGNING_KEY"] = signing_key

        subprocess.run(
            [script_path, self.input_dir, self.output_dir, "amd64"],
            env=env,
            check=True,
            capture_output=True,
        )

        packages_file = os.path.join(
            self.output_dir, "dists", "stable", "main", "binary-amd64", "Packages"
        )
        with open(packages_file, "r", encoding="utf-8") as f:
            content = f.read()

        self.assertIn("Version: 0.1.0", content)
        self.assertIn("Version: 0.1.1", content)


if __name__ == "__main__":
    unittest.main()
