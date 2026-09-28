use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// How sensitive the material in this manifest is.
///
/// Ordered: `public` < `internal` < `regulated` < `restricted`. Opt-in — a
/// manifest with no `data_class` is unclassified and no egress check applies
/// (F3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataClass {
    Public,
    Internal,
    Regulated,
    Restricted,
}

impl DataClass {
    fn rank(self) -> u8 {
        match self {
            DataClass::Public => 0,
            DataClass::Internal => 1,
            DataClass::Regulated => 2,
            DataClass::Restricted => 3,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DataClass::Public => "public",
            DataClass::Internal => "internal",
            DataClass::Regulated => "regulated",
            DataClass::Restricted => "restricted",
        }
    }
}

/// The category of an outbound call, so a policy can allow a grader without
/// implicitly allowing the optimizer too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressKind {
    /// Invoked as a `type: "llm"` evaluator.
    Grader,
    /// Invoked as the Meta-Optimizer to write a rule.
    Optimizer,
}

impl EgressKind {
    pub fn label(self) -> &'static str {
        match self {
            EgressKind::Grader => "llm evaluator (grader)",
            EgressKind::Optimizer => "meta-optimizer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EgressProvider {
    /// Local llama.cpp. Never leaves the machine.
    Embedded,
    /// Any remote endpoint: `custom`, and any provider that sends data off-box.
    Hosted,
}

impl EgressProvider {
    pub fn label(self) -> &'static str {
        match self {
            EgressProvider::Embedded => "embedded",
            EgressProvider::Hosted => "hosted",
        }
    }
}

/// One permitted path: this provider, carrying at most this class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressAllow {
    pub provider: EgressProvider,
    pub max_class: DataClass,
}

/// Egress policy. Enforcement activates only when the manifest declares
/// `data_class`; absent that, behavior is unchanged and no manifest breaks.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EgressPolicy {
    #[serde(default)]
    pub allow: Vec<EgressAllow>,
}

impl EgressPolicy {
    /// The ceiling for a provider, or `None` if it is not listed. Unlisted
    /// providers are denied while enforcement is active.
    pub fn ceiling(&self, provider: EgressProvider) -> Option<DataClass> {
        self.allow
            .iter()
            .filter(|a| a.provider == provider)
            .map(|a| a.max_class)
            .max_by_key(|c| c.rank())
    }
}

/// The classification and policy a manifest declared, if any.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DataHandling {
    pub data_class: Option<DataClass>,
    #[serde(default)]
    pub egress: Option<EgressPolicy>,
}

/// Classify a `meta_llm` provider string into an egress category.
pub fn provider_kind(provider: &str) -> EgressProvider {
    match provider {
        "embedded" => EgressProvider::Embedded,
        // Everything else reaches the network: `custom`, and any future
        // provider that is not explicitly local.
        _ => EgressProvider::Hosted,
    }
}

/// The result of checking one outbound call.
#[derive(Debug, Clone, PartialEq)]
pub enum EgressDecision {
    Allowed {
        provider: EgressProvider,
        kind: EgressKind,
    },
    Denied {
        provider: EgressProvider,
        kind: EgressKind,
        data_class: DataClass,
        reason: String,
    },
}

impl EgressDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, EgressDecision::Allowed { .. })
    }
}

/// Check one outbound call against the manifest's data handling policy.
///
/// Returns `Allowed` unconditionally when the manifest declares no
/// `data_class`, which is what keeps this backward compatible.
pub fn check_egress(handling: &DataHandling, provider: &str, kind: EgressKind) -> EgressDecision {
    let (Some(data_class), Some(policy)) = (handling.data_class, handling.egress.as_ref()) else {
        return EgressDecision::Allowed {
            provider: provider_kind(provider),
            kind,
        };
    };

    let provider = provider_kind(provider);

    // Local inference never leaves the machine, so no policy is needed for it.
    // Requiring an allow entry would defeat the point of declaring a
    // restrictive class while using an embedded model.
    if provider == EgressProvider::Embedded {
        return EgressDecision::Allowed { provider, kind };
    }

    match policy.ceiling(provider) {
        Some(ceiling) if data_class.rank() <= ceiling.rank() => {
            EgressDecision::Allowed { provider, kind }
        }
        Some(ceiling) => EgressDecision::Denied {
            provider,
            kind,
            data_class,
            reason: format!(
                "manifest data_class is `{}` but the {} provider is allowed only up to `{}`",
                data_class.label(),
                provider.label(),
                ceiling.label()
            ),
        },
        None => EgressDecision::Denied {
            provider,
            kind,
            data_class,
            reason: format!(
                "the {} provider is not listed in egress.allow; unlisted providers are denied \
                 while a data_class is declared",
                provider.label()
            ),
        },
    }
}

