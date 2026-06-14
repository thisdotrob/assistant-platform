//! Access control for command dispatch.
//!
//! Operators on the host are trusted; agents reaching in through the container
//! bridge are not. Rather than link the permissions/approvals crates, this
//! crate defines an [`AccessPolicy`] trait the host implements — folding in
//! roles, group scope, and approval state — and a [`dispatch`] entry point that
//! resolves a command, authorizes it, and runs it.

use serde::{Deserialize, Serialize};

use crate::command::{ActionKind, Caller, CommandOutcome, CommandRequest};
use crate::registry::CommandRegistry;

/// What an access policy decides for a command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum AccessDecision {
    /// The caller may run the command now.
    Allow,
    /// The caller may not run the command; `reason` is shown to them.
    Deny { reason: String },
    /// The command is gated on an approval the policy has recorded; the caller
    /// must get `approval_id` granted before it will run.
    NeedsApproval { approval_id: String },
}

/// The context a policy decides over: who is calling, the resolved action and
/// whether it reads or writes, and the full request for any finer-grained
/// argument inspection.
pub struct AccessRequest<'a> {
    pub caller: &'a Caller,
    pub resource: &'a str,
    pub action: &'a str,
    pub kind: ActionKind,
    pub request: &'a CommandRequest,
}

/// Decides whether a caller may run a command. The host implements this,
/// consulting permissions, group membership, and recorded approvals; this crate
/// never sees those details.
pub trait AccessPolicy {
    fn authorize(&self, request: &AccessRequest<'_>) -> AccessDecision;
}

/// A trust-the-operator policy: operators may run anything, agents nothing.
/// The default for the host's own operator command surface.
pub struct OperatorOnly;

impl AccessPolicy for OperatorOnly {
    fn authorize(&self, request: &AccessRequest<'_>) -> AccessDecision {
        match request.caller {
            Caller::Operator => AccessDecision::Allow,
            Caller::Agent { .. } => AccessDecision::Deny {
                reason: "agent callers are not permitted on this surface".to_string(),
            },
        }
    }
}

/// Resolve a command against the registry, authorize it via the policy, and run
/// it. An unknown resource or action is reported without consulting the policy;
/// anything that resolves is gated by [`AccessPolicy::authorize`].
pub fn dispatch(
    registry: &CommandRegistry,
    policy: &dyn AccessPolicy,
    caller: &Caller,
    request: &CommandRequest,
) -> CommandOutcome {
    let Some(resource) = registry.get(&request.resource) else {
        return CommandOutcome::error(format!("unknown resource {:?}", request.resource));
    };
    let Some(kind) = resource
        .actions()
        .into_iter()
        .find(|a| a.name == request.action)
        .map(|a| a.kind)
    else {
        return CommandOutcome::error(format!(
            "resource {:?} has no action {:?}",
            request.resource, request.action
        ));
    };

    let access = AccessRequest {
        caller,
        resource: &request.resource,
        action: &request.action,
        kind,
        request,
    };
    match policy.authorize(&access) {
        AccessDecision::Allow => resource.execute(request),
        AccessDecision::Deny { reason } => CommandOutcome::error(format!("access denied: {reason}")),
        AccessDecision::NeedsApproval { approval_id } => {
            CommandOutcome::error(format!("approval required: {approval_id}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::ResultTable;
    use crate::registry::{ActionSpec, Resource};

    struct UsersResource;

    impl Resource for UsersResource {
        fn name(&self) -> &str {
            "users"
        }
        fn actions(&self) -> Vec<ActionSpec> {
            vec![ActionSpec::read("list"), ActionSpec::write("create")]
        }
        fn execute(&self, request: &CommandRequest) -> CommandOutcome {
            let mut table = ResultTable::new(["ran"]);
            table.push_row([request.action.clone()]);
            CommandOutcome::table(table)
        }
    }

    /// A policy modeling group-scoped reads + approval-gated writes: operators
    /// do anything; an agent reads only within its allowed group, every agent
    /// write needs approval, and an unknown group is denied.
    struct GroupScopedPolicy {
        allowed_group: &'static str,
    }

    impl AccessPolicy for GroupScopedPolicy {
        fn authorize(&self, request: &AccessRequest<'_>) -> AccessDecision {
            match request.caller {
                Caller::Operator => AccessDecision::Allow,
                Caller::Agent { agent_group_id } => {
                    if agent_group_id != self.allowed_group {
                        return AccessDecision::Deny {
                            reason: format!("group {agent_group_id:?} is out of scope"),
                        };
                    }
                    if request.kind.is_write() {
                        AccessDecision::NeedsApproval {
                            approval_id: format!("{}:{}", request.resource, request.action),
                        }
                    } else {
                        AccessDecision::Allow
                    }
                }
            }
        }
    }

    fn registry() -> CommandRegistry {
        let mut reg = CommandRegistry::new();
        reg.register(Box::new(UsersResource)).unwrap();
        reg
    }

    #[test]
    fn unknown_resource_or_action_is_reported_before_policy() {
        let reg = registry();
        let bad_resource = dispatch(
            &reg,
            &OperatorOnly,
            &Caller::Operator,
            &CommandRequest::new("ghosts", "list"),
        );
        assert!(matches!(bad_resource, CommandOutcome::Error { .. }));

        let bad_action = dispatch(
            &reg,
            &OperatorOnly,
            &Caller::Operator,
            &CommandRequest::new("users", "teleport"),
        );
        assert!(matches!(bad_action, CommandOutcome::Error { .. }));
    }

    #[test]
    fn operator_runs_reads_and_writes() {
        let reg = registry();
        assert!(dispatch(&reg, &OperatorOnly, &Caller::Operator, &CommandRequest::new("users", "list")).is_ok());
        assert!(dispatch(&reg, &OperatorOnly, &Caller::Operator, &CommandRequest::new("users", "create")).is_ok());
    }

    #[test]
    fn operator_only_denies_agents() {
        let reg = registry();
        let agent = Caller::Agent { agent_group_id: "g1".to_string() };
        let outcome = dispatch(&reg, &OperatorOnly, &agent, &CommandRequest::new("users", "list"));
        match outcome {
            CommandOutcome::Error { message } => assert!(message.contains("access denied")),
            _ => panic!("expected denial"),
        }
    }

    #[test]
    fn agent_read_in_scope_is_allowed_out_of_scope_denied() {
        let reg = registry();
        let policy = GroupScopedPolicy { allowed_group: "g1" };

        let in_scope = Caller::Agent { agent_group_id: "g1".to_string() };
        assert!(dispatch(&reg, &policy, &in_scope, &CommandRequest::new("users", "list")).is_ok());

        let out_of_scope = Caller::Agent { agent_group_id: "g2".to_string() };
        let denied = dispatch(&reg, &policy, &out_of_scope, &CommandRequest::new("users", "list"));
        match denied {
            CommandOutcome::Error { message } => assert!(message.contains("out of scope")),
            _ => panic!("expected out-of-scope denial"),
        }
    }

    #[test]
    fn agent_write_is_approval_gated() {
        let reg = registry();
        let policy = GroupScopedPolicy { allowed_group: "g1" };
        let agent = Caller::Agent { agent_group_id: "g1".to_string() };
        let outcome = dispatch(&reg, &policy, &agent, &CommandRequest::new("users", "create"));
        match outcome {
            CommandOutcome::Error { message } => {
                assert!(message.contains("approval required"));
                assert!(message.contains("users:create"));
            }
            _ => panic!("expected approval gate"),
        }
    }
}
