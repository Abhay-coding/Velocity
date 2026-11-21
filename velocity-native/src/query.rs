use memchr::memmem::Finder;
use velocity_core::{VelocityError, VelocityResult};

pub(crate) enum Term {
    Name(Finder<'static>),
    Path(Finder<'static>),
    Extension(String),
}

impl Term {
    pub(crate) fn parse(text: &str) -> VelocityResult<Vec<Self>> {
        let mut terms = Vec::new();
        let mut token = String::new();
        let mut quoted = false;
        let mut operator = false;
        for ch in text.chars().chain(Some(' ')) {
            match ch {
                '"' => quoted = !quoted,
                ch if ch.is_whitespace() && !quoted => {
                    if !token.is_empty() {
                        terms.push(Self::from_token(&token, operator)?);
                        token.clear();
                        operator = false;
                    }
                }
                ch => {
                    if !quoted && matches!(ch, ':' | '!' | '|' | '<' | '>' | '*' | '?') {
                        operator = true;
                    }
                    token.push(ch);
                }
            }
        }
        if quoted {
            return Err(VelocityError::InvalidQuery(
                "Close the quoted phrase".into(),
            ));
        }
        Ok(terms)
    }

    fn from_token(token: &str, operator: bool) -> VelocityResult<Self> {
        let key = token.to_lowercase();
        if operator {
            if let Some(extension) = key.strip_prefix("ext:") {
                let extension = extension.trim_start_matches('.');
                if !extension.is_empty() && extension.chars().all(|ch| ch.is_alphanumeric()) {
                    return Ok(Self::Extension(extension.to_owned()));
                }
                return Err(VelocityError::InvalidQuery(
                    "Use an extension such as ext:pdf".into(),
                ));
            }
            let drive_path = matches!(key.as_bytes(), [letter, b':', b'/' | b'\\', ..] if letter.is_ascii_alphabetic());
            if !drive_path || key[2..].contains([':', '!', '|', '<', '>', '*', '?']) {
                return Err(VelocityError::InvalidQuery(format!(
                    "Unsupported search expression: {token}"
                )));
            }
        }
        if key.contains(['/', '\\']) {
            Ok(Self::Path(
                Finder::new(key.replace('\\', "/").as_bytes()).into_owned(),
            ))
        } else {
            Ok(Self::Name(Finder::new(key.as_bytes()).into_owned()))
        }
    }

    pub(crate) fn matches(&self, name_key: &str, path_key: &str, is_directory: bool) -> bool {
        match self {
            Self::Name(term) => term.find(name_key.as_bytes()).is_some(),
            Self::Path(term) => term.find(path_key.as_bytes()).is_some(),
            Self::Extension(term) => {
                !is_directory
                    && name_key
                        .rsplit_once('.')
                        .is_some_and(|(stem, ext)| !stem.is_empty() && ext == term)
            }
        }
    }
}
