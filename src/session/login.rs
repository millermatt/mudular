//! Auto-login: the fixed opening exchange a MUD asks for before play
//! starts (docs/ARCHITECTURE.md §10).
//!
//! Sans-IO, like the rest of the pipeline: it is fed the lines and prompts
//! the server sends and returns what to send back, so the whole thing is
//! testable without a socket.
//!
//! The password never comes from YAML — it is read from the OS keyring by
//! the caller and handed in here (§13). What keeps it from leaking is the
//! state machine: each step fires at most once, the steps only run forward,
//! and anything the player types disarms the machine for the rest of the
//! connection. A later `Password:` in chat therefore has nothing to fire.

use anyhow::{Context, Result};
use regex::Regex;

/// A MUD that asks for a name asks in one of a handful of ways. Overridable
/// per profile, because the ones that don't are not rare enough to ignore.
const DEFAULT_NAME_PROMPT: &str =
    r"(?i)(what is your name|by what name|character name|enter your name|^\s*(login|name)\s*[:?])";
const DEFAULT_PASSWORD_PROMPT: &str = r"(?i)password";

/// How far through the exchange we are. Never goes backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Name,
    Password,
    Done,
}

pub struct Autologin {
    name: String,
    /// `None` when the keyring had nothing for this profile: the name is
    /// still sent, and the player types the password themselves.
    password: Option<String>,
    name_prompt: Regex,
    password_prompt: Regex,
    step: Step,
}

/// What the session should do with a line it just fed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginAction {
    /// Send this to the server. Never echoed to the pane: it is either the
    /// character name (which the server echoes itself) or the password.
    Send(String),
    /// Say why nothing was sent, in the pane.
    Notice(String),
}

impl Autologin {
    pub fn new(
        name: String,
        password: Option<String>,
        name_prompt: Option<&str>,
        password_prompt: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            name,
            password,
            name_prompt: compile(name_prompt.unwrap_or(DEFAULT_NAME_PROMPT), "name_prompt")?,
            password_prompt: compile(
                password_prompt.unwrap_or(DEFAULT_PASSWORD_PROMPT),
                "password_prompt",
            )?,
            step: Step::Name,
        })
    }

    /// Feeds one line or prompt, already stripped of ANSI. Login prompts
    /// often arrive as prompts rather than lines (no newline before the
    /// server waits), so both are fed through here.
    pub fn on_line(&mut self, text: &str) -> Option<LoginAction> {
        match self.step {
            Step::Name if self.name_prompt.is_match(text) => {
                self.step = Step::Password;
                Some(LoginAction::Send(self.name.clone()))
            }
            Step::Password if self.password_prompt.is_match(text) => self.send_password(),
            _ => None,
        }
    }

    /// The server turning on ECHO *is* a password prompt, whatever words it
    /// used — the one signal that doesn't depend on guessing the wording.
    pub fn on_echo_mask(&mut self, masked: bool) -> Option<LoginAction> {
        match (self.step, masked) {
            (Step::Password, true) => self.send_password(),
            _ => None,
        }
    }

    /// Stops the machine for the rest of the connection. Called the moment
    /// the player types anything: past that point the login either worked
    /// or they are handling it, and either way an automated password send
    /// would be going somewhere nobody asked for.
    pub fn disarm(&mut self) {
        self.step = Step::Done;
    }

    fn send_password(&mut self) -> Option<LoginAction> {
        self.step = Step::Done;
        match self.password.take() {
            Some(password) => Some(LoginAction::Send(password)),
            None => Some(LoginAction::Notice(
                "auto-login: no password in the keyring for this profile \
                 (type it below and you'll be offered to save it)"
                    .to_string(),
            )),
        }
    }
}

fn compile(pattern: &str, field: &str) -> Result<Regex> {
    Regex::new(pattern).with_context(|| format!("login.{field}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login() -> Autologin {
        Autologin::new("Kestrel".into(), Some("hunter2".into()), None, None).unwrap()
    }

    fn sends(action: Option<LoginAction>) -> Option<String> {
        match action {
            Some(LoginAction::Send(line)) => Some(line),
            _ => None,
        }
    }

    #[test]
    fn answers_the_name_then_the_password() {
        let mut login = login();
        assert_eq!(login.on_line("Welcome to HercMUD!"), None);
        assert_eq!(
            sends(login.on_line("By what name are you known? ")).as_deref(),
            Some("Kestrel")
        );
        assert_eq!(
            sends(login.on_line("Password: ")).as_deref(),
            Some("hunter2")
        );
    }

    /// The wording of a password prompt varies; the ECHO negotiation that
    /// hides your typing does not.
    #[test]
    fn a_masked_prompt_is_a_password_prompt_whatever_it_says() {
        let mut login = login();
        login.on_line("What is your name?");

        assert_eq!(
            sends(login.on_echo_mask(true)).as_deref(),
            Some("hunter2"),
            "the server masking input is the prompt"
        );
    }

    #[test]
    fn masking_before_the_name_is_answered_sends_nothing() {
        let mut login = login();
        assert_eq!(login.on_echo_mask(true), None);
    }

    /// The whole safety argument: nothing fires twice, so a `Password:` in
    /// chat an hour later has no armed step to trigger.
    #[test]
    fn each_step_fires_once_and_the_machine_never_rearms() {
        let mut login = login();
        login.on_line("What is your name?");
        login.on_line("Password: ");

        assert_eq!(login.on_line("Password: "), None);
        assert_eq!(login.on_line("What is your name?"), None);
        assert_eq!(login.on_echo_mask(true), None);
    }

    #[test]
    fn typing_anything_disarms_it() {
        let mut login = login();
        login.on_line("What is your name?");
        login.disarm();

        assert_eq!(
            login.on_line("Password: "),
            None,
            "the player took over; nothing may be sent on their behalf"
        );
    }

    /// A profile with no stored password still gets its name sent, and is
    /// told why the rest didn't happen rather than silently stalling.
    #[test]
    fn an_empty_keyring_answers_the_name_and_explains_itself() {
        let mut login = Autologin::new("Kestrel".into(), None, None, None).unwrap();
        assert_eq!(
            sends(login.on_line("What is your name?")).as_deref(),
            Some("Kestrel")
        );

        match login.on_line("Password: ") {
            Some(LoginAction::Notice(text)) => {
                assert!(text.contains("no password in the keyring"), "{text}")
            }
            other => panic!("expected a notice, got {other:?}"),
        }
    }

    #[test]
    fn custom_prompts_replace_the_defaults() {
        let mut login = Autologin::new(
            "Kestrel".into(),
            Some("hunter2".into()),
            Some(r"^Who goes there\?"),
            Some(r"^Speak the word"),
        )
        .unwrap();

        assert_eq!(login.on_line("What is your name?"), None);
        assert_eq!(
            sends(login.on_line("Who goes there? ")).as_deref(),
            Some("Kestrel")
        );
        assert_eq!(
            sends(login.on_line("Speak the word: ")).as_deref(),
            Some("hunter2")
        );
    }

    #[test]
    fn a_malformed_prompt_pattern_is_an_error_not_a_panic() {
        // Deliberately not `expect_err`: that needs `Debug` on the Ok side,
        // and a derived `Debug` here would print the password.
        let err = match Autologin::new("Kestrel".into(), None, Some("(unclosed"), None) {
            Err(err) => err,
            Ok(_) => panic!("an invalid regex must be rejected"),
        };
        assert!(err.to_string().contains("login.name_prompt"), "{err}");
    }
}
