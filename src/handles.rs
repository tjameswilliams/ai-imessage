//! Normalization of Messages handles (phone numbers and email addresses).

/// Normalize a raw handle for comparison and grouping.
///
/// - Email addresses are trimmed and lowercased.
/// - Phone numbers keep a leading `+` and lose all formatting characters.
/// - Short codes and anything without digits pass through trimmed.
///
/// No country-code inference is performed: assuming a default region would
/// silently corrupt non-US numbers, so `5550100` and `+15550100` remain
/// distinct until a smarter comparison exists.
pub fn normalize_handle(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.contains('@') {
        return trimmed.to_ascii_lowercase();
    }
    let digits: String = trimmed.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return trimmed.to_string();
    }
    if trimmed.starts_with('+') {
        format!("+{digits}")
    } else {
        digits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatted_us_number_is_flattened() {
        assert_eq!(normalize_handle("+1 (555) 010-4477"), "+15550104477");
    }

    #[test]
    fn dots_and_spaces_are_removed() {
        assert_eq!(normalize_handle("555.010.4477"), "5550104477");
        assert_eq!(normalize_handle("555 010 4477"), "5550104477");
    }

    #[test]
    fn international_number_keeps_plus() {
        assert_eq!(normalize_handle("+49 170 1234567"), "+491701234567");
    }

    #[test]
    fn email_is_lowercased_and_trimmed() {
        assert_eq!(
            normalize_handle("  Tim.J@Example.COM "),
            "tim.j@example.com"
        );
    }

    #[test]
    fn email_case_of_local_part_is_lowercased_too() {
        // Messages treats handles case-insensitively; match that.
        assert_eq!(normalize_handle("USER@HOST.COM"), "user@host.com");
    }

    #[test]
    fn short_codes_pass_through() {
        assert_eq!(normalize_handle("22395"), "22395");
    }

    #[test]
    fn no_digits_passes_through_trimmed() {
        assert_eq!(normalize_handle("  unknown  "), "unknown");
    }

    #[test]
    fn empty_string_stays_empty() {
        assert_eq!(normalize_handle(""), "");
    }

    #[test]
    fn plus_without_digits_passes_through() {
        assert_eq!(normalize_handle("+"), "+");
    }
}
