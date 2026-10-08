import unittest

from lingo.config import Config
from lingo.realtime import session_update, translation_update

ARGS = dict(delay="high", silence_ms=500, threshold=0.5, noise_reduction=None)


def audio_input(model: str, languages: list[str], keywords: list[str] = ()) -> dict:
    return session_update(model=model, languages=languages, keywords=list(keywords),
                          **ARGS)["session"]["audio"]["input"]


class SessionUpdateTest(unittest.TestCase):
    def test_live_model_gets_language_hints_and_keywords_without_server_vad(self):
        cfg = audio_input("gpt-live-transcribe", ["pt-BR", "en"], ["Event Hub", "UAT"])
        self.assertIsNone(cfg["turn_detection"])
        self.assertEqual(cfg["transcription"], {"model": "gpt-live-transcribe", "languages": ["pt", "en"],
                                                "delay": "high", "keywords": ["Event Hub", "UAT"]})

    def test_realtime_whisper_uses_one_language_without_prompt(self):
        cfg = audio_input("gpt-realtime-whisper", ["en"])
        self.assertIsNone(cfg["turn_detection"])
        self.assertEqual(cfg["transcription"], {"model": "gpt-realtime-whisper", "language": "en"})

    def test_batch_model_prompt_is_in_the_call_language_with_the_vocabulary(self):
        cfg = audio_input("gpt-4o-transcribe", ["pt-BR"], ["Pedro", "Event Hub"])
        self.assertEqual(cfg["turn_detection"]["type"], "server_vad")
        self.assertEqual(cfg["transcription"]["language"], "pt")
        self.assertTrue(cfg["transcription"]["prompt"].startswith("Escreva apenas"))
        self.assertTrue(cfg["transcription"]["prompt"].endswith("Vocabulário: Pedro, Event Hub."))
        self.assertTrue(audio_input("gpt-4o-transcribe", ["en"])["transcription"]["prompt"].startswith("Write only"))


class TranslationUpdateTest(unittest.TestCase):
    def test_only_translates_without_transcribing_again(self):
        self.assertEqual(translation_update("pt-BR"),
                         {"type": "session.update", "session": {"audio": {"output": {"language": "pt"}}}})


class ConfigLanguagesTest(unittest.TestCase):
    def test_native_mode_hints_english_jargon(self):
        cfg = Config(keywords=["Pedro", "UAT"])
        self.assertEqual(cfg.languages("native"), (["pt-BR", "en"], ["pt-BR"]))
        self.assertEqual(cfg.languages("translate"), (["en"], ["en"]))
        self.assertEqual(cfg.vocabulary(), ["Pedro", "UAT"])


if __name__ == "__main__":
    unittest.main()
