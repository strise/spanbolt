//! Resource names and id validation with the official emulator's messages.

use tonic::Status;

pub struct DatabaseName<'a> {
    pub instance: &'a str,
    pub id: &'a str,
}

pub fn instance_from_name(name: &str) -> Result<(&str, &str), Status> {
    let parts: Vec<&str> = name.split('/').collect();
    match parts.as_slice() {
        ["projects", p, "instances", i] if !p.is_empty() && !i.is_empty() => Ok((p, i)),
        _ => Err(Status::invalid_argument(format!(
            "Invalid instance name: {name}"
        ))),
    }
}

pub fn project_from_parent(parent: &str) -> Result<&str, Status> {
    match parent.split('/').collect::<Vec<_>>().as_slice() {
        ["projects", p] if !p.is_empty() => Ok(p),
        _ => Err(Status::invalid_argument(format!(
            "Invalid project name: {parent}"
        ))),
    }
}

pub fn database_name(name: &str) -> Result<DatabaseName<'_>, Status> {
    let parts: Vec<&str> = name.splitn(6, '/').collect();
    match parts.as_slice() {
        ["projects", p, "instances", i, "databases", d]
            if !p.is_empty() && !i.is_empty() && !d.is_empty() && !d.contains('/') =>
        {
            let instance_len = "projects/".len() + p.len() + "/instances/".len() + i.len();
            Ok(DatabaseName {
                instance: &name[..instance_len],
                id: d,
            })
        }
        _ => Err(Status::invalid_argument(format!(
            "Invalid database name: {name}"
        ))),
    }
}

/// Checks a session name the way the emulator's `ParseSessionUri` does, with its messages. It is
/// lenient: each segment is `<kind>s/<id>` with any id up to the next `/` (empty included), and
/// the session id is the whole rest (`.../sessions/` and `.../sessions/a/b` pass). A failing
/// segment's message shows what is left of the name from that segment on.
pub fn validate_session_name(name: &str) -> Result<(), Status> {
    fn consume<'a>(rest: &'a str, prefix: &str) -> Option<&'a str> {
        let rest = rest.strip_prefix(prefix)?;
        let id = rest.split('/').next().unwrap_or_default();
        Some(
            rest.strip_prefix(id)
                .and_then(|r| r.strip_prefix('/'))
                .unwrap_or(rest),
        )
    }
    let mut rest = name;
    for (prefix, kind) in [
        ("projects/", "project"),
        ("instances/", "instance"),
        ("databases/", "database"),
        ("sessions/", "session"),
    ] {
        rest = consume(rest, prefix)
            .ok_or_else(|| Status::invalid_argument(format!("Invalid {kind} uri: {rest}")))?;
    }
    Ok(())
}

pub fn validate_instance_id(id: &str) -> Result<(), Status> {
    let b = id.as_bytes();
    let ok = (2..=64).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[b.len() - 1] != b'-';
    if ok {
        Ok(())
    } else {
        Err(Status::invalid_argument(format!(
            "Instance ID must start with a lowercase letter, be 2-64 characters long, contain only lowercase letters, numbers, or hyphens, and not end with a hyphen. Got: {id}"
        )))
    }
}

pub fn validate_database_id(id: &str) -> Result<(), Status> {
    let b = id.as_bytes();
    let ok = (2..=30).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
        && !matches!(b[b.len() - 1], b'-' | b'_');
    if ok {
        Ok(())
    } else {
        Err(Status::invalid_argument(format!(
            "Invalid database name. Database name must start with a lowercase letter, be 2-30 characters long, contain only lowercase letters, numbers, underscores or hyphens, and not end with an underscore or hyphen. Got: {id}"
        )))
    }
}

/// Extracts the id from `CREATE DATABASE <id>` (optionally backquoted).
pub fn parse_create_database(stmt: &str) -> Result<String, Status> {
    let trimmed = stmt.trim();
    let syntax = |col: usize, found: &str| {
        Status::invalid_argument(format!(
            "Error parsing Spanner DDL statement: {trimmed} : Syntax error on line 1, column {col}: Encountered '{found}' while parsing: identifier"
        ))
    };
    let mut words = trimmed.splitn(3, char::is_whitespace);
    let (Some(a), Some(b)) = (words.next(), words.next()) else {
        return Err(Status::invalid_argument(format!(
            "Invalid CREATE DATABASE statement: {trimmed}"
        )));
    };
    if !a.eq_ignore_ascii_case("CREATE") || !b.eq_ignore_ascii_case("DATABASE") {
        return Err(Status::invalid_argument(format!(
            "Invalid CREATE DATABASE statement: {trimmed}"
        )));
    }
    let rest = words.next().unwrap_or("").trim();
    let col = trimmed.len() - rest.len() + 1;
    if let Some(inner) = rest.strip_prefix('`') {
        return inner
            .strip_suffix('`')
            .map(str::to_string)
            .ok_or_else(|| syntax(col, "`"));
    }
    match rest.chars().next() {
        None => Err(syntax(col, "EOF")),
        Some(c) if c.is_ascii_alphabetic() || c == '_' => Ok(rest.to_string()),
        Some(c) => Err(syntax(col, &c.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded from emulator 1.5.59 (`conformance/cases/api/sessions.case`) and its `uris.cc`.
    #[test]
    fn session_names() {
        let msg = |n: &str| {
            validate_session_name(n)
                .err()
                .map(|s| s.message().to_string())
        };
        let db = "projects/p/instances/i/databases/d";
        assert_eq!(msg(&format!("{db}/sessions/s1")), None);
        assert_eq!(msg(&format!("{db}/sessions/")), None);
        assert_eq!(msg(&format!("{db}/sessions/a/b")), None);
        assert_eq!(
            msg(&format!("{db}/sessions")).as_deref(),
            Some("Invalid session uri: sessions")
        );
        assert_eq!(
            msg(&format!("{db}/session/a")).as_deref(),
            Some("Invalid session uri: session/a")
        );
        assert_eq!(msg(db).as_deref(), Some("Invalid session uri: d"));
        assert_eq!(
            msg("projects/p/instances/i/sessions/a").as_deref(),
            Some("Invalid database uri: sessions/a")
        );
        assert_eq!(
            msg("projects/p").as_deref(),
            Some("Invalid instance uri: p")
        );
        assert_eq!(msg("").as_deref(), Some("Invalid project uri: "));
    }

    #[test]
    fn ids() {
        assert!(validate_instance_id("i1").is_ok());
        assert!(validate_instance_id("a").is_err());
        assert!(validate_instance_id("Bad_Id").is_err());
        assert!(validate_instance_id("ab-").is_err());
        assert!(validate_database_id("db_1").is_ok());
        assert!(validate_database_id("Bad").is_err());
        assert!(validate_database_id(&"a".repeat(31)).is_err());
        assert_eq!(
            parse_create_database("CREATE DATABASE `my-db`")
                .ok()
                .as_deref(),
            Some("my-db")
        );
        let e = parse_create_database("CREATE DATABASE 1abc")
            .err()
            .map(|s| s.message().to_string());
        assert_eq!(
            e.as_deref(),
            Some(
                "Error parsing Spanner DDL statement: CREATE DATABASE 1abc : Syntax error on line 1, column 17: Encountered '1' while parsing: identifier"
            )
        );
        let d = database_name("projects/p/instances/i/databases/d")
            .ok()
            .map(|d| (d.instance, d.id));
        assert_eq!(d, Some(("projects/p/instances/i", "d")));
    }
}
