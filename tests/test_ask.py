import sys
import tempfile
import unittest
from pathlib import Path

from lingo.ask import AskError, ask_claude

HEADER = """
import json, sys
sys.stdin.read()
def out(ev):
    print(json.dumps(ev), flush=True)
def delta(text):
    out({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": text}}})
"""

ANSWER = HEADER + """
out({"type": "system", "subtype": "init"})
out({"type": "stream_event", "event": {"type": "message_start"}})
delta("Vou ver.")
out({"type": "stream_event", "event": {"type": "message_delta", "delta": {"stop_reason": "tool_use"}}})
out({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Grep",
     "input": {"pattern": "payments-lib", "path": "/home/x/code/checkout-service"}}]}})
out({"type": "user", "message": {"content": "x" * 300000}})
out({"type": "stream_event", "event": {"type": "message_start"}})
delta("Usa a ")
delta("2.10.1.")
out({"type": "result", "subtype": "success", "is_error": False, "result": "Usa a 2.10.1."})
"""

FAILS = HEADER + """
out({"type": "result", "subtype": "error_during_execution", "is_error": True, "result": "sem crédito"})
"""

CRASHES = """
import sys
sys.stderr.write("not logged in")
sys.exit(1)
"""


class AskClaudeTest(unittest.IsolatedAsyncioTestCase):
    async def run_fake(self, script: str) -> list[tuple[str, str]]:
        with tempfile.TemporaryDirectory() as tmp:
            fake = Path(tmp) / "fake_claude.py"
            fake.write_text(script)
            return [item async for item in ask_claude([sys.executable, str(fake)], Path(tmp), "pergunta")]

    async def test_streams_the_answer_and_what_it_is_looking_at(self):
        events = await self.run_fake(ANSWER)
        self.assertEqual(events, [
            ("text", "Vou ver."),
            ("text", ""),  # o "vou ver" some quando ele vai usar uma ferramenta
            ("progress", "Procurando “payments-lib” em code/checkout-service"),
            ("text", "Usa a "),
            ("text", "Usa a 2.10.1."),
            ("text", "Usa a 2.10.1."),
        ])

    async def test_error_result_raises(self):
        with self.assertRaisesRegex(AskError, "sem crédito"):
            await self.run_fake(FAILS)

    async def test_crash_shows_stderr(self):
        with self.assertRaisesRegex(AskError, "not logged in"):
            await self.run_fake(CRASHES)


if __name__ == "__main__":
    unittest.main()
