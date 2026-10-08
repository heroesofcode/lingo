import unittest

from lingo.text import (
    clean_transcript,
    filter_note,
    is_question,
    mentions,
    parse_note,
    parse_options,
    similar,
    split_translation,
    take_sentences,
)


class TakeSentencesTest(unittest.TestCase):
    def test_cuts_finished_sentences_and_keeps_the_rest(self):
        text = " Okay, so quick update on the payment service. We rolled out"
        pieces, used = take_sentences(text, final=False)
        self.assertEqual(pieces, ["Okay, so quick update on the payment service."])
        self.assertEqual(text[used:], " We rolled out")

    def test_question_mark_closes_right_away(self):
        self.assertEqual(take_sentences(" Pedro, you looked into the logs, right?", final=False)[0],
                         ["Pedro, you looked into the logs, right?"])

    def test_waits_when_a_number_may_continue(self):
        self.assertEqual(take_sentences(" It costs 3.", final=False), ([], 0))
        self.assertEqual(take_sentences(" It costs 3.5 dollars.", final=False)[0], ["It costs 3.5 dollars."])

    def test_long_sentence_is_cut_at_a_comma(self):
        text = " " + " ".join(["word"] * 15) + ", and then more"
        pieces, used = take_sentences(text, final=False)
        self.assertEqual(pieces, [" ".join(["word"] * 15) + ","])
        self.assertEqual(text[used:], " and then more")
        self.assertEqual(take_sentences(" short one, and more", final=False), ([], 0))

    def test_tiny_sentence_waits_for_the_next(self):
        self.assertEqual(take_sentences(" Pedro.", final=False), ([], 0))
        self.assertEqual(take_sentences(" Pedro. You looked into the logs, right?", final=False)[0],
                         ["Pedro. You looked into the logs, right?"])
        self.assertEqual(take_sentences(" Right?", final=True)[0], ["Right?"])

    def test_final_takes_the_rest(self):
        self.assertEqual(take_sentences(" It is done. and the tail", final=True)[0], ["It is done.", "and the tail"])


class CleanTranscriptTest(unittest.TestCase):
    def test_drops_known_hallucinations(self):
        for text in ["you", "Thanks for watching!", "[Music]", "♪ la la ♪", "...", "  "]:
            self.assertEqual(clean_transcript(text), "", text)

    def test_keeps_real_speech(self):
        self.assertEqual(clean_transcript("  Can you   hear me? "), "Can you hear me?")
        self.assertEqual(clean_transcript("Thank you, Pedro."), "Thank you, Pedro.")


class TriggersTest(unittest.TestCase):
    def test_question_and_name(self):
        self.assertTrue(is_question("What do you think?"))
        self.assertFalse(is_question("Let's touch base on Friday."))
        self.assertTrue(mentions("Pedro, any update?", ["Pedro"]))
        self.assertFalse(mentions("Pedroso will join later", ["Pedro"]))

    def test_echo_similarity(self):
        self.assertGreater(similar("Can you walk us through it?", "can you walk us through it"), 0.9)
        self.assertLess(similar("Sure, I can do that.", "Can you walk us through it?"), 0.6)


class ParseOptionsTest(unittest.TestCase):
    def test_full_output(self):
        raw = "A|Sure.\na|Claro.\nB|Yes, because X.\nb|Sim, porque X.\nC|Can I follow up?\nc|Posso retornar?"
        opts = parse_options(raw)
        self.assertEqual([o.text for o in opts], ["Sure.", "Yes, because X.", "Can I follow up?"])
        self.assertEqual(opts[2].gloss, "Posso retornar?")

    def test_partial_stream_and_markdown(self):
        opts = parse_options("**A**| Sure thing\na| Claro\nB| I'd say")
        self.assertEqual(opts[0].text, "Sure thing")
        self.assertEqual(opts[1].text, "I'd say")
        self.assertEqual(opts[2].text, "")


class SplitTranslationTest(unittest.TestCase):
    def test_translation_with_note(self):
        tr, note = split_translation("Vamos nos falar na sexta.\nNOTE: touch base = falar rapidamente")
        self.assertEqual(tr, "Vamos nos falar na sexta.")
        self.assertEqual(note, "touch base = falar rapidamente")

    def test_strips_leaked_labels(self):
        self.assertEqual(split_translation(">>> Eles: Honestamente, sim.")[0], "Honestamente, sim.")
        self.assertEqual(split_translation('Tradução: "Oi"')[0], "Oi")

    def test_note_keeps_only_expressions_from_the_line(self):
        line = "Let's touch base again on Friday."
        note = "touch base = falar rapidamente; circle back = retomar depois"
        self.assertEqual(filter_note(note, line), "touch base = falar rapidamente")

    def test_note_dedupes_and_accepts_inflections(self):
        line = "We're circling back on that."
        note = "circle back = retomar; circle back = revisit later"
        self.assertEqual(filter_note(note, line), "circle back = retomar")

    def test_notes_only_answer(self):
        self.assertEqual(parse_note("NOTE: ballpark = estimativa aproximada"), "ballpark = estimativa aproximada")
        self.assertEqual(parse_note("**NOTE:** heads-up = aviso"), "heads-up = aviso")
        self.assertEqual(parse_note("NONE"), "")

    def test_streaming_only_note_prefix(self):
        self.assertEqual(split_translation("NOTE: x"), ("", ""))


if __name__ == "__main__":
    unittest.main()
