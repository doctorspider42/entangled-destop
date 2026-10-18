"""Release safety boundaries: never execute an unverified SDK installer."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'fetch_macos_sdk', Path(__file__).resolve().parents[1] / 'fetch-macos-sdk.py')
fetch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fetch)


class SdkReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / 'installer/macos').mkdir(parents=True)
        self.output = self.root / 'sdk'
        self.payload = b'test SDK archive'
        self.pin = {'version': '1.4.363.0', 'url': 'https://vendor.invalid/sdk.zip',
                    'sha256': hashlib.sha256(self.payload).hexdigest()}
        self.source = self.root / 'scripts/fetch-macos-sdk.py'
        self.patchers = [
            patch.object(fetch, '__file__', str(self.source)),
            patch('sys.argv', [str(self.source), '--output', str(self.output)]),
        ]
        for patcher in self.patchers:
            patcher.start()
            self.addCleanup(patcher.stop)

    def write_pin(self):
        (self.root / 'installer/macos/vulkan-sdk.json').write_text(json.dumps(self.pin))

    def test_hash_mismatch_never_executes_installer(self):
        self.pin['sha256'] = '0' * 64
        self.write_pin()
        with patch.object(fetch, 'download', side_effect=lambda url, path: path.write_bytes(self.payload)), \
                patch.object(fetch.subprocess, 'run') as run:
            with self.assertRaisesRegex(SystemExit, 'SHA-256 mismatch'):
                fetch.main()
            run.assert_not_called()
        self.assertFalse(self.output.exists())

    def test_existing_directory_is_never_overwritten(self):
        self.output.mkdir()
        sentinel = self.output / 'kept'
        sentinel.write_text('existing SDK')
        with patch.object(fetch, 'download') as download:
            with self.assertRaises(SystemExit):
                fetch.main()
            download.assert_not_called()
        self.assertEqual(sentinel.read_text(), 'existing SDK')

    def test_missing_runtime_fails_and_removes_only_new_output(self):
        self.write_pin()
        sentinel = self.root / 'kept'
        sentinel.write_text('outside task output')
        def install(command, **kwargs):
            if command[0] != 'ditto':
                self.output.mkdir()
        with patch.object(fetch, 'download', side_effect=lambda url, path: path.write_bytes(self.payload)), \
                patch.object(fetch.subprocess, 'run', side_effect=install):
            with self.assertRaisesRegex(RuntimeError, 'required runtime'):
                fetch.main()
        self.assertFalse(self.output.exists())
        self.assertEqual(sentinel.read_text(), 'outside task output')


if __name__ == '__main__':
    unittest.main()
