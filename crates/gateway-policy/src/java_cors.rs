//! Java Lambda origin matching, separate from Gateway's CORS policy semantics.
//! No transport/provider state, same-host fallback or URL normalization.
pub fn sanitize_default_port(origin: &str) -> Result<&str, InvalidOrigin> {
    let Some(scheme_end) = origin.find("://") else {
        return Ok(origin);
    };
    let scheme = &origin[..scheme_end];
    let mut from = scheme_end + 3;
    if let Some(bracket) = origin[from..].find('[') {
        from = origin[from + bracket..]
            .find(']')
            .map(|end| from + bracket + end)
            .ok_or(InvalidOrigin)?;
    }
    if let Some(colon) = origin[from..].find(':') {
        let colon = from + colon;
        let port = origin[colon + 1..]
            .parse::<i32>()
            .map_err(|_| InvalidOrigin)?;
        if (scheme == "http" && port == 80) || (scheme == "https" && port == 443) {
            return Ok(&origin[..colon]);
        }
    }
    Ok(origin)
}
#[derive(Debug)]
pub struct InvalidOrigin;
pub fn origin_allowed(origin: &str, allowed: &[String]) -> Result<bool, InvalidOrigin> {
    // Java never sanitizes the configured values, only the incoming Origin.
    if allowed.is_empty() {
        return Ok(false);
    }
    let sanitized = sanitize_default_port(origin)?;
    Ok(allowed
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(sanitized)))
}
