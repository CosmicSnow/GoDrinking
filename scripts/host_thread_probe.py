"""Opt-in numeric macOS snapshots of the host owned by the cadence harness.

Public SDK ABI: sys/proc_info.h proc_threadinfo, libproc.h proc_pidinfo.
TH_STATE_RUNNING includes runnable threads; snapshots are NOT a scheduler trace.
No names, addresses, room data or other processes enter the output.
"""
import ctypes
import json
import threading
import time


class ThreadInfo(ctypes.Structure):
    _fields_ = [('user_time', ctypes.c_uint64), ('system_time', ctypes.c_uint64)] + [
        (n, ctypes.c_int32) for n in ('cpu_usage', 'policy', 'run_state', 'flags',
                                    'sleep_time', 'curpri', 'priority', 'maxpriority')
    ] + [('name', ctypes.c_char * 64)]


class HostThreadProbe:
    def __init__(self, pid, path, memory_reader):
        self.pid, self.path, self.memory_reader = pid, path, memory_reader
        self.lib = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
        self.lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64,
                                         ctypes.c_void_p, ctypes.c_int]
        self.lib.proc_pidinfo.restype = ctypes.c_int
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.run, name='host-observer', daemon=True)
        self.error = None
        self.available_samples = 0

    def info(self, tid):
        info = ThreadInfo()
        size = self.lib.proc_pidinfo(self.pid, 5, tid, ctypes.byref(info), ctypes.sizeof(info))
        return info if size == ctypes.sizeof(info) and 1 <= info.run_state <= 5 else None

    def encoder_threads(self):
        # Bounded list: far above the app's normal thread count. A full list
        # is treated as unavailable rather than claiming complete coverage.
        ids = (ctypes.c_uint64 * 4096)()
        size = self.lib.proc_pidinfo(self.pid, 6, 0, ids, ctypes.sizeof(ids))
        if size <= 0 or size >= ctypes.sizeof(ids):
            return []
        result = []
        for tid in ids[:size // 8]:
            info = self.info(tid)
            if info is not None and info.name == b'golive-encode':
                result.append(tid)
        return result

    def start(self):
        self.thread.start()
        return self

    def close(self):
        self.stop.set()
        self.thread.join(timeout=3)
        if self.thread.is_alive():
            raise RuntimeError('host observer did not stop')
        if self.error:
            raise RuntimeError('host observer failed') from self.error

    def run(self):
        try:
            tids, next_lookup, next_memory = [], 0, 0
            memory = {}
            with self.path.open('w', buffering=65536) as out:
                while not self.stop.is_set():
                    now = time.monotonic()
                    if now >= next_lookup:
                        tids = self.encoder_threads()
                        next_lookup = now + 1
                    if now >= next_memory:
                        memory = self.memory_reader(self.pid)
                        next_memory = now + .1
                    base = dict(timestamp_ms=time.time_ns() // 1_000_000,
                                monotonic_ns=time.monotonic_ns(), threads=len(tids),
                                memory_timestamp_ms=memory.get('timestamp_ms', 0),
                                memory_available=int(memory.get('available', False)),
                                pageins=memory.get('pageins', 0),
                                resident_bytes=memory.get('resident_bytes', 0),
                                physical_bytes=memory.get('physical_bytes', 0))
                    if not tids:
                        out.write(json.dumps(dict(base, available=0)) + '\n')
                    for tid in tids:
                        query_start = time.monotonic_ns()
                        info = self.info(tid)
                        record = dict(base, thread_id=tid, available=int(info is not None))
                        record['query_duration_ns'] = time.monotonic_ns() - query_start
                        if info is not None:
                            self.available_samples += 1
                            record.update(user_time_raw=info.user_time, system_time_raw=info.system_time,
                                          state=info.run_state, flags=info.flags, cpu_usage=info.cpu_usage,
                                          current_priority=info.curpri, base_priority=info.priority)
                        out.write(json.dumps(record) + '\n')
                    self.stop.wait(.01)
        except Exception as error:
            self.error = error
