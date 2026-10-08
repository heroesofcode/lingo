import asyncio
import time
import unittest
from unittest import mock

from lingo.config import Config
from lingo.engine import ME, THEY, Engine


class AutoSuggestTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.engine = Engine(Config(mode="native"), "test-key", lambda ev: None)
        self.asked: list[str] = []

        async def fake_suggest(target, reason):
            self.asked.append(target.text)

        self.engine._suggest = fake_suggest

    async def asyncTearDown(self):
        self.engine._cancel_auto()
        await self.engine.chat.close()

    async def say(self, item: str, *deltas: str) -> None:
        for delta in deltas:
            self.engine._on_delta(THEY, item, delta)
            await asyncio.sleep(0)

    async def test_suggests_as_soon_as_the_question_mark_arrives(self):
        await self.say("i1", " Do", " you", " agree")
        self.assertEqual(self.asked, [])
        await self.say("i1", "?")
        self.assertEqual(self.asked, ["Do you agree?"])
        self.engine._on_completed(THEY, "i1", "Do you agree?")
        self.assertIsNone(self.engine._auto_handle)  # nada novo depois do "?": não sugere de novo

    async def test_suggests_again_when_they_keep_talking(self):
        await self.say("i1", " Right", "?", " Because", " it", " failed", ".")
        self.assertEqual(self.asked, ["Right?"])
        self.engine._on_completed(THEY, "i1", "Right? Because it failed.")
        self.assertIsNotNone(self.engine._auto_handle)

    async def test_each_new_question_wins(self):
        await self.say("i1", " Right", "?", " Any", " ideas", "?")
        self.assertEqual(self.asked, ["Right?", "Right? Any ideas?"])


class Clock:
    def __init__(self) -> None:
        self.now = 1000.0

    def __call__(self) -> float:
        return self.now


class LiveTranslationTest(unittest.IsolatedAsyncioTestCase):
    """A tradução ao vivo é um texto corrido; o motor decide embaixo de qual fala ela aparece."""

    async def asyncSetUp(self):
        self.clock = Clock()
        patcher = mock.patch("lingo.engine.time", mock.Mock(monotonic=self.clock, time=time.time))
        patcher.start()
        self.addCleanup(patcher.stop)
        self.events: list[dict] = []
        self.engine = Engine(Config(mode="translate", translate_model="gpt-realtime-translate", auto_suggest=False),
                             "test-key", self.events.append)

        async def no_notes(utt, seg):
            return None

        self.engine._translate = no_notes

    async def asyncTearDown(self):
        await self.engine.chat.close()

    def shown(self) -> dict[str, str]:
        out: dict[str, str] = {}
        for ev in self.events:
            if ev["type"] == "translation":
                out[ev["uid"]] = ev["text"]
        return {uid: text for uid, text in out.items() if text}

    def say(self, speaker: str, item: str, text: str) -> None:
        self.engine._on_delta(speaker, item, " " + text)
        self.engine._on_completed(speaker, item, text)

    def translate(self, text: str, after: float = 0.3) -> None:
        self.clock.now += after
        for word in text.split():
            self.engine._on_live_translation(" " + word)

    async def test_each_line_gets_its_part_of_the_translation(self):
        self.say(THEY, "a", "We rolled out the retry logic.")
        self.translate("Lançamos a lógica de retentativa.")
        self.clock.now += 1
        self.say(THEY, "b", "The error rate dropped.")
        self.translate("A taxa de erro caiu.")
        self.assertEqual(self.shown(), {"THEY:a": "Lançamos a lógica de retentativa.",
                                        "THEY:b": "A taxa de erro caiu."})

    async def test_translation_before_the_line_shows_up_is_kept(self):
        self.translate("Bom dia,")
        self.say(THEY, "a", "Good morning, everyone.")
        self.translate("pessoal.", after=0.1)
        self.assertEqual(self.shown(), {"THEY:a": "Bom dia, pessoal."})

    async def test_next_line_waits_for_a_pause_in_the_translation(self):
        self.say(THEY, "a", "Some timeouts come from the vendor.")
        self.translate("Alguns timeouts vêm")
        self.say(ME, "m", "Okay.")
        self.say(THEY, "c", "Pedro, any idea?")
        self.translate("do fornecedor.", after=0.2)  # ainda é o fim do bloco anterior
        self.translate("Pedro, alguma ideia?", after=1.5)  # depois de uma pausa: bloco novo
        self.assertEqual(self.shown(), {"THEY:a": "Alguns timeouts vêm do fornecedor.",
                                        "THEY:c": "Pedro, alguma ideia?"})


if __name__ == "__main__":
    unittest.main()
