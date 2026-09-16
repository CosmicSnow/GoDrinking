import importlib.util
from pathlib import Path
import unittest

spec=importlib.util.spec_from_file_location('correlate',Path(__file__).resolve().parents[1]/'correlate-host-stalls.py')
module=importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class CorrelationTests(unittest.TestCase):
    def test_only_contiguous_encoder_work_is_joined_to_encoder_thread(self):
        self.assertNotIn('send', module.ENCODER_INTERVALS)
        self.assertNotIn('encode_prepare', module.ENCODER_INTERVALS)
        self.assertIn('encode_convert', module.ENCODER_INTERVALS)

    def test_work_peak_uses_its_own_timestamp_and_reports_sampling_gaps(self):
        record=dict(stage='encode_convert',max_work_us=60000,max_work_end_ms=200,
                    timestamp_ms=1000,cpu_at_max_work_us=500,cpu_at_max_work_available=1)
        rows=[dict(timestamp_ms=t,available=1,thread_id=7,state=state,flags=0,
                   user_time_raw=t,system_time_raw=0,memory_available=1,
                   memory_timestamp_ms=t,pageins=pages)
              for t,state,pages in [(100,1,3),(150,4,3),(190,4,5),(210,1,5)]]
        result=module.correlate(record,rows)
        self.assertEqual(result['samples'],2)
        self.assertEqual(result['state_counts'],{4:2})
        self.assertEqual(result['cpu_ms'],.5)
        self.assertEqual(result['pageins_delta'],2)
        self.assertEqual(result['max_observer_gap_ms'],50)
        self.assertEqual(result['memory_bracket_ms'],[100,210])

    def test_missing_observer_does_not_claim_no_faults_or_no_wait(self):
        result=module.correlate(dict(stage='encode',max_work_us=100000,max_work_end_ms=300),[])
        self.assertEqual(result['samples'],0)
        self.assertIsNone(result['pageins_delta'])
        self.assertIsNone(result['cpu_ms'])
        self.assertIsNone(result['max_observer_gap_ms'])
