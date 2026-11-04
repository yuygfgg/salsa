"""Check marker coordinates with a nonzero capture sample origin."""

import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

import numpy as np


class MarkerCoordinatesTest(unittest.TestCase):
    def test_device_sample_origin_does_not_change_matching(self):
        with tempfile.TemporaryDirectory(prefix="salsa-markers-") as directory:
            root = pathlib.Path(directory)
            audio = np.zeros((4000, 2), dtype="<f4")
            audio[[1000, 2000, 3000]] = 1
            audio.tofile(root / "capture.raw")
            (root / "markers.csv").write_text(
                "sync,carrier_sample,host_ns,latency_frames,kind\n"
                "0,0,2000000000,0,0\n"
                "1,192000,3000000000,0,1\n"
                "2,384000,4000000000,0,2\n"
            )
            reports = []
            for origin in (0, 10_000_000):
                (root / "meta.json").write_text(json.dumps({
                    "rate": 1000, "channels": 2, "first_sample": origin,
                    "first_host_ns": 1_000_000_000,
                }))
                subprocess.run([
                    sys.executable, str(pathlib.Path(__file__).with_name("measure_markers.py")),
                    "--capture", str(root / "capture.raw"),
                    "--meta", str(root / "meta.json"),
                    "--markers", str(root / "markers.csv"),
                    "--out", str(root / "report.json"),
                ], check=True, capture_output=True, timeout=10)
                reports.append(json.loads((root / "report.json").read_text()))
            self.assertEqual(reports[0], reports[1])
            self.assertEqual(reports[1]["click_count"], 3)
            self.assertEqual(reports[1]["pair_ms"], 1000)
            self.assertEqual(reports[1]["pair_error_ms"], 0)
            self.assertEqual(reports[1]["drift_ms"], 0)
            self.assertEqual(len({m["detected_sample"] for m in reports[1]["markers"]}), 3)


if __name__ == "__main__":
    unittest.main()
