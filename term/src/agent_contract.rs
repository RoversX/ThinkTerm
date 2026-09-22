//! Budgets for the agent user variable, shared by local and mirrored panes.
//! Check before retaining a value; a rejected update clears the old value.
pub const AGENT_USER_VAR: &str = "THINKTERM_AGENT";
pub const MAX_AGENT_CONTRACT_BYTES: usize = 4096;
pub const MAX_AGENT_ID_BYTES: usize = 128;
pub const MAX_AGENT_SESSION_BYTES: usize = 512;

pub fn agent_contract_within_budget(value: &str) -> bool {
    value.len() <= MAX_AGENT_CONTRACT_BYTES
        && value.split(';').all(|field| match field.split_once('=') {
            Some((key, val)) => match key.trim() {
                "agent" => val.trim().len() <= MAX_AGENT_ID_BYTES,
                "session" => val.trim().len() <= MAX_AGENT_SESSION_BYTES,
                _ => true,
            },
            None => true,
        })
}

/// Clear an oversized contract without retaining its allocation. Other user
/// variables are outside this contract and keep their existing behavior.
pub fn sanitize_agent_user_var(name: &str, value: &mut String) {
    if name == AGENT_USER_VAR && !agent_contract_within_budget(value) {
        *value = String::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn limits_cover_duplicates_unknown_fields_and_multibyte_ids() {
        assert!(agent_contract_within_budget(
            "v1;agent=claude;state=idle;session=abc"
        ));
        assert!(!agent_contract_within_budget(&format!(
            "v1;agent={};agent=ok",
            "x".repeat(129)
        )));
        assert!(!agent_contract_within_budget(&format!(
            "v1;session={}",
            "界".repeat(171)
        )));
        assert!(!agent_contract_within_budget(&format!(
            "v1;ignored={}",
            "x".repeat(4096)
        )));
        assert!(agent_contract_within_budget(""));
    }
}
