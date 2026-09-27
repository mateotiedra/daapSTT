#!/usr/bin/python3
import errno
import json
import os
import socket
import stat
import signal
import struct

import gi

gi.require_version("IBus", "1.0")
from gi.repository import GLib, IBus

NAME = "daapstt"
MAX_FRAME = 256 * 1024
MAX_QUEUED_OUTPUT = MAX_FRAME * 2
BEGIN_TIMEOUT_MS = 2000
# IBus GTK normalizes X11 hardware codes by subtracting 8. KEY_F24
# (linux/input-event-codes.h) remains 194 even when XKB has no F24 keysym.
EVDEV_F24 = 194


class DictationEngine(IBus.EngineSimple):
    bridge = None

    def __init__(self, **kwargs):
        super().__init__(**kwargs)
        self.context = None
        self.generation = 0
        self.purpose = IBus.InputPurpose.FREE_FORM
        self.capabilities = 0
        self.ordinary_composition = False
        self.bridge.engine_created(self)

    def do_focus_in_id(self, path, client):
        self.context = path
        self.generation += 1
        self.bridge.focus_in(self, path)

    def do_focus_out_id(self, path):
        self.generation += 1
        self.ordinary_composition = False
        self.bridge.focus_out(self)
        self.context = None

    def do_focus_in(self):
        self.context = self.bridge.current_context()
        self.generation += 1
        self.bridge.focus_in(self, self.context)

    def do_focus_out(self):
        self.generation += 1
        self.ordinary_composition = False
        self.bridge.focus_out(self)
        self.context = None

    def do_reset(self):
        self.ordinary_composition = False
        self.generation += 1
        self.bridge.engine_invalidated("focus_changed", self)

    def do_disable(self):
        self.ordinary_composition = False
        self.generation += 1
        self.bridge.engine_invalidated("disabled", self)

    def do_set_capabilities(self, capabilities):
        self.capabilities = capabilities

    def do_set_content_type(self, purpose, hints):
        self.purpose = purpose

    def do_process_key_event(self, keyval, keycode, state):
        if self.bridge.is_active_engine(self):
            # Modifier-only transitions cannot insert text. In particular,
            # shortcut release can deliver Control_L as well as Alt.
            # The next non-modifier key still cancels before being delegated.
            modifier = IBus.KEY_Shift_L <= keyval <= IBus.KEY_Hyper_R
            if modifier or keycode == EVDEV_F24 or keyval in (IBus.KEY_Alt_L, IBus.KEY_Alt_R, IBus.KEY_F24):
                return True
            if not state & IBus.ModifierType.RELEASE_MASK:
                self.bridge.engine_invalidated("user_input", self)
                if keyval == IBus.KEY_Escape:
                    return True
        # EngineSimple owns normal compose/dead-key behavior while idle, and
        # receives the user's triggering key after cancelling a live preview.
        handled = IBus.EngineSimple.do_process_key_event(self, keyval, keycode, state)
        if not state & IBus.ModifierType.RELEASE_MASK:
            dead_key = any(
                keyval == getattr(IBus, name)
                for name in dir(IBus)
                if name.startswith("KEY_dead_")
            )
            multi_key = keyval == getattr(IBus, "KEY_Multi_key", -1)
            if dead_key or multi_key:
                self.ordinary_composition = True
            elif self.ordinary_composition and keyval not in (
                IBus.KEY_Alt_L,
                IBus.KEY_Alt_R,
            ):
                self.ordinary_composition = False
        return handled

    def preedit(self, text):
        self.update_preedit_text_with_mode(
            IBus.Text.new_from_string(text),
            len(text),
            bool(text),
            IBus.PreeditFocusMode.CLEAR,
        )


