import importlib.util
import pathlib
import sys
import types
import unittest


class FakeGLib:
    IO_IN = 1
    IO_OUT = 2
    IO_HUP = 4
    IO_ERR = 8
    PRIORITY_DEFAULT = 0
    SOURCE_REMOVE = False

    class Error(Exception):
        pass

    @staticmethod
    def idle_add(callback, *args):
        return callback(*args)

    @staticmethod
    def timeout_add(*_args):
        return 1

    @staticmethod
    def source_remove(_source):
        return None


class FakeEngineSimple:
    def __init__(self, **_kwargs):
        self.parent_keys = []
        self.preedits = []
        self.commits = []
        self.forwarded = []

    def do_process_key_event(self, keyval, keycode, state):
        self.parent_keys.append((keyval, keycode, state))
        return keyval == 77

    def update_preedit_text_with_mode(self, text, cursor, visible, mode):
        self.preedits.append((text, cursor, visible, mode))

    def commit_text(self, text):
        self.commits.append(text)

    def forward_key_event(self, keyval, keycode, state):
        self.forwarded.append((keyval, keycode, state))


class Text:
    @staticmethod
    def new_from_string(value):
        return value


FakeIBus = types.SimpleNamespace(
    EngineSimple=FakeEngineSimple,
    InputPurpose=types.SimpleNamespace(FREE_FORM=0, PASSWORD=1, PIN=2),
    Capabilite=types.SimpleNamespace(PREEDIT_TEXT=1),
    ModifierType=types.SimpleNamespace(RELEASE_MASK=1, CONTROL_MASK=2),
    PreeditFocusMode=types.SimpleNamespace(CLEAR=0),
    Text=Text,
    KEY_Alt_L=10,
    KEY_Alt_R=11,
    KEY_F24=12,
    KEY_Escape=13,
    KEY_v=14,
    KEY_dead_acute=15,
    KEY_Multi_key=16,
    KEY_Shift_L=0xffe1,
    KEY_Hyper_R=0xffee,
)

gi = types.ModuleType("gi")
gi.require_version = lambda *_args: None
repository = types.ModuleType("gi.repository")
repository.GLib = FakeGLib
repository.IBus = FakeIBus
gi.repository = repository
sys.modules.setdefault("gi", gi)
sys.modules.setdefault("gi.repository", repository)

module_path = pathlib.Path(__file__).parents[1] / "engine.py"
spec = importlib.util.spec_from_file_location("daapstt_ibus_engine", module_path)
engine_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(engine_module)
Bridge = engine_module.Bridge
DictationEngine = engine_module.DictationEngine
NAME = engine_module.NAME


class Selected:
    def __init__(self, name):
        self.name = name

    def get_name(self):
        return self.name


class FakeBus:
    def __init__(self, context="/context", selected=NAME):
        self.context = context
        self.selected = selected
        self.selections = []

    def is_connected(self):
        return True

    def current_input_context(self):
        return self.context

    def get_global_engine(self):
        return Selected(self.selected) if self.selected else None

    def set_global_engine_async(self, name, timeout, cancellable, callback, data):
        self.selected = name
        self.selections.append(name)

    def set_global_engine_async_finish(self, _result):
        return True


class FakeDictationEngine(FakeEngineSimple):
    def __init__(self, generation=1):
        super().__init__()
        self.generation = generation
        self.capabilities = FakeIBus.Capabilite.PREEDIT_TEXT
        self.purpose = FakeIBus.InputPurpose.FREE_FORM
        self.ordinary_composition = False

    def preedit(self, value):
        self.preedits.append(value)


