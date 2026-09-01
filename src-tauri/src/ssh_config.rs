use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

const MAX_CONFIG_FILES: usize = 128;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Returns concrete `Host` aliases from the user's OpenSSH config. Wildcard and
/// negated patterns are matching rules, not useful connection suggestions, so
/// they are deliberately omitted.
pub fn aliases() -> Result<Vec<String>, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set; SSH aliases are unavailable".to_string())?;
    let ssh_dir = home.join(".ssh");
    Ok(aliases_from(&ssh_dir.join("config"), &ssh_dir))
}

fn aliases_from(config_path: &Path, ssh_dir: &Path) -> Vec<String> {
    let mut aliases = BTreeSet::new();
    let mut visited = HashSet::new();
    let mut remaining = MAX_CONFIG_FILES;
    collect_aliases(
        config_path,
        ssh_dir,
        &mut aliases,
        &mut visited,
        &mut remaining,
    );
    aliases.into_iter().collect()
}

fn collect_aliases(
    path: &Path,
    ssh_dir: &Path,
    aliases: &mut BTreeSet<String>,
    visited: &mut HashSet<PathBuf>,
    remaining: &mut usize,
) {
    if *remaining == 0 {
        return;
    }
    let canonical = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(_) => return,
    };
    if !visited.insert(canonical.clone()) {
        return;
    }
    *remaining -= 1;
    if fs::metadata(&canonical)
        .ok()
        .is_none_or(|metadata| !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES)
    {
        return;
    }
    let Ok(contents) = fs::read_to_string(&canonical) else {
        return;
    };

    for line in contents.lines() {
        let Some((keyword, values)) = ssh_config_directive(line) else {
            continue;
        };
        if keyword.eq_ignore_ascii_case("host") {
            for alias in values {
                if concrete_alias(&alias) {
                    aliases.insert(alias);
                }
            }
        } else if keyword.eq_ignore_ascii_case("include") {
            for pattern in values {
                for included in expand_include_pattern(&pattern, ssh_dir) {
                    collect_aliases(&included, ssh_dir, aliases, visited, remaining);
                }
            }
        }
    }
}

fn concrete_alias(alias: &str) -> bool {
    !alias.is_empty()
        && !alias.starts_with('!')
        && !alias.starts_with('-')
        && !alias
            .chars()
            .any(|character| matches!(character, '*' | '?' | '[' | ']'))
        && !alias.chars().any(char::is_whitespace)
        && !alias.chars().any(char::is_control)
}

fn ssh_config_directive(line: &str) -> Option<(String, Vec<String>)> {
    let words = ssh_config_words(line);
    let (first, rest) = words.split_first()?;
    if let Some((keyword, value)) = first.split_once('=') {
        let mut values = Vec::with_capacity(rest.len() + usize::from(!value.is_empty()));
        if !value.is_empty() {
            values.push(value.to_string());
        }
        values.extend(rest.iter().cloned());
        Some((keyword.to_string(), values))
    } else {
        let values = rest
            .iter()
            .cloned()
            .skip_while(|value| value == "=")
            .collect();
        Some((first.clone(), values))
    }
}

fn ssh_config_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in line.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character == '#' {
            break;
        } else if character.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn expand_include_pattern(pattern: &str, ssh_dir: &Path) -> Vec<PathBuf> {
    let home = ssh_dir.parent().unwrap_or(ssh_dir);
    let expanded = if pattern == "~" {
        home.to_path_buf()
    } else if let Some(relative) = pattern.strip_prefix("~/") {
        home.join(relative)
    } else if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        ssh_dir.join(pattern)
    };

    let mut candidates = vec![PathBuf::new()];
    for component in expanded.components() {
        match component {
            Component::RootDir => candidates = vec![PathBuf::from("/")],
            Component::CurDir => {}
            Component::ParentDir => {
                for candidate in &mut candidates {
                    candidate.push("..");
                }
            }
            Component::Normal(component) => {
                let component = component.to_string_lossy();
                if component.contains(['*', '?']) {
                    let mut matches = Vec::new();
                    for parent in &candidates {
                        let Ok(entries) = fs::read_dir(parent) else {
                            continue;
                        };
                        for entry in entries.flatten() {
                            let name = entry.file_name();
                            if wildcard_matches(&component, &name.to_string_lossy()) {
                                matches.push(entry.path());
                            }
                        }
                    }
                    matches.sort();
                    candidates = matches;
                } else {
                    for candidate in &mut candidates {
                        candidate.push(component.as_ref());
                    }
                }
            }
            Component::Prefix(_) => return Vec::new(),
        }
    }
    candidates
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut pattern_index, mut value_index) = (0, 0);
    let (mut star, mut star_value) = (None, 0);
    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?' || pattern[pattern_index] == value[value_index])
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            pattern_index += 1;
            star_value = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_value += 1;
            value_index = star_value;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_ssh_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("qmux-ssh-config-{nonce}"));
        fs::create_dir_all(dir.join("config.d")).unwrap();
        dir
    }

    #[test]
    fn aliases_include_concrete_hosts_from_included_files() {
        let ssh_dir = temp_ssh_dir();
        fs::write(
            ssh_dir.join("config"),
            "Host devbox *.internal !blocked\nInclude config.d/*\nHost quoted # comment\n",
        )
        .unwrap();
        fs::write(
            ssh_dir.join("config.d/work.conf"),
            "Host buildbox staging\nInclude config\n",
        )
        .unwrap();

        assert_eq!(
            aliases_from(&ssh_dir.join("config"), &ssh_dir),
            vec!["buildbox", "devbox", "quoted", "staging"]
        );
        fs::remove_dir_all(ssh_dir).unwrap();
    }

    #[test]
    fn tokenizer_honors_quotes_escapes_and_comments() {
        assert_eq!(
            ssh_config_words(r#"Host "space alias" escaped\ alias # ignored"#),
            vec!["Host", "space alias", "escaped alias"]
        );
        assert!(wildcard_matches("*.conf", "work.conf"));
        assert!(!wildcard_matches("*.conf", "work.txt"));
        assert_eq!(
            ssh_config_directive("Host=devbox staging"),
            Some((
                "Host".to_string(),
                vec!["devbox".to_string(), "staging".to_string()]
            ))
        );
        assert_eq!(
            ssh_config_directive("Include = config.d/*"),
            Some(("Include".to_string(), vec!["config.d/*".to_string()]))
        );
    }
}
