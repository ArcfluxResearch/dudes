//! The filename and content shapes that mark text as secret. Shared by every view that shows
//! file or tool text, so the rules stay in one place.

const CREDENTIAL_KEYS: &[&[u8]] = &[
    b"api_key",
    b"api-key",
    b"apikey",
    b"access_token",
    b"access-token",
    b"accesstoken",
    b"client_secret",
    b"client-secret",
    b"clientsecret",
    b"password",
];

/// File names whose text is never shown, matched on the last path component without regard to
/// case. `Exact` is the whole name, `Suffix` its end, and `Between` its start and end.
enum Name {
    Exact(&'static str),
    Suffix(&'static str),
    Between(&'static str, &'static str),
}

const SENSITIVE_NAMES: &[Name] = &[
    Name::Exact("id_rsa"),
    Name::Exact("id_ed25519"),
    Name::Exact("credentials"),
    Name::Exact("credentials.json"),
    Name::Exact(".npmrc"),
    Name::Exact(".netrc"),
    Name::Exact(".pgpass"),
    Name::Exact(".git-credentials"),
    Name::Exact(".pypirc"),
    Name::Suffix(".tfvars"),
    Name::Suffix(".tfstate"),
    Name::Suffix(".keystore"),
    Name::Between("service-account", ".json"),
];

fn sensitive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SENSITIVE_NAMES.iter().any(|rule| match rule {
        Name::Exact(exact) => lower == *exact,
        Name::Suffix(suffix) => lower.ends_with(suffix),
        Name::Between(start, end) => {
            lower.len() >= start.len() + end.len()
                && lower.starts_with(start)
                && lower.ends_with(end)
        }
    })
}

pub(crate) fn sensitive_path(path: &str) -> bool {
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if part.eq_ignore_ascii_case(".env")
            || part
                .get(..5)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(".env."))
            || parts.peek().is_some()
                && (part.eq_ignore_ascii_case(".ssh") || part.eq_ignore_ascii_case(".aws"))
        {
            return true;
        }
        if parts.peek().is_none()
            && (part.rsplit_once('.').is_some_and(|(_, suffix)| {
                ["pem", "key", "p12", "pfx"]
                    .iter()
                    .any(|ext| suffix.eq_ignore_ascii_case(ext))
            }) || sensitive_name(part))
        {
            return true;
        }
    }
    false
}

pub(crate) fn contains_secret(bytes: &[u8]) -> bool {
    (0..bytes.len()).any(|start| {
        private_key(&bytes[start..])
            || token(bytes, start)
            || credential_value(bytes, start)
            || url_password(&bytes[start..])
    })
}

/// `://user:password@host`, with a non-empty user and password and no space, slash or second `@`
/// before the host.
fn url_password(bytes: &[u8]) -> bool {
    let Some(rest) = bytes.strip_prefix(b"://") else {
        return false;
    };
    let userinfo =
        |byte: &u8| !byte.is_ascii_whitespace() && !matches!(byte, b'/' | b'@' | b'"' | b'\'');
    let user = rest
        .iter()
        .take_while(|&byte| userinfo(byte) && *byte != b':')
        .count();
    if user == 0 || rest.get(user) != Some(&b':') {
        return false;
    }
    let rest = &rest[user + 1..];
    let password = rest.iter().take_while(|&byte| userinfo(byte)).count();
    password > 0
        && rest.get(password) == Some(&b'@')
        && rest.get(password + 1).is_some_and(|&host| word(host))
}

fn private_key(bytes: &[u8]) -> bool {
    let Some(rest) = bytes.strip_prefix(b"-----BEGIN ") else {
        return false;
    };
    let end = rest
        .iter()
        .position(|byte| !byte.is_ascii_uppercase() && !byte.is_ascii_digit() && *byte != b' ')
        .unwrap_or(rest.len());
    rest[..end].ends_with(b"PRIVATE KEY") && rest[end..].starts_with(b"-----")
}

fn word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn token(bytes: &[u8], start: usize) -> bool {
    if start > 0 && word(bytes[start - 1]) {
        return false;
    }
    let rest = &bytes[start..];
    if rest.starts_with(b"AKIA") || rest.starts_with(b"ASIA") {
        return rest.get(4..20).is_some_and(|suffix| {
            suffix
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        }) && rest.get(20).is_none_or(|&byte| !word(byte));
    }
    let (prefix_len, allow_hyphen) = if rest.starts_with(b"sk-") {
        (3, true)
    } else if [&b"sk_live_"[..], b"sk_test_", b"rk_live_"]
        .iter()
        .any(|prefix| rest.starts_with(prefix))
    {
        (8, false)
    } else if rest.starts_with(b"gh")
        && rest.get(2).is_some_and(|kind| b"pousr".contains(kind))
        && rest.get(3) == Some(&b'_')
    {
        (4, false)
    } else {
        return false;
    };
    let suffix = &rest[prefix_len..];
    let length = suffix
        .iter()
        .take(256)
        .take_while(|&&byte| {
            byte.is_ascii_alphanumeric() || allow_hyphen && matches!(byte, b'_' | b'-')
        })
        .count();
    (20..=255).contains(&length) && suffix.get(length).is_none_or(|&byte| !word(byte))
}

fn credential_value(bytes: &[u8], start: usize) -> bool {
    if start > 0 && (word(bytes[start - 1]) || matches!(bytes[start - 1], b'\'' | b'"')) {
        return false;
    }
    let mut rest = &bytes[start..];
    let quote = rest
        .first()
        .copied()
        .filter(|byte| matches!(byte, b'\'' | b'"'));
    if quote.is_some() {
        rest = &rest[1..];
    }
    let Some(key) = CREDENTIAL_KEYS.iter().find(|key| {
        rest.get(..key.len())
            .is_some_and(|value| value.eq_ignore_ascii_case(key))
    }) else {
        return false;
    };
    rest = &rest[key.len()..];
    if let Some(quote) = quote {
        if rest.first() != Some(&quote) {
            return false;
        }
        rest = &rest[1..];
    }
    rest = horizontal_space(rest);
    if !matches!(rest.first(), Some(b':' | b'=')) {
        return false;
    }
    rest = horizontal_space(&rest[1..]);
    let Some(&quote @ (b'\'' | b'"')) = rest.first() else {
        return false;
    };
    rest = &rest[1..];
    let Some(end) = rest
        .iter()
        .position(|&byte| byte == quote || matches!(byte, b'\r' | b'\n'))
    else {
        return false;
    };
    rest[end] == quote
        && std::str::from_utf8(&rest[..end]).is_ok_and(|value| value.chars().take(8).count() == 8)
}

fn horizontal_space(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(bytes.len());
    &bytes[start..]
}
