import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
TAO = ROOT / "vendor/tao/src/platform_impl/macos"


def block(source, marker):
    start = source.index(marker)
    opening = source.index("{", start)
    depth = 1
    end = opening + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


class NativeQuitTests(unittest.TestCase):
    def setUp(self):
        self.state = (TAO / "app_state.rs").read_text()
        self.delegate = (TAO / "app_delegate.rs").read_text()
        self.runtime = (ROOT / "vendor/tauri-runtime-wry/src/lib.rs").read_text()

    def test_actual_native_termination_state_machine(self):
        source = "use std::sync::atomic::{AtomicBool, Ordering};\n"
        declaration = self.state.index("#[derive(Default)]\nstruct NativeTermination(")
        source += self.state[declaration : self.state.index("impl NativeTermination", declaration)]
        source += block(self.state, "impl NativeTermination")
        source += "\n#[cfg(test)]\n" + block(self.state, "mod native_termination_tests")
        with tempfile.TemporaryDirectory(prefix="monhop-native-quit-test-") as directory:
            directory = pathlib.Path(directory)
            rust = directory / "native_quit.rs"
            binary = directory / "native_quit_tests"
            rust.write_text(source)
            subprocess.run(
                ["rustc", "+1.98.1", "--edition=2021", "--test", str(rust), "-o", str(binary)],
                check=True,
            )
            subprocess.run([str(binary)], check=True)

    def test_native_quit_defers_and_queues_instead_of_destroying_the_loop(self):
        self.assertIn("sel!(applicationShouldTerminate:)", self.delegate)
        request = block(self.delegate, "extern \"C\" fn application_should_terminate")
        self.assertIn("AppState::request_exit()", request)
        self.assertIn("NSApplicationTerminateReply::TerminateLater", request)
        self.assertNotIn("TerminateCancel", request)
        request = block(self.state, "pub fn request_exit()")
        self.assertIn("if HANDLER.native_termination.request()", request)
        self.assertIn("Self::queue_event(EventWrapper::StaticEvent(Event::ExitRequested))", request)
        self.assertIn("CFRunLoopWakeUp", request)
        self.assertNotIn("handle_nonuser_event", request)

    def test_appkit_reply_happens_after_callback_locks_are_released(self):
        cleared = block(self.state, "pub fn cleared(")
        release = cleared.index("HANDLER.set_in_callback(false)")
        approval = cleared.index("if HANDLER.should_exit()", release)
        reply = cleared.index("app.replyToApplicationShouldTerminate(true)")
        self.assertLess(release, approval)
        self.assertLess(approval, reply)
        self.assertIn(".take_approved(true, HANDLER.get_in_callback())", cleared[approval:reply])
        self.assertNotIn("replyToApplicationShouldTerminate", self.runtime)

    def test_runtime_forwards_a_cancellable_exit_request(self):
        forwarded = block(self.runtime, "Event::ExitRequested =>")
        self.assertIn("callback(RunEvent::ExitRequested { code: None, tx })", forwarded)
        self.assertIn("if !matches!(rx.try_recv(), Ok(ExitRequestedEventAction::Prevent))", forwarded)
        self.assertIn("*control_flow = ControlFlow::Exit", forwarded)
        self.assertNotIn("rx.recv(", forwarded)


if __name__ == "__main__":
    unittest.main()
