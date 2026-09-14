import importlib.util
from pathlib import Path
import struct
import unittest

spec = importlib.util.spec_from_file_location('windows_gui', Path(__file__).resolve().parents[1] / 'check-windows-gui.py')
gui = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gui)


def pe_fixture(subsystem, magic=0x20b):
    data = bytearray(512)
    data[:2] = b'MZ'
    struct.pack_into('<I', data, 0x3c, 128)
    data[128:132] = b'PE\0\0'
    struct.pack_into('<H', data, 148, 240)
    struct.pack_into('<H', data, 152, magic)
    struct.pack_into('<H', data, 220, subsystem)
    return data


class WindowsSubsystemTests(unittest.TestCase):
    def test_gui_and_console_are_distinct_in_pe32_and_pe64(self):
        for magic in (0x10b, 0x20b):
            self.assertEqual(gui.subsystem(pe_fixture(2, magic)), 2)
            self.assertEqual(gui.subsystem(pe_fixture(3, magic)), 3)

    def test_rejects_non_pe_truncated_and_corrupt_headers(self):
        for data in (b'', b'MZ', pe_fixture(2)[:200], bytes(512)):
            with self.assertRaises(ValueError):
                gui.subsystem(data)
        data = pe_fixture(2)
        struct.pack_into('<I', data, 0x3c, 0xffffffff)
        with self.assertRaises(ValueError):
            gui.subsystem(data)
