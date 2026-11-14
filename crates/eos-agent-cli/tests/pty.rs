//! `eos-agent chat` on a pseudo-terminal against the mock API: the question
//! dialog takes the keyboard from the prompt line, and Esc ends the turn.

#![cfg(unix)]

use std::io::{Read as _, Write as _};
use std::os::fd::{FromRawFd as _, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use eos_agent_mock_api::{MockApi, Reply};

const ASK: &str = r#"{"questions":[{"question":"Which colour?","header":"Colour","multiSelect":false,
    "options":[{"label":"Red","description":"warm"},{"label":"Blue","description":"cool"}]}]}"#;

/// A pseudo-terminal: the master side for the test, the slave for the child.
#[allow(unsafe_code)]
fn open_pty() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize { ws_row: 40, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: openpty fills the two descriptors it is given; the rest may be null.
    let rc = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) };
    assert_eq!(rc, 0, "openpty");
    // SAFETY: both descriptors were just opened and are owned by nobody else.
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

struct Session {
    child: Child,
    input: std::fs::File,
    screen: Arc<Mutex<String>>,
    _home: TempDir,
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Session {
    fn start(name: &str, mock: &MockApi) -> Self {
        let home = std::env::temp_dir().join(format!("eos-cli-pty-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("project")).expect("dirs");
        let (master, slave) = open_pty();
        let child = Command::new(env!("CARGO_BIN_EXE_eos-agent"))
            .args(["chat", "--no-mcp", "--api-key-env", "EOS_TEST_KEY", "--base-url", &mock.base_url()])
            .arg("--cwd")
            .arg(home.join("project"))
            .env("EOS_TEST_KEY", "k")
            .env("HOME", &home)
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .env_remove("ANTHROPIC_BASE_URL")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().expect("dup")))
            .stdout(Stdio::from(slave.try_clone().expect("dup")))
            .stderr(Stdio::from(slave))
            .spawn()
            .expect("eos-agent chat");
        let input = std::fs::File::from(master.try_clone().expect("dup"));
        let mut output = std::fs::File::from(master);
        let screen = Arc::new(Mutex::new(String::new()));
        let shared = Arc::clone(&screen);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = output.read(&mut buf) {
                if n == 0 {
                    break;
                }
                shared.lock().unwrap_or_else(PoisonError::into_inner).push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        Self { child, input, screen, _home: TempDir(home) }
    }

    fn screen(&self) -> String {
        self.screen.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Waits until `text` has been shown `count` times in all.
    fn wait_for(&self, text: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.screen().matches(text).count() < count {
            assert!(Instant::now() < deadline, "never saw {text:?} ×{count}:\n{}", self.screen());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn type_keys(&mut self, keys: &str) {
        self.input.write_all(keys.as_bytes()).expect("type");
        self.input.flush().expect("flush");
        std::thread::sleep(Duration::from_millis(150));
    }

    fn quit(mut self) {
        self.type_keys("\r");
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.child.try_wait().expect("wait").is_none() {
            assert!(Instant::now() < deadline, "chat did not end:\n{}", self.screen());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn record(seen: &Arc<Mutex<Vec<String>>>, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body).into_owned();
    seen.lock().unwrap_or_else(PoisonError::into_inner).push(text.clone());
    text
}

/// A question that arrives while the prompt waits for the next line takes
/// the keyboard: the digit typed goes to the dialog, the half-typed line
/// comes back afterwards and goes on.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_takes_the_keyboard_from_the_prompt() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let mock = MockApi::responder(([127, 0, 0, 1], 0).into(), move |body| {
        let text = record(&log, body);
        if text.contains("abc") {
            Reply::text("got abc")
        } else if text.contains(r#""tool_use_id":"toolu_ask""#) {
            Reply::text("thanks")
        } else if text.contains("<result>sub done</result>") {
            Reply::tool_use("toolu_ask", "AskUserQuestion", ASK)
        } else if text.contains(r#""tool_use_id":"toolu_bg""#) {
            Reply::text("launched")
        } else if !text.contains("start helper") {
            Reply::text("sub done").slowed(Duration::from_millis(1500))
        } else {
            let call = r#"{"description":"Helper","prompt":"SUBTASK","run_in_background":true}"#;
            Reply::tool_use("toolu_bg", "Agent", call)
        }
    })
    .await
    .expect("mock");
    let mut chat = Session::start("takeover", &mock);
    chat.wait_for("> ", 1);
    chat.type_keys("start helper\r");
    chat.wait_for("launched", 1);
    chat.wait_for("> ", 2);
    chat.type_keys("ab");
    chat.wait_for("Chat about this", 1);
    chat.type_keys("1");
    chat.wait_for("thanks", 1);
    chat.wait_for("> ab", 2);
    chat.type_keys("c\r");
    chat.wait_for("got abc", 1);
    chat.quit();
    let seen = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert!(seen.iter().any(|b| b.contains(r#"\"Which colour?\"=\"Red\""#)), "the digit answered the question");
    assert!(seen.iter().any(|b| b.contains(r#""text":"abc""#)), "the line kept what was typed before");
}

/// Esc in the dialog rejects the call and ends the turn: the model is not
/// asked again until the next line.
#[tokio::test(flavor = "multi_thread")]
async fn esc_in_the_dialog_ends_the_turn() {
    let mock =
        MockApi::start([Reply::tool_use("toolu_q", "AskUserQuestion", ASK), Reply::text("after")]).await.expect("mock");
    let mut chat = Session::start("esc", &mock);
    chat.wait_for("> ", 1);
    chat.type_keys("ask me\r");
    chat.wait_for("Chat about this", 1);
    chat.type_keys("\x1b");
    chat.wait_for("[interrupted]", 1);
    chat.wait_for("> ", 2);
    assert_eq!(mock.requests().len(), 1, "no request after Esc");
    chat.type_keys("go on\r");
    chat.wait_for("after", 1);
    let next = String::from_utf8_lossy(&mock.requests()[1].body).into_owned();
    assert!(next.contains("STOP what you are doing and wait for the user"), "{next}");
    assert!(next.contains("[Request interrupted by user for tool use]"), "{next}");
    chat.quit();
}
