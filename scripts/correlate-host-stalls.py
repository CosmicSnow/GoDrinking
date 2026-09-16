#!/usr/bin/env python3
"""Join numeric host work peaks to observer snapshots; does not infer causality."""
import argparse
import bisect
from collections import Counter
import json
from pathlib import Path

# Only contiguous work intervals on the observed encoder worker. prepare
# subtracts pool time and is not contiguous; send runs on a different thread.
ENCODER_INTERVALS = {'encode', 'encode_convert', 'encode_copy', 'encode_unlock',
                     'encode_submit', 'encode_completion', 'encode_resume'}


def correlate(record, snapshots):
    end = record['max_work_end_ms']
    start = end - record['max_work_us'] / 1000
    times = [r['timestamp_ms'] for r in snapshots]
    left, right = bisect.bisect_left(times, start), bisect.bisect_right(times, end)
    inside = [r for r in snapshots[left:right] if r.get('available')]
    bracket = snapshots[max(0, left - 1):min(len(snapshots), right + 1)]
    ids = {r.get('thread_id') for r in bracket if r.get('available')}
    usable = [r for r in bracket if r.get('available')]
    # Memory is sampled at ~100ms; retain its own timestamp and explicitly
    # bracket the event. These counters include nearby work, not just the peak.
    memories = {r['memory_timestamp_ms']: r for r in snapshots if r.get('memory_available')}
    before = max((t for t in memories if t <= start), default=None)
    after = min((t for t in memories if t >= end), default=None)
    return dict(stage=record['stage'], start_ms=start, end_ms=end,
                wall_ms=record['max_work_us'] / 1000,
                cpu_ms=record['cpu_at_max_work_us'] / 1000 if record.get('cpu_at_max_work_available') else None,
                samples=len(inside), state_counts=dict(Counter(r['state'] for r in inside)),
                flags_counts=dict(Counter(r['flags'] for r in inside)),
                max_observer_gap_ms=max((b['timestamp_ms']-a['timestamp_ms'] for a,b in zip(bracket,bracket[1:])), default=None),
                thread_cpu_delta_raw=(sum(usable[-1][k]-usable[0][k] for k in ('user_time_raw','system_time_raw'))
                                      if len(ids)==1 and len(usable)>=2 else None),
                memory_bracket_ms=[before,after],
                pageins_delta=(memories[after]['pageins']-memories[before]['pageins']
                               if before is not None and after is not None else None))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('artifact',type=Path)
    parser.add_argument('--minimum-ms',type=float,default=50)
    args=parser.parse_args()
    verdict=json.loads((args.artifact/'verdict.json').read_text())
    snapshots=sorted((json.loads(l) for l in (args.artifact/'host-thread.jsonl').read_text().splitlines()),key=lambda r:r['timestamp_ms'])
    events=[]
    for path in (args.artifact/'host').glob('*.jsonl'):
        for line in path.read_text().splitlines():
            r=json.loads(line)
            end=r.get('max_work_end_ms',0)
            start=end-r['max_work_us']/1000
            if r['stage'] in ENCODER_INTERVALS and r['max_work_us'] >= args.minimum_ms*1000 and start >= verdict['start_ms'] and end <= verdict['end_ms']:
                events.append(correlate(r,snapshots))
    events.sort(key=lambda r:r['wall_ms'],reverse=True)
    result=dict(events=events, observer_samples=len(snapshots),
                limits=['State 1 includes running/runnable; this is not a scheduler trace.',
                        'Memory deltas bracket the event at ~100ms resolution and include nearby work.',
                        'Low CPU plus an observed wait does not identify the kernel wait resource.',
                        'thread_cpu_delta_raw is nanoseconds over the surrounding snapshots, not exact event CPU.',
                        'Peak end times have millisecond precision; no exact frame identity is recorded.'])
    (args.artifact/'stall-correlation.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result,indent=2))


if __name__=='__main__':
    main()
