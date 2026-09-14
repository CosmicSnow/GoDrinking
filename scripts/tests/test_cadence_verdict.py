import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('cadence', Path(__file__).resolve().parents[1] / 'check-viewer-cadence.py')
cadence = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cadence)


class CadenceVerdictTests(unittest.TestCase):
    def setUp(self):
        self.stages = {name: dict(seconds=28, fps=60, instances=1, errors=0,
            dropped=0, max_gap_ms=25, gpu_frames=1680) for name in (
                'host.encode', 'host.send', 'viewer.decode', 'viewer.draw', 'viewer.present',
                'viewer2.decode', 'viewer2.draw', 'viewer2.present')}

    def test_complete_two_viewer_run_passes(self):
        self.assertEqual(cadence.verdict_failures(self.stages, 2, 30), [])

    def test_rejects_stalls_even_at_sixty_fps(self):
        self.stages['viewer2.present']['max_gap_ms'] = 98
        self.assertTrue(cadence.verdict_failures(self.stages, 2, 30))

    def test_missing_short_slow_failed_and_cpu_only_runs_fail(self):
        for field, value in [('seconds', 1), ('fps', 30), ('errors', 1), ('dropped', 1), ('gpu_frames', 0)]:
            stages = copy.deepcopy(self.stages)
            stages['viewer.draw'][field] = value
            with self.subTest(field=field):
                self.assertTrue(cadence.verdict_failures(stages, 2, 30))
        del self.stages['viewer2.present']
        self.assertTrue(cadence.verdict_failures(self.stages, 2, 30))

    def test_duplicate_encoders_fail(self):
        self.stages['host.encode']['instances'] = 2
        self.assertTrue(cadence.verdict_failures(self.stages, 2, 30))