/// Which providers a manifest's calls would use, without executing anything.
#[derive(Debug, Clone, PartialEq)]
pub struct EgressPlanEntry {
    pub kind: EgressKind,
    pub provider: EgressProvider,
    pub allowed: bool,
    pub reason: String,
}

/// The full plan for a manifest: one entry per outbound path.
pub fn build_plan(
    handling: &DataHandling,
    meta_llm_provider: &str,
    has_llm_evaluators: bool,
) -> Vec<EgressPlanEntry> {
    let mut plan = Vec::new();
    for kind in [EgressKind::Optimizer, EgressKind::Grader] {
        if kind == EgressKind::Grader && !has_llm_evaluators {
            continue;
        }
        let decision = check_egress(handling, meta_llm_provider, kind);
        plan.push(match decision {
            EgressDecision::Allowed { provider, .. } => EgressPlanEntry {
                kind,
                provider,
                allowed: true,
                reason: if provider == EgressProvider::Embedded {
                    "local inference, nothing leaves the machine".to_string()
                } else {
                    "within its declared ceiling".to_string()
                },
            },
            EgressDecision::Denied {
                provider,
                reason,
                data_class,
                ..
            } => EgressPlanEntry {
                kind,
                provider,
                allowed: false,
                reason: format!(
                    "{} data to a {} provider: {}",
                    data_class.label(),
                    provider.label(),
                    reason
                ),
            },
        });
    }
    plan
}

/// Fail loud on a denied call, naming the provider, the class, and the remedy.
pub fn enforce_egress(handling: &DataHandling, provider: &str, kind: EgressKind) -> Result<()> {
    match check_egress(handling, provider, kind) {
        EgressDecision::Allowed { .. } => Ok(()),
        EgressDecision::Denied {
            provider,
            kind,
            data_class,
            reason,
        } => bail!(
            "Egress policy violation: the {} would send `{}` data to a {} provider.\n\
             {}\n\n\
             Remedy: allow it in the manifest, e.g.\n  \
             \"egress\": {{ \"allow\": [{{ \"provider\": \"{}\", \"max_class\": \"{}\" }}] }}\n\
             Or remove `data_class` from the manifest to disable egress enforcement.",
            kind.label(),
            data_class.label(),
            provider.label(),
            reason,
            provider.label(),
            data_class.label()
        ),
    }
}

