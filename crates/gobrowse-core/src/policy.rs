use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RiskClass {
    Read,
    Write,
    Execute,
    ExternalSideEffect,
    Destructive,
    Admin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PolicyDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRule {
    pub tool_pattern: String,
    pub maximum_auto_risk: RiskClass,
    pub decision_above: PolicyDecision,
}

#[derive(Debug, Clone, Default)]
pub struct PolicyEngine {
    rules: Vec<PolicyRule>,
}

impl PolicyEngine {
    pub fn new(rules: Vec<PolicyRule>) -> Self {
        Self { rules }
    }

    pub fn evaluate(&self, tool_id: &str, risk: RiskClass) -> PolicyDecision {
        if let Some(rule) = self
            .rules
            .iter()
            .find(|rule| pattern_matches(&rule.tool_pattern, tool_id))
        {
            return if risk <= rule.maximum_auto_risk {
                PolicyDecision::Allow
            } else {
                rule.decision_above
            };
        }
        match risk {
            RiskClass::Read => PolicyDecision::Allow,
            RiskClass::Write | RiskClass::Execute => PolicyDecision::Ask,
            RiskClass::ExternalSideEffect | RiskClass::Destructive | RiskClass::Admin => {
                PolicyDecision::Ask
            }
        }
    }
}

fn pattern_matches(pattern: &str, value: &str) -> bool {
    pattern == "*"
        || pattern == value
        || pattern
            .strip_suffix('*')
            .is_some_and(|prefix| value.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_cannot_bypass_default_policy() {
        let engine = PolicyEngine::default();
        assert_eq!(
            engine.evaluate("filesystem.read", RiskClass::Read),
            PolicyDecision::Allow
        );
        assert_eq!(
            engine.evaluate("terminal.exec", RiskClass::Execute),
            PolicyDecision::Ask
        );
        assert_eq!(
            engine.evaluate("workspace.delete", RiskClass::Destructive),
            PolicyDecision::Ask
        );
    }

    #[test]
    fn wildcard_rules_are_prefix_bounded() {
        let engine = PolicyEngine::new(vec![PolicyRule {
            tool_pattern: "library.*".into(),
            maximum_auto_risk: RiskClass::Write,
            decision_above: PolicyDecision::Deny,
        }]);
        assert_eq!(
            engine.evaluate("library.create", RiskClass::Write),
            PolicyDecision::Allow
        );
        assert_eq!(
            engine.evaluate("terminal.exec", RiskClass::Execute),
            PolicyDecision::Ask
        );
    }
}
