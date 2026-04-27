//! Scope definitions and scope-checking utilities.

/// All defined scopes in the gateway.
pub mod scope {
    pub const READ_EVENTS: &str = "read:events";
    pub const READ_RISK: &str = "read:risk";
    pub const WRITE_ANALYZE: &str = "write:analyze";
    pub const ADMIN: &str = "admin";
}

/// Check that a JWT claims set contains the required scope.
///
/// Returns `Ok(())` if the scope is present, otherwise `Err(GatewayError::Unauthorized)`.
pub fn require_scope(
    scopes: &[String],
    required: &str,
) -> Result<(), crate::error::GatewayError> {
    if scopes.iter().any(|s| s == required) {
        Ok(())
    } else {
        Err(crate::error::GatewayError::Unauthorized(required.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_present_ok() {
        let scopes = vec!["read:events".into(), "write:analyze".into()];
        assert!(require_scope(&scopes, "write:analyze").is_ok());
    }

    #[test]
    fn scope_absent_err() {
        let scopes = vec!["read:events".into()];
        let result = require_scope(&scopes, "admin");
        assert!(matches!(result, Err(crate::error::GatewayError::Unauthorized(s)) if s == "admin"));
    }
}
