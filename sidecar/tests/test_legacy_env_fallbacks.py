"""The pre-rename key name still works for one release, behind the new name.
Deleted together with the fallback. Run: python -m unittest discover sidecar/tests
"""

import os
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import score_posts  # noqa: E402

NAMES = ("CLASSIFIER_API_KEY", "CLASSIFIER_URL", "CLASSIFIER_MODEL", "TYPESAFE_API_KEY")


def _env(**values):
    clean = {k: v for k, v in os.environ.items() if k not in NAMES}
    clean.update(values)
    return mock.patch.dict(os.environ, clean, clear=True)


class LegacyEnvFallbacks(unittest.TestCase):
    def test_old_key_name_is_a_fallback(self):
        with _env(CLASSIFIER_URL="https://classifier.example.invalid", CLASSIFIER_MODEL="m"):
            self.assertIsNone(score_posts.classifier_key())
            self.assertFalse(score_posts.classifier_configured())
        with _env(CLASSIFIER_URL="https://classifier.example.invalid", CLASSIFIER_MODEL="m", TYPESAFE_API_KEY="old-key"):
            self.assertEqual(score_posts.classifier_key(), "old-key")
            self.assertTrue(score_posts.classifier_configured())
        with _env(TYPESAFE_API_KEY="old-key", CLASSIFIER_API_KEY="new-key"):
            self.assertEqual(score_posts.classifier_key(), "new-key")


if __name__ == "__main__":
    unittest.main()