class BridgeTests(unittest.TestCase):
    def make_bridge(self):
        bridge = Bridge.__new__(Bridge)
        bridge.client = object()
        bridge.client_epoch = 1
        bridge.last_id = 0
        bridge.sequence = 0
        bridge.session = None
        bridge.engine = None
        bridge.context = None
        bridge.generation = None
        bridge.previous = None
        bridge.starting = None
        bridge.selection_marker = None
        bridge.selection_previous = None
        bridge.focused_engine = None
        bridge.focused_context = None
        bridge.commit_in_progress = False
        bridge.bus = FakeBus()
        bridge.sent = []
        bridge.send = bridge.sent.append
        return bridge

    def activate(self, bridge, token=1):
        active = FakeDictationEngine()
        bridge.sequence = token
        bridge.session = token
        bridge.engine = active
        bridge.context = bridge.bus.context
        bridge.generation = active.generation
        bridge.previous = "xkb:fr::fra"
        bridge.focused_engine = active
        bridge.focused_context = bridge.context
        return active

    def test_strict_shape_and_increasing_ids(self):
        bridge = self.make_bridge()
        bridge.handle({"v": 1, "id": 1, "op": "begin", "extra": True})
        bridge.handle({"v": 1, "id": True, "op": "begin"})
        bridge.handle({"v": True, "id": 1, "op": "begin"})
        bridge.handle({"v": 1, "id": 1, "op": []})
        bridge.handle({"v": 1, "id": 1, "op": {}})
        bridge.handle({"v": 1, "id": 1, "op": "partial", "session": 1, "text": "\ud800"})
        bridge.handle({"v": 1, "id": 1, "op": "partial", "session": 1, "text": "bad\u0000text"})
        self.assertEqual(
            [message["error"] for message in bridge.sent],
            ["invalid_request"] * 7,
        )
        self.assertEqual(bridge.last_id, 0)

        active = self.activate(bridge)
        bridge.handle({"v": 1, "id": 2, "op": "partial", "session": 1, "text": "été"})
        bridge.handle({"v": 1, "id": 2, "op": "clear", "session": 1})
        self.assertEqual(active.preedits, ["été"])
        self.assertTrue(bridge.sent[-2]["ok"])
        self.assertEqual(bridge.sent[-1]["error"], "invalid_request")

    def test_begin_reuses_already_focused_daapstt_without_focus_event(self):
        bridge = self.make_bridge()
        focused = FakeDictationEngine(generation=7)
        bridge.focused_engine = focused
        bridge.focused_context = bridge.bus.context

        request = {"v": 1, "id": 1, "op": "begin"}
        bridge.handle(request)
        self.assertIsNotNone(bridge.starting)
        bridge.selected(bridge.bus, object(), (1, 1))

        self.assertEqual(bridge.session, 1)
        self.assertIs(bridge.engine, focused)
        self.assertEqual(bridge.sent[-1]["session"], 1)
        self.assertTrue(bridge.sent[-1]["ok"])

    def test_focus_change_clears_preview_and_invalidates_immediately(self):
        bridge = self.make_bridge()
        active = self.activate(bridge, token=4)
        bridge.focus_out(active)
        self.assertIsNone(bridge.session)
        self.assertEqual(active.preedits, [""])
        self.assertEqual(bridge.sent[0]["event"], "cancelled")
        self.assertEqual(bridge.sent[0]["reason"], "focus_changed")
        self.assertEqual(bridge.sent[0]["session"], 4)
        self.assertEqual(bridge.bus.selections, ["xkb:fr::fra"])

    def test_disconnect_aborts_pending_begin_and_restores_previous_engine(self):
        bridge = self.make_bridge()
        bridge.bus.selected = NAME
        bridge.starting = {
            "request": {"v": 1, "id": 3, "op": "begin"},
            "context": bridge.bus.context,
            "previous": "xkb:fr::fra",
            "epoch": bridge.client_epoch,
            "selected": True,
        }
        bridge.abort_begin("unavailable")
        self.assertIsNone(bridge.starting)
        self.assertEqual(bridge.bus.selections, ["xkb:fr::fra"])
        self.assertEqual(bridge.sent[-1]["error"], "unavailable")

    def test_late_selection_after_disconnect_is_finished_then_restored(self):
        bridge = self.make_bridge()
        marker = (bridge.client_epoch, 3)
        bridge.selection_marker = marker
        bridge.selection_previous = "xkb:fr::fra"
        bridge.starting = {
            "request": {"v": 1, "id": 3, "op": "begin"},
            "context": bridge.bus.context,
            "previous": "xkb:fr::fra",
            "epoch": bridge.client_epoch,
            "selected": False,
        }
        bridge.abort_begin("unavailable", restore=False)
        bridge.bus.selected = NAME
        bridge.selected(bridge.bus, object(), marker)
        self.assertIsNone(bridge.selection_marker)
        self.assertEqual(bridge.bus.selections, ["xkb:fr::fra"])

    def test_begin_refuses_ordinary_engine_composition(self):
        bridge = self.make_bridge()
        focused = FakeDictationEngine()
        focused.ordinary_composition = True
        bridge.focused_engine = focused
        bridge.focused_context = bridge.bus.context
        bridge.handle({"v": 1, "id": 1, "op": "begin"})
        self.assertIsNone(bridge.starting)
        self.assertEqual(bridge.sent[-1]["error"], "no_context")

    def test_final_commit_ack_precedes_restore_and_ignores_synchronous_reset(self):
        bridge = self.make_bridge()
        active = self.activate(bridge, token=8)
        events = []
        bridge.send = lambda payload: events.append(("send", payload.copy()))
        original_restore = bridge._restore
        bridge._restore = lambda previous: (events.append(("restore", previous)), original_restore(previous))

        original_preedit = active.preedit

        def clear_with_reset(text):
            original_preedit(text)
            if not text:
                active.generation += 1
                bridge.engine_invalidated("focus_changed", active)

        def commit_with_reset(text):
            active.commits.append(text)
            active.generation += 1
            bridge.engine_invalidated("focus_changed", active)
            bridge.focus_out(active)
            bridge.focus_in(active, bridge.context)

        active.preedit = clear_with_reset
        active.commit_text = commit_with_reset
        bridge.handle({"v": 1, "id": 9, "op": "commit", "session": 8, "text": "bonjour"})

        self.assertEqual(active.commits, ["bonjour"])
        self.assertEqual(events[0], ("send", {"id": 9, "ok": True}))
        self.assertEqual(events[1], ("restore", "xkb:fr::fra"))
        self.assertFalse(any(item[0] == "send" and "event" in item[1] for item in events))
        self.assertIsNone(bridge.session)


