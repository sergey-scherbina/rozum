#!/usr/bin/env python3

import unittest

from memory_correctness_frontier import build_candidates, wilson_lower


class FrontierTests(unittest.TestCase):
    def test_single_pass_is_not_high_confidence(self) -> None:
        self.assertLess(wilson_lower(1, 1), 0.5)
        self.assertGreater(wilson_lower(20, 20), 0.8)

    def test_failed_attempt_costs_memory_time(self) -> None:
        rows = [
            {"agent": "a", "model": "m", "pass": "1", "seconds": "10", "mlx_peak_mb": "1024"},
            {"agent": "a", "model": "m", "pass": "0", "seconds": "10", "mlx_peak_mb": "1024"},
        ]
        candidate = build_candidates(rows, 0.0)[0]
        self.assertEqual(candidate["gib_seconds_per_solve"], 20.0)

    def test_dominated_candidate_is_not_pareto(self) -> None:
        rows = []
        for passed in ("1", "1", "1", "0"):
            rows.append({"agent": "a", "model": "large", "pass": passed, "seconds": "20", "mlx_peak_mb": "2048"})
            rows.append({"agent": "a", "model": "small", "pass": passed, "seconds": "10", "mlx_peak_mb": "1024"})
        by_model = {c["model"]: c for c in build_candidates(rows, 0.0)}
        self.assertTrue(by_model["small"]["pareto"])
        self.assertFalse(by_model["large"]["pareto"])


if __name__ == "__main__":
    unittest.main()
