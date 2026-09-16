import ctypes
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import unittest

spec = importlib.util.spec_from_file_location('host_probe', Path(__file__).resolve().parents[1] / 'host_thread_probe.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


@unittest.skipUnless(sys.platform == 'darwin', 'macOS libproc')
class HostProbeTests(unittest.TestCase):
    def test_owned_named_thread_is_found_and_numeric_samples_stop(self):
        ready, stop = threading.Event(), threading.Event()
        def worker():
            lib = ctypes.CDLL('/usr/lib/libSystem.B.dylib')
            lib.pthread_setname_np.argtypes = [ctypes.c_char_p]
            lib.pthread_setname_np(b'golive-encode')
            ready.set()
            stop.wait(3)
        worker_thread = threading.Thread(target=worker)
        worker_thread.start()
        try:
            self.assertTrue(ready.wait(1))
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / 'probe.jsonl'
                probe = module.HostThreadProbe(os.getpid(), path, lambda _: dict(available=True, pageins=7))
                ids = probe.encoder_threads()
                self.assertEqual(len(ids), 1)
                self.assertEqual(probe.info(ids[0]).name, b'golive-encode')
                probe.start()
                # Wait for discovery using the public probe operation; close
                # flushes the buffer, no concurrent reads of partial JSON.
                probe.stop.wait(.05)
                probe.close()
                rows = [json.loads(line) for line in path.read_text().splitlines()]
                self.assertTrue(rows)
                self.assertTrue(all(row['available'] == 1 for row in rows))
                self.assertTrue(all(row['pageins'] == 7 for row in rows))
                self.assertTrue(all(isinstance(value, (int, float)) for row in rows for value in row.values()))
        finally:
            stop.set()
            worker_thread.join(1)