class KeyboardTests(unittest.TestCase):
    def setUp(self):
        self.bridge = types.SimpleNamespace(
            active=True,
            invalidations=[],
            engine_created=lambda _engine: None,
            current_context=lambda: "/context",
        )
        self.bridge.is_active_engine = lambda _engine: self.bridge.active
        self.bridge.engine_invalidated = lambda reason, _engine: self.bridge.invalidations.append(reason)
        DictationEngine.bridge = self.bridge
        self.engine = DictationEngine()

    def test_hotkey_is_consumed_without_cancelling(self):
        self.assertTrue(self.engine.do_process_key_event(FakeIBus.KEY_F24, 0, 0))
        self.assertTrue(self.engine.do_process_key_event(FakeIBus.KEY_Alt_L, 0, FakeIBus.ModifierType.RELEASE_MASK))
        self.assertEqual(self.bridge.invalidations, [])
        self.assertEqual(self.engine.parent_keys, [])

    def test_unmapped_physical_f24_repeats_do_not_cancel(self):
        # XWayland keycode 202 has no keysym in the installed layout.
        # GTK passes its evdev code (202 - 8) to IBus, even for VoidSymbol.
        for state in (0, 0, FakeIBus.ModifierType.RELEASE_MASK):
            self.assertTrue(self.engine.do_process_key_event(0xFFFFFF, 194, state))
        self.assertEqual(self.bridge.invalidations, [])
        self.assertEqual(self.engine.parent_keys, [])

    def test_modifier_restore_preserves_session_but_next_letter_cancels(self):
        # Captured on the real shortcut release: Control_L, evdev 29, state 0x18.
        self.assertTrue(self.engine.do_process_key_event(0xffe3, 29, 0x18))
        self.assertTrue(self.engine.do_process_key_event(
            0xffe3, 29, 0x18 | FakeIBus.ModifierType.RELEASE_MASK
        ))
        self.assertEqual(self.bridge.invalidations, [])
        self.assertTrue(self.engine.do_process_key_event(77, 4, 0))
        self.assertEqual(self.bridge.invalidations, ["user_input"])

    def test_manual_key_cancels_then_uses_engine_simple_parent(self):
        self.assertTrue(self.engine.do_process_key_event(77, 4, 0))
        self.assertEqual(self.bridge.invalidations, ["user_input"])
        self.assertEqual(self.engine.parent_keys, [(77, 4, 0)])

    def test_escape_cancels_and_is_not_forwarded(self):
        self.assertTrue(self.engine.do_process_key_event(FakeIBus.KEY_Escape, 9, 0))
        self.assertEqual(self.bridge.invalidations, ["user_input"])
        self.assertEqual(self.engine.parent_keys, [])

    def test_idle_dead_key_tracks_ordinary_composition(self):
        self.bridge.active = False
        self.engine.do_process_key_event(FakeIBus.KEY_dead_acute, 0, 0)
        self.assertTrue(self.engine.ordinary_composition)
        self.engine.do_process_key_event(77, 0, 0)
        self.assertFalse(self.engine.ordinary_composition)


if __name__ == "__main__":
    unittest.main()
