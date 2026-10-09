"""Real terminal acceptance checks. Run: uv run --with pyte tests/terminal_ui.py.

Uses disposable repositories only. Build jw first with cargo build.
"""

import errno
import atexit
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import shlex
import shutil
import struct
import subprocess
import tempfile
import termios
import time
import sys
import json

import pyte


class Terminal:
    def __init__(self, command, cwd, env, columns=120, lines=36):
        self.master, self.slave = pty.openpty()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", lines, columns, 0, 0))
        self.original_mode = termios.tcgetattr(self.slave)
        self.screen = pyte.Screen(columns, lines)
        self.screen.write_process_input = lambda data: os.write(self.master, data.encode())
        self.stream = pyte.ByteStream(self.screen)
        self.raw = bytearray()
        def child_terminal():
            os.setsid()
            fcntl.ioctl(self.slave, termios.TIOCSCTTY, 0)

        driver = ('before=$(stty -g); "$@"; command_result=$?; after=$(stty -g); '
                  'if [ "$before" != "$after" ]; then echo TERMINAL_NOT_RESTORED; exit 99; fi; '
                  'echo TERMINAL_RESTORED; exit "$command_result"')
        command = [shutil.which("bash"), "--noprofile", "--norc", "-c", driver,
                   "terminal-check", *command]
        self.process = subprocess.Popen(command, cwd=cwd, env=env, stdin=self.slave,
                                        stdout=self.slave, stderr=self.slave,
                                        preexec_fn=child_terminal)
        atexit.register(self.stop_if_running)

    def stop_if_running(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait()

    @property
    def text(self):
        return "\n".join(self.screen.display)

    def pump(self, duration=0.05):
        if select.select([self.master], [], [], duration)[0]:
            try:
                data = os.read(self.master, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                return
            self.raw.extend(data)
            self.stream.feed(data)

    def wait(self, predicate, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.pump()
            if predicate(self.text):
                return self.text
            if self.process.poll() is not None:
                break
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGKILL)
        self.process.wait()
        raise AssertionError(f"Terminal condition not reached:\n{self.text}\nRaw tail: {bytes(self.raw[-2000:])!r}")

    def send(self, keys):
        os.write(self.master, keys.encode())

    def search(self, query):
        self.send("/" + "\x7f" * 100 + query + "\r")
        self.wait(lambda text: "Search —" not in text and f"search:{query}" in text)

    def finish(self):
        deadline = time.monotonic() + 15
        while self.process.poll() is None and time.monotonic() < deadline:
            self.pump()
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait()
            raise AssertionError(f"Terminal failed to exit:\n{self.text}")
        for _ in range(20):
            if not select.select([self.master], [], [], 0)[0]:
                break
            self.pump(0)
        assert self.process.returncode == 0, self.text
        assert b"TERMINAL_RESTORED" in self.raw, "terminal attributes were not restored"
        os.close(self.master)
        os.close(self.slave)


def run():
    binary = Path(__file__).resolve().parents[1] / "target/debug/jw"
    assert binary.is_file(), "run cargo build first"
    with tempfile.TemporaryDirectory(prefix="jw-terminal-") as temporary:
        root = Path(temporary)
        repo = root / "repo with spaces"
        config = root / "config"
        (config / "jj-waltz").mkdir(parents=True)
        (config / "jj-waltz/config.toml").write_text('[trunk]\nrevset = "root()"\n')
        env = dict(os.environ, TERM="xterm-256color", XDG_CONFIG_HOME=str(config),
                   XDG_STATE_HOME=str(root / "state"))
        env.pop("NO_COLOR", None)
        jj = shutil.which("jj")
        assert jj

        def command(args, cwd=repo):
            return subprocess.run(args, cwd=cwd, env=env, check=True, capture_output=True, text=True)

        command([jj, "git", "init", str(repo)], root)
        for index in range(10):
            command([str(binary), "add", f"feature-{index:02}", "--at", "root()", "--no-links"])

        started = time.monotonic()
        terminal = Terminal([str(binary), "ui"], repo, env)
        terminal.wait(lambda text: "workspace" in text.lower())
        first_frame = time.monotonic() - started
        terminal.wait(lambda text: "feature-09" in text)
        loaded = time.monotonic() - started
        terminal.send("m")
        terminal.wait(lambda text: "mouse: text selection" in text)
        assert b"\x1b[?1000l" in terminal.raw, "mouse reporting remains enabled"
        terminal.pump(0.1)
        quiet_start = len(terminal.raw)
        for _ in range(5):
            terminal.pump(0.1)
        assert len(terminal.raw) == quiet_start, "idle rendering interrupts text selection"
        terminal.send("m")
        terminal.wait(lambda text: "mouse: controls" in text)
        terminal.send("?")
        terminal.wait(lambda text: "Help —" in text)
        terminal.send("\x1b")
        terminal.wait(lambda text: "Help —" not in text)
        terminal.send("q")
        terminal.finish()
        print(f"11 workspaces: first frame {first_frame:.3f}s; rows {loaded:.3f}s; help/cancel/restoration passed")

        # Test the actual generated shell function and a destination containing spaces.
        bash = shutil.which("bash")
        shell_command = (f'eval "$({shlex.quote(str(binary))} shell init bash)"; '
                         'jw ui; printf "\\nFINAL_DIRECTORY=%s\\n" "$PWD"')
        shell_env = dict(env, PATH=str(binary.parent) + os.pathsep + env["PATH"])
        terminal = Terminal([bash, "--noprofile", "--norc", "-c", shell_command], repo, shell_env)
        terminal.wait(lambda text: "feature-09" in text)
        terminal.send("/feature-09\r")
        terminal.send("\r")
        terminal.wait(lambda text: "FINAL_DIRECTORY=" in text)
        terminal.finish()
        expected = command([str(binary), "path", "feature-09"]).stdout.strip()
        assert f"FINAL_DIRECTORY={expected}" in terminal.raw.decode(errors="replace"), terminal.text
        print("Bash Enter switched to the selected workspace, including path spaces")

        terminal = Terminal([str(binary), "ui"], repo, env)
        terminal.wait(lambda text: "feature-09" in text)
        terminal.search("feature-00")
        terminal.send(" ")
        terminal.wait(lambda text: "selected:1" in text)
        terminal.search("feature-01")
        terminal.send(" ")
        terminal.wait(lambda text: "selected:2 (+1 hidden)" in text)
        terminal.send("d")
        terminal.wait(lambda text: "PERMANENT" in text and "feature-00" in text and "feature-01" in text)
        terminal.send("\x1b")
        terminal.wait(lambda text: "PERMANENT" not in text)
        assert command([str(binary), "path", "feature-00"]).stdout.strip()
        terminal.send("d")
        terminal.wait(lambda text: "PERMANENT" in text)
        terminal.send("\r")
        terminal.wait(lambda text: "Removal results" in text)
        assert "DONE feature-00" in terminal.text and "DONE feature-01" in terminal.text, terminal.text
        terminal.send("\x1b")
        terminal.wait(lambda text: "Removal results" not in text)
        terminal.send("q")
        terminal.finish()
        print("Selection survives search; cancel preserves targets; confirmed batch removes both")

        terminal = Terminal([str(binary), "ui"], repo, env, columns=80, lines=25)
        terminal.wait(lambda text: "feature-09" in text)
        terminal.send("m")
        terminal.wait(lambda text: "mouse: text selection" in text)
        terminal.send("m")
        terminal.wait(lambda text: "mouse: controls" in text)
        terminal.send("\t")
        terminal.wait(lambda text: "Details" in text)
        terminal.send("\t")
        terminal.wait(lambda text: "Details" not in text)
        terminal.send("n")
        terminal.wait(lambda text: "New workspace" in text)
        terminal.send("created-demo\r")
        terminal.wait(lambda text: "created-demo" in text and "workspace created" in text)
        terminal.send("q")
        terminal.finish()
        assert command([str(binary), "path", "created-demo"]).stdout.strip()
        print("Narrow layout details and interactive creation passed")

        command([str(binary), "add", "risky-demo", "--at", "root()", "--bookmark", "wip/risky-demo", "--no-links"])
        risky_path = Path(command([str(binary), "path", "risky-demo"]).stdout.strip())
        (risky_path / ".gitignore").write_text("private/\n")
        (risky_path / "private").mkdir()
        (risky_path / "private/data").write_text("unrecorded data\n")
        terminal = Terminal([str(binary), "ui"], repo, env)
        terminal.wait(lambda text: "risky-demo" in text)
        mouse_row, mouse_line = next((index, line) for index, line in enumerate(terminal.screen.display)
                                     if "feature-02" in line)
        mouse_column = mouse_line.index("[ ]")
        terminal.send(f"\x1b[<0;{mouse_column + 1};{mouse_row + 1}M\x1b[<0;{mouse_column + 1};{mouse_row + 1}m")
        terminal.wait(lambda text: "selected:1" in text)
        terminal.send("c")
        terminal.search("risky-demo")
        terminal.send("d")
        terminal.wait(lambda text: "PERMANENT" in text and "not recorded by JJ: private/" in text and "RISKY" in text)
        assert "RISKY" in terminal.text, terminal.text
        terminal.send("rb\r")
        terminal.wait(lambda text: "choose" in text.lower() and "Removal preview" in text)
        assert risky_path.exists(), "unacknowledged content was deleted"
        terminal.send("i\r")
        terminal.wait(lambda text: "Removal results" in text)
        assert "DONE risky-demo" in terminal.text, terminal.text
        terminal.send("\x1b")
        terminal.wait(lambda text: "Removal results" not in text)
        terminal.send("q")
        terminal.finish()
        assert not risky_path.exists()
        assert "wip/risky-demo" not in command([jj, "bookmark", "list"]).stdout
        assert json.loads((root / "state/jj-waltz/ui.json").read_text())["delete_bookmarks"] is True
        print("Mouse selection, risky override, ignored acknowledgement, bookmark toggle and persistence passed")

        # Explicitly skipping unrecorded files preserves that row while another completes.
        command([str(binary), "add", "skip-files", "remove-clean", "--at", "root()", "--no-links"])
        skip_path = Path(command([str(binary), "path", "skip-files"]).stdout.strip())
        (skip_path / ".gitignore").write_text("private/\n")
        (skip_path / "private").mkdir()
        (skip_path / "private/data").write_text("keep this\n")
        unavailable_state = root / "state-is-a-file"
        unavailable_state.write_text("cannot store preferences here\n")
        terminal = Terminal([str(binary), "ui"], repo,
                            dict(env, XDG_STATE_HOME=str(unavailable_state)))
        terminal.wait(lambda text: "skip-files" in text)
        terminal.search("skip-files")
        terminal.send(" ")
        terminal.wait(lambda text: "selected:1" in text)
        terminal.search("remove-clean")
        terminal.send(" ")
        terminal.wait(lambda text: "selected:2" in text)
        terminal.send("d")
        terminal.wait(lambda text: "Removal preview" in text and "not recorded by JJ: private/" in text)
        terminal.send("rs\r")
        terminal.wait(lambda text: "Removal results" in text)
        assert "DONE remove-clean" in terminal.text, terminal.text
        assert "SKIPPED skip-files" in terminal.text, terminal.text
        assert "save" in terminal.text.lower() and "choice" in terminal.text.lower(), terminal.text
        assert (skip_path / "private/data").read_text() == "keep this\n"
        terminal.send("\x1b")
        terminal.wait(lambda text: "Removal results" not in text and "selected:1" in text)
        terminal.send("q")
        terminal.finish()
        print("Explicit skip retains files and selection; unavailable preferences do not block removal")

        for default_command, expected_text in [("list", "feature-02"), ("help", "Usage:")]:
            (config / "jj-waltz/config.toml").write_text(f'default_command = "{default_command}"\n[trunk]\nrevset = "root()"\n')
            terminal = Terminal([str(binary)], repo, env)
            terminal.wait(lambda text: expected_text in text)
            terminal.finish()
            assert b"\x1b[?1049h" not in terminal.raw
        (config / "jj-waltz/config.toml").write_text('[trunk]\nrevset = "root()"\n')
        print("Bare jw respects configured list/help defaults")

        # Fifty total workspaces. Log queries and delay JJ to exercise input during loading.
        existing = len(command([jj, "workspace", "list", "-T", 'name ++ "\\n"']).stdout.splitlines())
        for index in range(50 - existing):
            command([str(binary), "add", f"scale-{index:02}", "--at", "root()", "--no-links"])
        shim_dir = root / "shim"
        shim_dir.mkdir()
        query_log = root / "queries.log"
        shim = shim_dir / "jj"
        shim.write_text(f'#!{sys.executable}\nimport os, sys, time\n'
                        f'with open({str(query_log)!r}, "a") as log: log.write("query\\n")\n'
                        'time.sleep(0.02)\n'
                        f'os.execv({jj!r}, [{jj!r}, *sys.argv[1:]])\n')
        shim.chmod(0o755)
        slow_env = dict(env, PATH=str(shim_dir) + os.pathsep + env["PATH"])
        started = time.monotonic()
        terminal = Terminal([str(binary), "ui"], repo, slow_env)
        terminal.wait(lambda text: "Checking the current repository" in text)
        first_frame = time.monotonic() - started
        input_started = time.monotonic()
        terminal.send("?")
        terminal.wait(lambda text: "Help —" in text)
        input_latency = time.monotonic() - input_started
        terminal.send("\x1b")
        terminal.wait(lambda text: "Help —" not in text)
        terminal.wait(lambda text: "50 workspaces" in text)
        load_duration = time.monotonic() - started
        terminal.send("q")
        terminal.finish()
        query_count = len(query_log.read_text().splitlines())
        assert input_latency < 0.5, f"input blocked for {input_latency:.3f}s"
        print(f"50 workspaces (20ms artificial delay/query): first frame {first_frame:.3f}s; "
              f"input {input_latency:.3f}s; load {load_duration:.3f}s; {query_count} JJ queries")


if __name__ == "__main__":
    run()
