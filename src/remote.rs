/// A string already escaped for a POSIX shell.
///
/// Cargo flags travel from local `argv` into a command line that a remote shell parses,
/// which is exactly where an unescaped argument turns into command injection. The only
/// constructor is [`Quoted::new`], and the remote command is assembled purely from
/// `Quoted` values, so forgetting to escape something is a compile error rather than a
/// security hole.
///
/// ```
/// let flag = Quoted::new("--features a b");
/// assert_eq!(flag.as_str(), "'--features a b'");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quoted(String);

impl Quoted {
    /// Wraps the whole argument in single quotes, which suppress every shell
    /// expansion. A literal single quote cannot appear inside single quotes, so each
    /// one is closed, escaped, and reopened — `it's` becomes `'it'\''s'`.
    pub fn new(raw: &str) -> Quoted {
        Quoted(format!("'{}'", raw.replace('\'', r"'\''")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Runs `echo` in a real shell with `arg` already quoted, and returns what the
    /// shell actually produced. This is the only way to prove the escaping works —
    /// comparing against a hand-written expected string only tests our own opinion.
    fn round_trip(arg: &str) -> String {
        let script = format!("printf '%s' {}", Quoted::new(arg).as_str());
        let out = Command::new("sh").arg("-c").arg(&script).output().unwrap();
        assert!(out.status.success(), "shell rejected: {script}");
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn plain_argument_is_unchanged_after_the_shell_sees_it() {
        assert_eq!(round_trip("--release"), "--release");
    }

    #[test]
    fn spaces_survive() {
        assert_eq!(round_trip("--features foo bar"), "--features foo bar");
    }

    #[test]
    fn single_quotes_survive() {
        assert_eq!(round_trip("it's"), "it's");
        assert_eq!(round_trip("'"), "'");
        assert_eq!(round_trip("a'b'c"), "a'b'c");
    }

    #[test]
    fn shell_metacharacters_are_never_executed() {
        assert_eq!(round_trip("$(id)"), "$(id)");
        assert_eq!(round_trip("`id`"), "`id`");
        assert_eq!(round_trip("; rm -rf /"), "; rm -rf /");
        assert_eq!(round_trip("$HOME"), "$HOME");
        assert_eq!(round_trip("a|b&c"), "a|b&c");
    }

    #[test]
    fn empty_argument_stays_a_single_empty_argument() {
        assert_eq!(Quoted::new("").as_str(), "''");
        assert_eq!(round_trip(""), "");
    }
}
