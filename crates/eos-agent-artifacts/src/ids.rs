//! Artifact ids. One artifact has two URL forms naming the same uuid (its
//! "slug"): `claude.ai/code/artifact/<uuid>` and `claude.ai/artifact/<short id>`,
//! where the short id is the uuid's 128 bits in base58 (Bitcoin alphabet),
//! always 22 characters. Artifact types are artifacts too, so a `type_url` is
//! parsed the same way.

const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const SHORT_LEN: usize = 22;

/// True for a lowercase or uppercase hyphenated uuid.
pub(crate) fn is_slug(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

pub fn short_id(slug: &str) -> Option<String> {
    if !is_slug(slug) {
        return None;
    }
    let hex: String = slug.chars().filter(|c| *c != '-').collect();
    let mut n = u128::from_str_radix(&hex, 16).ok()?;
    let mut out = [0u8; SHORT_LEN];
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[(n % 58) as usize];
        n /= 58;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

pub(crate) fn slug_of_short_id(short: &str) -> Option<String> {
    if short.len() != SHORT_LEN {
        return None;
    }
    let mut n: u128 = 0;
    for c in short.bytes() {
        let digit = ALPHABET.iter().position(|a| *a == c)? as u128;
        n = n.checked_mul(58)?.checked_add(digit)?;
    }
    let hex = format!("{n:032x}");
    Some(format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))
}

/// The slug named by either URL form, or `None` for any other URL.
pub fn slug_from_url(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    if !matches!(host, "claude.ai" | "claude.com" | "preview.claude.ai" | "preview.claude.com") {
        return None;
    }
    let (long_form, id) = match path.strip_prefix("code/artifact/") {
        Some(id) => (true, id),
        None => (false, path.strip_prefix("artifact/")?),
    };
    let id = id.split(['/', '?', '#']).next().unwrap_or_default();
    if long_form { is_slug(id).then(|| id.to_ascii_lowercase()) } else { slug_of_short_id(id) }
}

/// The share link claude.ai serves for an artifact.
pub fn viewer_url(slug: &str) -> String {
    match short_id(slug) {
        Some(short) => format!("https://claude.ai/artifact/{short}"),
        None => format!("https://claude.ai/code/artifact/{slug}"),
    }
}

/// An uploaded asset's id: 32 lowercase hex characters.
pub(crate) fn is_asset_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ids_round_trip() {
        let slug = "0f8e2a9c-1b3d-4e5f-8a7b-6c5d4e3f2a1b";
        let short = short_id(slug).expect("short id");
        assert_eq!(short.len(), 22);
        assert_eq!(slug_of_short_id(&short).as_deref(), Some(slug));
        assert_eq!(slug_from_url(&viewer_url(slug)).as_deref(), Some(slug));
        let long = format!("https://claude.ai/code/artifact/{}?x=1", slug.to_uppercase());
        assert_eq!(slug_from_url(&long).as_deref(), Some(slug));
    }

    #[test]
    fn other_urls_are_not_artifacts() {
        assert!(slug_from_url("https://example.com/artifact/abc").is_none());
        assert!(slug_from_url("https://claude.ai/chat/0f8e2a9c-1b3d-4e5f-8a7b-6c5d4e3f2a1b").is_none());
        assert!(slug_from_url("http://claude.ai/artifact/x").is_none());
        assert_eq!(short_id("00000000-0000-0000-0000-000000000000").as_deref(), Some("1111111111111111111111"));
    }
}
