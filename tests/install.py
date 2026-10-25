#!/usr/bin/env python3
"""Exercise the release installer without network access or changing a real installation."""
import hashlib
import io
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

INSTALLER = Path(__file__).resolve().parents[1] / "scripts/install.sh"


class InstallerSecurity(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="arranger-installer-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        archive = self.root / "fixture.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            for name in ("arranger", "arranger-gui"):
                content = b"#!/bin/sh\nexit 0\n"
                entry = tarfile.TarInfo("arranger-x86_64-unknown-linux-gnu/" + name)
                entry.mode, entry.size = 0o755, len(content)
                tar.addfile(entry, io.BytesIO(content))
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (self.root / "checksum").write_text(digest + "  archive.tar.gz\n")
        self.script("uname", '#!/bin/sh\ncase "$1" in -s) echo Linux ;; -m) echo x86_64 ;; esac\n')
        self.script("curl", """#!/usr/bin/env python3
import os, pathlib, shutil, sys
args = sys.argv[1:]
url = next(a for a in args if a.startswith('https://'))
dst = pathlib.Path(args[args.index('-o') + 1])
root = pathlib.Path(os.environ['INSTALL_FIXTURE'])
if os.environ.get('FAIL_DOWNLOAD') and not url.endswith('.sha256'):
    dst.write_bytes(b'partial archive')
    sys.exit(22)
shutil.copyfile(root / ('checksum' if url.endswith('.sha256') else 'fixture.tar.gz'), dst)
""")
        self.script("tar", '#!/bin/sh\n: > "$INSTALL_FIXTURE/extracted"\nexec ' + shutil.which("tar") + ' "$@"\n')
        self.environ = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
                            INSTALL_FIXTURE=str(self.root), ARRANGER_INSTALL_DIR=str(self.root / "installed"),
                            ARRANGER_VERSION="v-test")

    def script(self, name, content):
        path = self.bin / name
        path.write_text(content)
        path.chmod(0o755)

    def run_install(self):
        return subprocess.run(["sh", str(INSTALLER)], env=self.environ, capture_output=True, text=True)

    def test_verified_archive_installs(self):
        result = self.run_install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / "installed/arranger").is_file())
        self.assertTrue((self.root / "installed/arranger-gui").is_file())

    def test_bad_checksum_never_extracts_or_overwrites(self):
        installed = self.root / "installed"
        installed.mkdir()
        (installed / "arranger").write_text("existing installation")
        (self.root / "checksum").write_text("0" * 64)
        self.assertNotEqual(self.run_install().returncode, 0)
        self.assertFalse((self.root / "extracted").exists())
        self.assertEqual((installed / "arranger").read_text(), "existing installation")

    def test_failed_download_never_extracts(self):
        self.environ["FAIL_DOWNLOAD"] = "1"
        self.assertNotEqual(self.run_install().returncode, 0)
        self.assertFalse((self.root / "extracted").exists())

    def test_version_cannot_change_the_download_path(self):
        self.environ["ARRANGER_VERSION"] = "../../evil"
        self.assertNotEqual(self.run_install().returncode, 0)
        self.assertFalse((self.root / "extracted").exists())


if __name__ == "__main__":
    unittest.main()