/// Human-readable plan of which providers a run would use, before any container
/// starts.
pub fn render_egress_plan(
    handling: &DataHandling,
    meta_llm_provider: &str,
    has_llm_evaluators: bool,
) -> String {
    let mut out = String::from("Egress plan\n===========\n");
    out.push_str(&format!(
        "Data class: {}\n",
        handling
            .data_class
            .map(|c| c.label())
            .unwrap_or("unclassified (no enforcement applies)")
    ));
    out.push_str(&format!("meta_llm provider: {}\n\n", meta_llm_provider));

    for entry in build_plan(handling, meta_llm_provider, has_llm_evaluators) {
        out.push_str(&format!(
            "  {:<17} {} → {}{}\n",
            entry.kind.label(),
            entry.provider.label(),
            if entry.allowed { "ALLOWED" } else { "DENIED " },
            format!(" ({})", entry.reason)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handling(class: DataClass, allow: &[(&str, DataClass)]) -> DataHandling {
        DataHandling {
            data_class: Some(class),
            egress: Some(EgressPolicy {
                allow: allow
                    .iter()
                    .map(|(p, c)| EgressAllow {
                        provider: match *p {
                            "embedded" => EgressProvider::Embedded,
                            _ => EgressProvider::Hosted,
                        },
                        max_class: *c,
                    })
                    .collect(),
            }),
        }
    }

    // --- Backward compatibility ---

    /// The property that makes this safe to ship: an unclassified manifest
    /// behaves exactly as before.
    #[test]
    fn unclassified_manifests_are_unrestricted() {
        for provider in ["embedded", "custom", "anything-else"] {
            let decision = check_egress(&DataHandling::default(), provider, EgressKind::Grader);
            assert!(decision.is_allowed(), "{} should be allowed", provider);
        }
    }

    #[test]
    fn data_class_without_egress_block_still_allows() {
        // Declaring a class with no policy object is treated as unclassified,
        // so a half-written manifest cannot brick a run.
        let handling = DataHandling {
            data_class: Some(DataClass::Regulated),
            egress: None,
        };
        assert!(check_egress(&handling, "custom", EgressKind::Grader).is_allowed());
    }

    // --- Enforcement ---

    #[test]
    fn embedded_is_allowed_even_with_an_empty_policy() {
        // The headline use case: regulated data, local model, no allow list.
        let h = handling(DataClass::Restricted, &[]);
        assert!(check_egress(&h, "embedded", EgressKind::Grader).is_allowed());
        assert!(check_egress(&h, "embedded", EgressKind::Optimizer).is_allowed());
    }

    #[test]
    fn hosted_provider_is_denied_when_unlisted() {
        let h = handling(DataClass::Internal, &[("embedded", DataClass::Regulated)]);
        let decision = check_egress(&h, "custom", EgressKind::Grader);
        assert!(!decision.is_allowed(), "{:?}", decision);
        match decision {
            EgressDecision::Denied { reason, .. } => {
                assert!(reason.contains("not listed"), "{}", reason);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn embedded_is_always_local_and_need_not_be_listed_when_unclassified() {
        let h = DataHandling::default();
        assert!(check_egress(&h, "embedded", EgressKind::Optimizer).is_allowed());
    }

    #[test]
    fn class_above_ceiling_is_denied() {
        // Hosted allowed up to internal, but the manifest is regulated.
        let h = handling(DataClass::Regulated, &[("hosted", DataClass::Internal)]);
        let decision = check_egress(&h, "custom", EgressKind::Optimizer);
        assert!(!decision.is_allowed());
        match decision {
            EgressDecision::Denied { reason, .. } => {
                assert!(
                    reason.contains("allowed only up to `internal`"),
                    "{}",
                    reason
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn class_at_or_below_ceiling_is_allowed() {
        let h = handling(DataClass::Internal, &[("hosted", DataClass::Regulated)]);
        assert!(check_egress(&h, "custom", EgressKind::Grader).is_allowed());
    }

    #[test]
    fn embedded_may_be_allowed_regulated_data() {
        let h = handling(DataClass::Regulated, &[("embedded", DataClass::Restricted)]);
        assert!(check_egress(&h, "embedded", EgressKind::Grader).is_allowed());
    }

    #[test]
    fn the_most_permissive_entry_wins_when_a_provider_is_listed_twice() {
        let h = handling(
            DataClass::Regulated,
            &[
                ("hosted", DataClass::Internal),
                ("hosted", DataClass::Restricted),
            ],
        );
        assert!(check_egress(&h, "custom", EgressKind::Grader).is_allowed());
    }

    /// A denial must name the provider, the class, and the remedy.
    #[test]
    fn denial_message_is_actionable() {
        let h = handling(
            DataClass::Restricted,
            &[("embedded", DataClass::Restricted)],
        );
        let err = enforce_egress(&h, "custom", EgressKind::Grader)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("meta-optimizer") || err.contains("llm evaluator"),
            "{}",
            err
        );
        assert!(err.contains("restricted"), "{}", err);
        assert!(err.contains("hosted"), "{}", err);
        assert!(
            err.contains("egress.allow"),
            "should name the remedy: {}",
            err
        );
    }

    #[test]
    fn optimizer_and_grader_are_reported_separately() {
        let h = handling(DataClass::Internal, &[("hosted", DataClass::Internal)]);
        assert!(check_egress(&h, "custom", EgressKind::Grader).is_allowed());
        assert!(check_egress(&h, "custom", EgressKind::Optimizer).is_allowed());

        let plan = render_egress_plan(&h, "custom", true);
        assert!(plan.contains("meta-optimizer"), "{}", plan);
        assert!(plan.contains("llm evaluator"), "{}", plan);
    }

    #[test]
    fn plan_shows_denial_before_a_run_starts() {
        let h = handling(DataClass::Regulated, &[("embedded", DataClass::Regulated)]);
        let plan = render_egress_plan(&h, "custom", true);
        assert!(plan.contains("DENIED"), "{}", plan);
        assert!(plan.contains("regulated"), "{}", plan);
    }

    #[test]
    fn unclassified_plan_says_so() {
        let plan = render_egress_plan(&DataHandling::default(), "embedded", false);
        assert!(plan.contains("unclassified"), "{}", plan);
        assert!(plan.contains("ALLOWED"), "{}", plan);
    }

    #[test]
    fn no_llm_evaluators_means_no_grader_entry() {
        let h = handling(DataClass::Internal, &[("hosted", DataClass::Internal)]);
        let plan = build_plan(&h, "custom", false);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].kind, EgressKind::Optimizer);
    }

    #[test]
    fn embedded_needs_no_allow_entry_when_data_class_is_declared() {
        // An embedded-only manifest is the whole point: regulated data can stay
        // local, so a restrictive data class must not block it.
        let h = handling(DataClass::Restricted, &[]);
        let plan = build_plan(&h, "embedded", true);
        assert_eq!(plan.len(), 2);
        assert!(
            plan.iter().all(|e| e.allowed),
            "an embedded provider sends nothing off-box: {:?}",
            plan
        );
    }
}
