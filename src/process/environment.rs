use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

use super::ProcessError;

const MAGIC: &[u8] = b"PCENV1\0";
pub(crate) const MAX_HANDOFF_BYTES: usize = 1024 * 1024;
pub(crate) const HANDOFF_SOCKET_ENV: &str = "SENATE_ENVIRONMENT_SOCKET";

pub(crate) fn safe_environment_name(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            "HOME"
                | "PATH"
                | "LANG"
                | "LC_ALL"
                | "LC_CTYPE"
                | "TMPDIR"
                | "USER"
                | "LOGNAME"
                | "SHELL"
                | "TERM"
                | "COLORTERM"
                | "NO_COLOR"
                | "SSH_AUTH_SOCK"
                | "XDG_CONFIG_HOME"
                | "XDG_DATA_HOME"
                | "XDG_CACHE_HOME"
                | "CLAUDE_CONFIG_DIR"
                | "GIT_CONFIG_GLOBAL"
                | "GIT_CONFIG_SYSTEM"
        )
    )
}

#[cfg(unix)]
pub(crate) fn encode_forwarded_environment() -> Result<Option<Vec<u8>>, ProcessError> {
    use std::os::unix::ffi::OsStrExt as _;

    let values = std::env::vars_os()
        .filter(|(key, _)| !safe_environment_name(key) && !internal_environment_name(key))
        .collect::<Vec<_>>();
    if values.is_empty() {
        return Ok(None);
    }
    let mut encoded = Vec::with_capacity(4096);
    encoded.extend_from_slice(MAGIC);
    push_u32(&mut encoded, values.len())?;
    for (key, value) in values {
        push_bytes(&mut encoded, key.as_bytes())?;
        push_bytes(&mut encoded, value.as_bytes())?;
        if encoded.len() > MAX_HANDOFF_BYTES {
            return Err(ProcessError::InvalidSpec(
                "forwarded environment exceeds safe handoff limit",
            ));
        }
    }
    Ok(Some(encoded))
}

#[cfg(not(unix))]
pub(crate) fn encode_forwarded_environment() -> Result<Option<Vec<u8>>, ProcessError> {
    Err(ProcessError::UnsupportedPlatform)
}

pub(crate) fn internal_environment_name(name: &OsStr) -> bool {
    crate::exec::jira_environment_name(name)
        || matches!(
            name.to_str(),
            Some(
                "SENATE_MANAGED_PROCESS_ID"
                    | "SENATE_COMMAND_FINGERPRINT"
                    | "SENATE_ENVIRONMENT_SOCKET"
            )
        )
}

#[cfg(unix)]
pub(crate) fn decode_forwarded_environment(
    encoded: &[u8],
) -> Result<BTreeMap<OsString, OsString>, ProcessError> {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    if encoded.len() > MAX_HANDOFF_BYTES || !encoded.starts_with(MAGIC) {
        return Err(ProcessError::InvalidSpec(
            "invalid forwarded environment handoff",
        ));
    }
    let mut cursor = MAGIC.len();
    let count = usize::try_from(read_u32(encoded, &mut cursor)?)
        .map_err(|_| ProcessError::InvalidSpec("environment count overflow"))?;
    if count > 4096 {
        return Err(ProcessError::InvalidSpec(
            "forwarded environment has too many entries",
        ));
    }
    let mut environment = BTreeMap::new();
    for _ in 0..count {
        let key = read_bytes(encoded, &mut cursor)?;
        let value = read_bytes(encoded, &mut cursor)?;
        if key.is_empty() || key.contains(&b'=') || key.contains(&0) || value.contains(&0) {
            return Err(ProcessError::InvalidSpec(
                "invalid forwarded environment entry",
            ));
        }
        let key_os = OsStr::from_bytes(key);
        if safe_environment_name(key_os) || internal_environment_name(key_os) {
            return Err(ProcessError::InvalidSpec(
                "forwarded environment contains reserved entry",
            ));
        }
        if environment
            .insert(
                OsString::from_vec(key.to_vec()),
                OsString::from_vec(value.to_vec()),
            )
            .is_some()
        {
            return Err(ProcessError::InvalidSpec(
                "forwarded environment contains duplicate entry",
            ));
        }
    }
    if cursor != encoded.len() {
        return Err(ProcessError::InvalidSpec(
            "forwarded environment has trailing bytes",
        ));
    }
    Ok(environment)
}

#[cfg(not(unix))]
pub(crate) fn decode_forwarded_environment(
    _encoded: &[u8],
) -> Result<BTreeMap<OsString, OsString>, ProcessError> {
    Err(ProcessError::UnsupportedPlatform)
}

fn push_u32(output: &mut Vec<u8>, value: usize) -> Result<(), ProcessError> {
    let value = u32::try_from(value)
        .map_err(|_| ProcessError::InvalidSpec("environment handoff length overflow"))?;
    output.extend_from_slice(&value.to_be_bytes());
    Ok(())
}

fn push_bytes(output: &mut Vec<u8>, value: &[u8]) -> Result<(), ProcessError> {
    push_u32(output, value.len())?;
    output.extend_from_slice(value);
    Ok(())
}

fn read_u32(encoded: &[u8], cursor: &mut usize) -> Result<u32, ProcessError> {
    let end = cursor
        .checked_add(4)
        .ok_or(ProcessError::InvalidSpec("environment handoff overflow"))?;
    let bytes = encoded
        .get(*cursor..end)
        .ok_or(ProcessError::InvalidSpec("truncated environment handoff"))?;
    *cursor = end;
    Ok(u32::from_be_bytes(
        bytes
            .try_into()
            .expect("validated environment length is four bytes"),
    ))
}

fn read_bytes<'a>(encoded: &'a [u8], cursor: &mut usize) -> Result<&'a [u8], ProcessError> {
    let length = usize::try_from(read_u32(encoded, cursor)?)
        .map_err(|_| ProcessError::InvalidSpec("environment handoff length overflow"))?;
    let end = cursor
        .checked_add(length)
        .ok_or(ProcessError::InvalidSpec("environment handoff overflow"))?;
    let bytes = encoded
        .get(*cursor..end)
        .ok_or(ProcessError::InvalidSpec("truncated environment handoff"))?;
    *cursor = end;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jira_configuration_never_crosses_environment_handoff() {
        assert!(internal_environment_name(OsStr::new("SENATE_JIRA_TOKEN")));
        assert!(internal_environment_name(OsStr::new("SENATE_JIRA_URL")));
        let mut encoded = Vec::new();
        encoded.extend_from_slice(MAGIC);
        push_u32(&mut encoded, 1).unwrap();
        push_bytes(&mut encoded, b"SENATE_JIRA_TOKEN").unwrap();
        push_bytes(&mut encoded, b"secret").unwrap();
        assert!(decode_forwarded_environment(&encoded).is_err());
    }

    #[test]
    fn codec_round_trips_non_secret_safe_shape() {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(MAGIC);
        push_u32(&mut encoded, 1).unwrap();
        push_bytes(&mut encoded, b"ANTHROPIC_API_KEY").unwrap();
        push_bytes(&mut encoded, b"not-logged").unwrap();
        let decoded = decode_forwarded_environment(&encoded).unwrap();
        assert_eq!(
            decoded.get(OsStr::new("ANTHROPIC_API_KEY")),
            Some(&OsString::from("not-logged"))
        );
    }

    #[test]
    fn malformed_handoff_fails_closed() {
        assert!(decode_forwarded_environment(b"bad").is_err());
    }
}
