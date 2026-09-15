//! Why a Host that is not `ok` did not work, inferred from ssh's stderr.
//!
//! This is the one place output text is consulted at all, and it never changes
//! a Host's status or the run's exit code. See ADR-0005.

/// A best-effort explanation of a Host's failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// ssh could not authenticate, or refused the host key.
    Auth,
    /// ssh could not resolve the Host's name.
    Dns,
    /// ssh reached a name but could not open a connection.
    Connect,
}

impl Cause {
    pub fn as_str(self) -> &'static str {
        match self {
            Cause::Auth => "auth",
            Cause::Dns => "dns",
            Cause::Connect => "connect",
        }
    }

    /// Reads ssh's stderr and guesses. Returns `None` when nothing matches,
    /// which is the honest answer for a failure whose text says nothing useful.
    pub fn infer(stderr: &[u8]) -> Option<Cause> {
        let text = String::from_utf8_lossy(stderr);
        let text = text.to_ascii_lowercase();

        // Authentication is checked first: "Permission denied (publickey)" is
        // the common case, and its wording shares no markers with the others.
        const AUTH: &[&str] = &[
            "permission denied",
            "authentication failed",
            "too many authentication failures",
            "no supported authentication methods",
            "host key verification failed",
            "remote host identification has changed",
            "no matching host key",
        ];
        const DNS: &[&str] = &[
            "could not resolve hostname",
            "name or service not known",
            "nodename nor servname provided",
            "temporary failure in name resolution",
            "no address associated with hostname",
        ];
        const CONNECT: &[&str] = &[
            "connection refused",
            "connection timed out",
            "connection closed by",
            "connection reset by",
            "no route to host",
            "network is unreachable",
            "operation timed out",
            "broken pipe",
        ];

        for (patterns, cause) in [
            (AUTH, Cause::Auth),
            (DNS, Cause::Dns),
            (CONNECT, Cause::Connect),
        ] {
            if patterns.iter().any(|pattern| text.contains(pattern)) {
                return Some(cause);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::Cause;

    fn infer(text: &str) -> Option<Cause> {
        Cause::infer(text.as_bytes())
    }

    #[test]
    fn real_ssh_failures_are_classified() {
        // Wording taken from OpenSSH 10.5p1.
        assert_eq!(
            infer("charon@127.0.0.1: Permission denied (publickey).\n"),
            Some(Cause::Auth)
        );
        assert_eq!(
            infer(
                "Received disconnect from 127.0.0.1 port 2222:2: Too many authentication failures\n"
            ),
            Some(Cause::Auth)
        );
        assert_eq!(
            infer(
                "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\nHost key verification failed.\n"
            ),
            Some(Cause::Auth)
        );
        assert_eq!(
            infer("ssh: Could not resolve hostname nope: Name or service not known\n"),
            Some(Cause::Dns)
        );
        assert_eq!(
            infer("ssh: connect to host 127.0.0.1 port 22: Connection refused\n"),
            Some(Cause::Connect)
        );
        assert_eq!(
            infer("ssh: connect to host example port 22: Connection timed out\n"),
            Some(Cause::Connect)
        );
        assert_eq!(
            infer("ssh: connect to host example port 22: No route to host\n"),
            Some(Cause::Connect)
        );
    }

    #[test]
    fn unrelated_failures_get_no_cause() {
        assert_eq!(infer("bash: nope: command not found\n"), None);
        assert_eq!(infer(""), None);
    }

    #[test]
    fn a_remote_commands_own_permission_error_looks_like_an_auth_failure() {
        // ssh folds the remote command's stderr together with its own (a
        // consequence noted in ADR-0001), so a remote `Permission denied` is
        // indistinguishable from an authentication failure here. That is why
        // `cause` is advisory and never changes a status or an exit code.
        assert_eq!(
            infer("cat: /etc/shadow: Permission denied\n"),
            Some(Cause::Auth)
        );
    }

    #[test]
    fn classification_is_case_insensitive() {
        assert_eq!(infer("PERMISSION DENIED\n"), Some(Cause::Auth));
        assert_eq!(infer("Connection Refused\n"), Some(Cause::Connect));
    }

    #[test]
    fn the_cause_is_a_short_stable_word() {
        assert_eq!(Cause::Auth.as_str(), "auth");
        assert_eq!(Cause::Dns.as_str(), "dns");
        assert_eq!(Cause::Connect.as_str(), "connect");
    }
}