class Bridge:
    def __init__(self):
        self.client = None
        self.client_watch = None
        self.output_watch = None
        self.buffer = bytearray()
        self.output = bytearray()
        self.last_id = 0
        self.client_epoch = 0

        self.sequence = 0
        self.session = None
        self.engine = None
        self.context = None
        self.generation = None
        self.previous = None
        self.starting = None
        self.focused_engine = None
        self.focused_context = None
        self.commit_in_progress = False

        runtime = os.environ.get("XDG_RUNTIME_DIR")
        self.selection_marker = None
        self.selection_previous = None
        if not runtime:
            raise RuntimeError("XDG_RUNTIME_DIR is required")
        runtime_stat = os.stat(runtime)
        if not stat.S_ISDIR(runtime_stat.st_mode) or runtime_stat.st_uid != os.getuid():
            raise RuntimeError("XDG_RUNTIME_DIR must be an owned directory")
        directory = os.path.join(runtime, "daapstt")
        try:
            os.mkdir(directory, 0o700)
        except FileExistsError:
            directory_stat = os.lstat(directory)
            if not stat.S_ISDIR(directory_stat.st_mode) or directory_stat.st_uid != os.getuid():
                raise RuntimeError("unsafe IME directory")
        os.chmod(directory, 0o700)
        self.path = os.path.join(directory, "ime.sock")

        self.bus = IBus.Bus()
        if not self.bus.is_connected():
            raise RuntimeError("IBus is not connected")
        self.bus.connect("disconnected", lambda *_: self.bus_lost())
        self.factory = IBus.Factory.new(self.bus.get_connection())
        DictationEngine.bridge = self
        self.factory.add_engine(NAME, DictationEngine)
        component = IBus.Component.new(
            "org.freedesktop.IBus.DaapSTT", "DaapSTT", "1", "MIT", "DaapSTT", "", "", ""
        )
        component.add_engine(
            IBus.EngineDesc.new(NAME, "DaapSTT", "Voice composition", "fr", "MIT", "DaapSTT", "", "default")
        )
        if not self.bus.register_component(component):
            raise RuntimeError("failed to register DaapSTT IBus component")

        self.listener = self._create_listener()
        socket_stat = os.lstat(self.path)
        self.socket_identity = (socket_stat.st_dev, socket_stat.st_ino)
        self.listener_watch = GLib.io_add_watch(
            self.listener, GLib.IO_IN | GLib.IO_ERR | GLib.IO_HUP, self.accept
        )

    def _create_listener(self):
        if os.path.lexists(self.path):
            existing = os.lstat(self.path)
            if existing.st_uid != os.getuid() or not stat.S_ISSOCK(existing.st_mode):
                raise RuntimeError("unsafe existing IME socket")
            probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                probe.connect(self.path)
            except OSError as exc:
                if exc.errno not in (errno.ECONNREFUSED, errno.ENOENT):
                    raise
                os.unlink(self.path)
            else:
                raise RuntimeError("IME socket already active")
            finally:
                probe.close()
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.setblocking(False)
        try:
            listener.bind(self.path)
            os.chmod(self.path, 0o600)
            listener.listen(1)
        except Exception:
            listener.close()
            if os.path.lexists(self.path):
                os.unlink(self.path)
            raise
        return listener

    def current_context(self):
        context = self.bus.current_input_context()
        return context if context and context != "/" else None

    def engine_created(self, engine):
        # Factory construction can precede focus notification. No session is
        # attached until focus_in confirms the selected context.
        if self.current_context() == getattr(engine, "context", None):
            self.focused_engine = engine

    def focus_in(self, engine, context):
        context = context or self.current_context()
        self.focused_engine = engine
        self.focused_context = context
        if self.session is not None:
            if not self.commit_in_progress:
                self.invalidate("focus_changed", engine)
            return
        pending = self.starting
        if pending is not None:
            if context != pending["context"]:
                self.abort_begin("focus_changed")
                return
            pending["engine"] = engine
            pending["generation"] = engine.generation
            GLib.idle_add(self.complete_begin)

    def focus_out(self, engine):
        if self.focused_engine is engine:
            self.focused_engine = None
            self.focused_context = None
        if self.session is not None and not self.commit_in_progress:
            self.invalidate("focus_changed", engine)
        # A focus-out caused by selecting this engine is tolerated only while
        # Starting. complete_begin still requires a focus-in on the same context.

    def engine_invalidated(self, reason, engine):
        if self.commit_in_progress and engine is self.engine:
            return
        if self.session is not None:
            self.invalidate(reason, engine)
        elif self.starting is not None and engine is self.starting.get("engine"):
            self.abort_begin("focus_changed")

    def is_active_engine(self, engine):
        return (
            self.session is not None
            and self.engine is engine
            or self.starting is not None
            and self.starting.get("engine") is engine
        )

    def bus_lost(self):
        self.abort_begin("unavailable", restore=False)
        self.invalidate("disconnected", restore=False)
        self.disconnect(cleanup=False)
        if hasattr(self, "loop"):
            self.loop.quit()

    def accept(self, source, condition):
        if condition & (GLib.IO_ERR | GLib.IO_HUP):
            return True
        while True:
            try:
                connection, _ = self.listener.accept()
            except BlockingIOError:
                break
            connection.setblocking(False)
            credentials = connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize("3i"))
            _pid, uid, _gid = struct.unpack("3i", credentials)
            if uid != os.getuid() or self.client is not None:
                connection.close()
                continue
            self.client = connection
            self.client_epoch += 1
            self.buffer.clear()
            self.output.clear()
            self.last_id = 0
            self.client_watch = GLib.io_add_watch(
                connection, GLib.IO_IN | GLib.IO_HUP | GLib.IO_ERR, self.receive
            )
        return True

    def send(self, payload):
        if self.client is None:
            return
        frame = (json.dumps({"v": 1, **payload}, ensure_ascii=False, separators=(",", ":")) + "\n").encode("utf-8")
        if len(self.output) + len(frame) > MAX_QUEUED_OUTPUT:
            self.disconnect()
            return
        self.output.extend(frame)
        self.flush_output()

    def flush_output(self, source=None, condition=None):
        if self.client is None:
            self.output.clear()
            return False
        if condition is not None and condition & (GLib.IO_ERR | GLib.IO_HUP):
            self.disconnect()
            return False
        while self.output:
            try:
                sent = self.client.send(self.output)
            except BlockingIOError:
                if self.output_watch is None:
                    self.output_watch = GLib.io_add_watch(
                        self.client, GLib.IO_OUT | GLib.IO_ERR | GLib.IO_HUP, self.flush_output
                    )
                return True
            except OSError:
                self.disconnect()
                return False
            if sent <= 0:
                self.disconnect()
                return False
            del self.output[:sent]
        self.output_watch = None
        return False

    def reply(self, request, error=None, **kwargs):
        self.send({"id": request["id"], "ok": error is None, **({"error": error} if error else kwargs)})

    def disconnect(self, cleanup=True):
        if cleanup:
            self.abort_begin("unavailable")
            self.invalidate("disconnected", notify=False)
        if self.output_watch is not None:
            GLib.source_remove(self.output_watch)
            self.output_watch = None
        connection = self.client
        self.client = None
        self.client_watch = None
        if connection is not None:
            connection.close()
        self.buffer.clear()
        self.output.clear()

    def receive(self, connection, condition):
        if connection is not self.client:
            return False
        if condition & (GLib.IO_HUP | GLib.IO_ERR):
            self.disconnect()
            return False
        try:
            data = connection.recv(65536)
        except BlockingIOError:
            return True
        except OSError:
            self.disconnect()
            return False
        if not data:
            self.disconnect()
            return False
        self.buffer.extend(data)
        if len(self.buffer) > MAX_FRAME and b"\n" not in self.buffer:
            self.disconnect()
            return False
        while b"\n" in self.buffer:
            newline = self.buffer.index(b"\n")
            frame = bytes(self.buffer[:newline])
            del self.buffer[: newline + 1]
            if not frame or len(frame) > MAX_FRAME:
                self.disconnect()
                return False
            try:
                request = json.loads(frame.decode("utf-8"))
            except (ValueError, UnicodeError):
                self.send({"id": None, "ok": False, "error": "invalid_request"})
                continue
            self.handle(request)
            if connection is not self.client:
                return False
        if len(self.buffer) > MAX_FRAME:
            self.disconnect()
            return False
        return True

    def _global_engine_name(self):
        selected = self.bus.get_global_engine() if self.bus.is_connected() else None
        return selected.get_name() if selected is not None else None

    def _restore(self, previous):
        if previous and self.bus.is_connected() and self._global_engine_name() == NAME:
            self.bus.set_global_engine_async(previous, BEGIN_TIMEOUT_MS, None, self.restore_finished, None)

    @staticmethod
    def restore_finished(bus, result, _data):
        try:
            bus.set_global_engine_async_finish(result)
        except GLib.Error:
            pass

    def abort_begin(self, error, restore=True):
        pending = self.starting
        if pending is None:
            return
        self.starting = None
        if restore:
            self._restore(pending.get("previous"))
        if self.client is not None and pending["epoch"] == self.client_epoch:
            self.reply(pending["request"], error)

    def complete_begin(self):
        pending = self.starting
        if pending is None or not pending.get("selected"):
            return False
        engine = pending.get("engine")
        if engine is None and self.focused_context == pending["context"]:
            engine = self.focused_engine
            if engine is not None:
                pending["engine"] = engine
                pending["generation"] = engine.generation
        if engine is None:
            return False
        if (
            self.client is None
            or pending["epoch"] != self.client_epoch
            or self.current_context() != pending["context"]
            or self.focused_context != pending["context"]
            or self.focused_engine is not engine
            or engine.generation != pending["generation"]
        ):
            self.abort_begin("focus_changed")
            return False
        if not engine.capabilities & IBus.Capabilite.PREEDIT_TEXT or engine.purpose in (
            IBus.InputPurpose.PASSWORD,
            IBus.InputPurpose.PIN,
        ):
            self.abort_begin("no_context")
            return False
        self.sequence += 1
        self.session = self.sequence
        self.engine = engine
        self.context = pending["context"]
        self.generation = engine.generation
        self.previous = pending["previous"]
        request = pending["request"]
        self.starting = None
        self.reply(request, session=self.session)
        return False

    def timeout_begin(self, epoch, request_id):
        pending = self.starting
        if pending is not None and pending["epoch"] == epoch and pending["request"]["id"] == request_id:
            self.abort_begin("no_context")
        return False

    def selected(self, bus, result, marker):
        try:
            succeeded = bool(bus.set_global_engine_async_finish(result))
        except GLib.Error:
            succeeded = False
        if marker != self.selection_marker:
            return
        previous = self.selection_previous
        self.selection_marker = None
        self.selection_previous = None
        pending = self.starting
        if pending is None or marker != (pending["epoch"], pending["request"]["id"]):
            if succeeded:
                self._restore(previous)
            return
        pending["selected"] = succeeded
        if not succeeded:
            self.abort_begin("unavailable")
            return
        # If daapstt was already selected, IBus emits no fresh focus event.
        # Reuse only the engine currently confirmed focused on this context.
        if self.focused_context == pending["context"] and self.focused_engine is not None:
            pending["engine"] = self.focused_engine
            pending["generation"] = self.focused_engine.generation
        self.complete_begin()

    def valid(self, request):
        return (
            self.session is not None
            and request.get("session") == self.session
            and self.engine is not None
            and self.engine.generation == self.generation
            and self.current_context() == self.context
            and self.focused_engine is self.engine
            and self.focused_context == self.context
            and self._global_engine_name() == NAME
        )

    def invalidate(self, reason, engine=None, notify=True, restore=True):
        if self.session is None or (engine is not None and engine is not self.engine):
            return
        old_session = self.session
        current = self.engine
        previous = self.previous
        self.session = self.engine = self.context = self.generation = self.previous = None
        if current is not None:
            current.preedit("")
        if notify:
            self.send({"event": "cancelled", "session": old_session, "reason": reason})
        if restore:
            self._restore(previous)

    @staticmethod
    def _valid_shape(request):
        if (
            not isinstance(request, dict)
            or type(request.get("v")) is not int
            or request["v"] != 1
            or type(request.get("id")) is not int
        ):
            return False
        operation = request.get("op")
        fields = {
            "begin": {"v", "id", "op"},
            "partial": {"v", "id", "op", "session", "text"},
            "commit": {"v", "id", "op", "session", "text"},
            "clear": {"v", "id", "op", "session"},
            "paste": {"v", "id", "op", "session"},
            "end": {"v", "id", "op", "session"},
            "cancel": {"v", "id", "op", "session"},
        }
        if not isinstance(operation, str) or operation not in fields:
            return False
        if set(request) != fields[operation]:
            return False
        if operation in ("partial", "commit"):
            text = request["text"]
            if not isinstance(text, str) or "\x00" in text:
                return False
            try:
                text.encode("utf-8")
            except UnicodeEncodeError:
                return False
        if operation != "begin" and type(request["session"]) is not int:
            return False
        return True

    def handle(self, request):
        request_id = request.get("id") if isinstance(request, dict) else None
        if not self._valid_shape(request) or request["id"] <= self.last_id:
            self.send({"id": request_id, "ok": False, "error": "invalid_request"})
            return
        self.last_id = request["id"]
        operation = request["op"]
        if operation == "begin":
            if self.session is not None or self.starting is not None or self.selection_marker is not None:
                self.reply(request, "busy")
                return
            context = self.current_context()
            if context is None:
                self.reply(request, "no_context")
                return
            if self.focused_engine is not None and self.focused_engine.ordinary_composition:
                self.reply(request, "no_context")
                return
            selected_name = self._global_engine_name()
            previous = selected_name if selected_name and selected_name != NAME else None
            marker = (self.client_epoch, request["id"])
            self.starting = {
                "request": request,
                "context": context,
                "previous": previous,
                "epoch": self.client_epoch,
                "selected": False,
            }
            self.selection_marker = marker
            self.selection_previous = previous
            try:
                self.bus.set_global_engine_async(NAME, BEGIN_TIMEOUT_MS, None, self.selected, marker)
            except GLib.Error:
                self.selection_marker = None
                self.selection_previous = None
                self.abort_begin("unavailable")
                return
            GLib.timeout_add(BEGIN_TIMEOUT_MS, self.timeout_begin, *marker)
            return
        if not self.valid(request):
            self.invalidate("focus_changed")
            self.reply(request, "focus_changed")
            return
        engine = self.engine
        if operation == "partial":
            engine.preedit(request["text"])
            self.reply(request)
        elif operation == "clear":
            engine.preedit("")
            self.reply(request)
        elif operation == "paste":
            engine.preedit("")
            engine.forward_key_event(IBus.KEY_v, 0, IBus.ModifierType.CONTROL_MASK)
            engine.forward_key_event(
                IBus.KEY_v,
                0,
                IBus.ModifierType.CONTROL_MASK | IBus.ModifierType.RELEASE_MASK,
            )
            self.reply(request)
        elif operation == "commit":
            # Clearing/committing can synchronously reset and refocus Chromium.
            # Suppress those invalidations: successful return means the one
            # native commit was accepted. Queue its ACK before restoring.
            self.commit_in_progress = True
            try:
                engine.preedit("")
                engine.commit_text(IBus.Text.new_from_string(request["text"]))
            except Exception:
                self.commit_in_progress = False
                self.invalidate("disabled", notify=False)
                self.reply(request, "unavailable")
                return
            self.commit_in_progress = False
            self.reply(request)
            self.invalidate("disabled", notify=False)
        else:
            engine.preedit("")
            self.reply(request)
            self.invalidate("disabled", notify=False)

    def close(self):
        self.disconnect()
        if getattr(self, "listener", None) is not None:
            self.listener.close()
        if getattr(self, "path", None) and os.path.lexists(self.path):
            existing = os.lstat(self.path)
            identity = getattr(self, "socket_identity", None)
            if (
                identity == (existing.st_dev, existing.st_ino)
                and existing.st_uid == os.getuid()
                and stat.S_ISSOCK(existing.st_mode)
            ):
                os.unlink(self.path)


def main():
    IBus.init()
    bridge = Bridge()
    bridge.loop = GLib.MainLoop()

    def stop(_signal):
        bridge.loop.quit()
        return GLib.SOURCE_REMOVE

    GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGTERM, stop, signal.SIGTERM)
    GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGINT, stop, signal.SIGINT)
    print("DaapSTT IBus bridge ready", flush=True)
    try:
        bridge.loop.run()
    finally:
        bridge.close()


if __name__ == "__main__":
    main()
