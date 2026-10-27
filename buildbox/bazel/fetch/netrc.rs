//! `.netrc` files, whose credentials downloads use, as Bazel's do.

use std::path::PathBuf;

/// A login and password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub login: String,
    pub password: String,
}

impl Credentials {
    /// The `Authorization` header that sends them.
    pub fn basic_authorization(&self) -> String {
        let pair = format!("{}:{}", self.login, self.password);
        format!("Basic {}", data_encoding::BASE64.encode(pair.as_bytes()))
    }
}

/// The credentials of a `.netrc` file, by machine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Netrc {
    machines: Vec<(String, Credentials)>,
    default: Option<Credentials>,
}

impl Netrc {
    /// The `.netrc` file `$NETRC` names, or `$HOME/.netrc`, in the
    /// environment `env` gives, if there is one.
    pub fn load(env: impl Fn(&str) -> Option<String>) -> Netrc {
        let path = match env("NETRC") {
            Some(path) => PathBuf::from(path),
            None => match env("HOME") {
                Some(home) => PathBuf::from(home).join(".netrc"),
                None => return Netrc::default(),
            },
        };
        match std::fs::read_to_string(path) {
            Ok(text) => Netrc::parse(&text),
            Err(_) => Netrc::default(),
        }
    }

    /// Parse a `.netrc` file: `machine`, `login` and `password` tokens,
    /// with `default` for every other machine. `account` and `macdef`
    /// entries are skipped.
    pub fn parse(text: &str) -> Netrc {
        let mut netrc = Netrc::default();
        let mut tokens = Tokens::new(text);
        let mut current: Option<(Option<String>, Credentials)> = None;
        while let Some(token) = tokens.next() {
            match token.as_str() {
                "machine" | "default" => {
                    netrc.finish(current.take());
                    let machine = match token.as_str() {
                        "machine" => Some(tokens.next().unwrap_or_default()),
                        _ => None,
                    };
                    let empty = Credentials { login: String::new(), password: String::new() };
                    current = Some((machine, empty));
                }
                "login" => set(&mut current, tokens.next(), |credentials, login| credentials.login = login),
                "password" => set(&mut current, tokens.next(), |credentials, password| credentials.password = password),
                "account" => {
                    tokens.next();
                }
                "macdef" => tokens.skip_macro(),
                _ => {}
            }
        }
        netrc.finish(current);
        netrc
    }

    fn finish(&mut self, entry: Option<(Option<String>, Credentials)>) {
        match entry {
            Some((Some(machine), credentials)) => self.machines.push((machine, credentials)),
            Some((None, credentials)) => self.default = Some(credentials),
            None => {}
        }
    }

    /// The credentials for `host`: its machine's first entry, or the default.
    pub fn credentials(&self, host: &str) -> Option<&Credentials> {
        let machine = self.machines.iter().find(|(machine, _)| machine == host);
        machine.map(|(_, credentials)| credentials).or(self.default.as_ref())
    }
}

fn set(current: &mut Option<(Option<String>, Credentials)>, value: Option<String>, apply: impl FnOnce(&mut Credentials, String)) {
    if let (Some((_, credentials)), Some(value)) = (current.as_mut(), value) {
        apply(credentials, value);
    }
}

/// The whitespace-separated tokens of a `.netrc` file, with quoted ones
/// unquoted.
struct Tokens<'a> {
    lines: std::str::Lines<'a>,
    pending: std::vec::IntoIter<String>,
}

impl<'a> Tokens<'a> {
    fn new(text: &'a str) -> Tokens<'a> {
        Tokens {
            lines: text.lines(),
            pending: Vec::new().into_iter(),
        }
    }

    /// Skip a macro definition: the rest of its line, then lines up to
    /// an empty one.
    fn skip_macro(&mut self) {
        self.pending = Vec::new().into_iter();
        for line in self.lines.by_ref() {
            if line.trim().is_empty() {
                break;
            }
        }
    }
}

impl Iterator for Tokens<'_> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        loop {
            if let Some(token) = self.pending.next() {
                return Some(token);
            }
            let line = self.lines.next()?;
            if line.trim_start().starts_with('#') {
                continue;
            }
            let tokens: Vec<String> = line.split_whitespace().map(|token| token.trim_matches('"').to_string()).collect();
            self.pending = tokens.into_iter();
        }
    }
}

/// The host of an HTTP URL.
pub fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    Some(host.split(':').next().unwrap_or(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netrc_files_give_credentials_by_machine() {
        let netrc = Netrc::parse("machine example.com login me password secret\n# comment\nmacdef init\ncd /\n\ndefault\n  login anon password guess\n");
        let credentials = netrc.credentials("example.com").unwrap();
        assert_eq!((credentials.login.as_str(), credentials.password.as_str()), ("me", "secret"));
        assert_eq!(credentials.basic_authorization(), "Basic bWU6c2VjcmV0");
        assert_eq!(netrc.credentials("other.com").map(|credentials| credentials.login.as_str()), Some("anon"));
    }

    #[test]
    fn hosts_come_from_urls() {
        assert_eq!(url_host("https://user@example.com:8080/a?b"), Some("example.com"));
        assert_eq!(url_host("file:///tmp/x"), Some(""));
    }
}
