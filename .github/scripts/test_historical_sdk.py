import importlib.util
from pathlib import Path
import unittest


spec = importlib.util.spec_from_file_location("historical_sdk", Path(__file__).with_name("historical-sdk.py"))
sdk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sdk)


class HistoricalResultsTests(unittest.TestCase):
    def test_accepts_the_declared_tests_without_a_fixed_count(self):
        for count in (5, 6):
            listing = "\n".join(f"case_{index}: test" for index in range(count))
            output = f"test result: ok. {count} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s"
            sdk.verify_results(listing, output)

    def test_rejects_incomplete_failed_ignored_and_empty_runs(self):
        listing = "\n".join(f"case_{index}: test" for index in range(5))
        for output in (
            "test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
            "test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
            "test result: FAILED. 4 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out",
            "test result: ok. 4 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out",
            "test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out",
            "",
        ):
            with self.subTest(output=output), self.assertRaises(RuntimeError):
                sdk.verify_results(listing, output)
        with self.assertRaises(RuntimeError):
            sdk.verify_results("", "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out")

    def test_rejects_multiple_summaries(self):
        summary = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
        with self.assertRaises(RuntimeError):
            sdk.verify_results("one: test", summary + "\n" + summary)


if __name__ == "__main__":
    unittest.main()
