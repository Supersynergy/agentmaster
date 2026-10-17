use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

const MAX_BYTES: u64 = 1024 * 1024;

fn accepts(bytes: &[u8], expected: bool) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    value.get("ok").and_then(serde_json::Value::as_bool) == Some(true)
        && value.get("verdict").and_then(serde_json::Value::as_bool) == Some(expected)
}

pub fn check(path: &Path, expected: bool) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "browser verdict exceeds 1 MiB"
    );
    anyhow::ensure!(
        accepts(&bytes, expected),
        "invalid browser verdict, transport failure or unexpected assertion"
    );
    println!(
        "{}",
        serde_json::json!({
            "accepted": true,
            "expected_verdict": expected,
            "file": std::path::absolute(path)?,
            "sha256": format!("{:x}", Sha256::digest(&bytes)),
        })
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_requires_transport_and_boolean_expectation() {
        assert!(accepts(br#"{"ok":true,"verdict":true}"#, true));
        assert!(accepts(br#"{"ok":true,"verdict":false}"#, false));
        for input in [
            r#"{"ok":true,"verdict":false}"#,
            r#"{"ok":false,"verdict":true}"#,
            r#"{"verdict":true}"#,
            r#"{"ok":true,"verdict":"true"}"#,
            r#"{"ok":true}"#,
            r#"[{"ok":true,"verdict":true}]"#,
            r#"{"ok":true,"verdict":true} trailing"#,
            "",
        ] {
            assert!(!accepts(input.as_bytes(), true), "accepted {input}");
        }
    }
}
