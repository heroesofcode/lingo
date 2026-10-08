import unittest

from lingo.ui import md_to_pango


class MarkdownTest(unittest.TestCase):
    def test_bold_and_code_become_pango_markup(self):
        self.assertEqual(md_to_pango("Está na **3.2.0** da `payments-lib`."),
                         "Está na <b>3.2.0</b> da <tt>payments-lib</tt>.")

    def test_escapes_and_survives_crossed_marks(self):
        self.assertEqual(md_to_pango("a < b & c"), "a &lt; b &amp; c")
        self.assertEqual(md_to_pango("**a `b** c`"), "a b c")


if __name__ == "__main__":
    unittest.main()
