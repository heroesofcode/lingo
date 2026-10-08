import unittest

from lingo.audio import TurnGate

VOICE, QUIET = 0.05, 0.0005


def chunk(n: int) -> bytes:
    return bytes([n])


class TurnGateTest(unittest.TestCase):
    def test_sends_nothing_before_speech_then_the_preroll(self):
        gate = TurnGate(speech_level=0.003, preroll_chunks=2)
        for i in range(5):
            self.assertEqual(gate.process(chunk(i), QUIET), ([], False, False))
        self.assertEqual(gate.process(chunk(9), VOICE), ([chunk(3), chunk(4), chunk(9)], True, False))
        self.assertEqual(gate.process(chunk(10), VOICE), ([chunk(10)], False, False))

    def test_commits_after_the_silence_and_stops_sending(self):
        gate = TurnGate(speech_level=0.003, commit_ms=300)
        gate.process(chunk(0), VOICE)
        self.assertEqual(gate.process(chunk(1), QUIET), ([chunk(1)], False, False))
        self.assertEqual(gate.process(chunk(2), QUIET), ([chunk(2)], False, False))
        self.assertEqual(gate.process(chunk(3), QUIET), ([chunk(3)], False, True))
        self.assertEqual(gate.process(chunk(4), QUIET), ([], False, False))

    def test_short_pause_keeps_the_same_sentence(self):
        gate = TurnGate(speech_level=0.003, commit_ms=700)
        gate.process(chunk(0), VOICE)
        for i in range(4):
            gate.process(chunk(i), QUIET)
        self.assertEqual(gate.process(chunk(5), VOICE), ([chunk(5)], False, False))

    def test_long_monologue_commits_on_a_short_pause(self):
        gate = TurnGate(speech_level=0.003, commit_ms=700, long_ms=1000, long_commit_ms=200)
        for i in range(10):
            gate.process(chunk(i), VOICE)
        gate.process(chunk(10), QUIET)
        self.assertTrue(gate.process(chunk(11), QUIET)[2])


if __name__ == "__main__":
    unittest.main()
