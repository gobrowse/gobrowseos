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

    #[test]
    fn rule_decision_above_fires_when_risk_exceeds_max() {
        // Write rule with Execute risk → Deny (decision_above arm)
        let engine = PolicyEngine::new(vec![PolicyRule {
            tool_pattern: "write_tool".into(),
            maximum_auto_risk: RiskClass::Write,
            decision_above: PolicyDecision::Deny,
        }]);
        // Execute is higher ordinal than Write, so risk > maximum_auto_risk
        assert_eq!(
            engine.evaluate("write_tool", RiskClass::Execute),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn exact_pattern_match_takes_precedence_over_default() {
        // Two rules: exact match first, star-default second.
        // The exact pattern must be found first by find().
        let engine = PolicyEngine::new(vec![
            PolicyRule {
                tool_pattern: "specific_tool".into(),
                maximum_auto_risk: RiskClass::Read,
                decision_above: PolicyDecision::Deny,
            },
            PolicyRule {
                tool_pattern: "*".into(),
                maximum_auto_risk: RiskClass::Write,
                decision_above: PolicyDecision::Deny,
            },
        ]);
        // "specific_tool" with Write risk → matches the exact rule first
        // (Read max, Write above → Deny)
        assert_eq!(
            engine.evaluate("specific_tool", RiskClass::Write),
            PolicyDecision::Deny
        );
        // A different tool falls through to the "*" default
        assert_eq!(
            engine.evaluate("other_tool", RiskClass::Write),
            PolicyDecision::Allow
        );
    }
}
