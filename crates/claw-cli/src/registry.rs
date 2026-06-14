//! The command/resource registry.
//!
//! The host registers one [`Resource`] per domain (groups, users, sessions,
//! tasks, runs, memory, approvals, …). Each resource is a trait object, so this
//! crate resolves and dispatches commands without ever linking the domain
//! crates that implement them. The registry rejects a second resource of the
//! same name so two handlers never race for one command.

use crate::command::{ActionKind, CommandOutcome, CommandRequest};

/// One action a resource supports, tagged read or write so the command layer
/// can gate writes without understanding what the action does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionSpec {
    pub name: String,
    pub kind: ActionKind,
}

impl ActionSpec {
    pub fn read(name: impl Into<String>) -> Self {
        Self { name: name.into(), kind: ActionKind::Read }
    }

    pub fn write(name: impl Into<String>) -> Self {
        Self { name: name.into(), kind: ActionKind::Write }
    }
}

/// A named resource the CLI can act on. Resources are wired in by the host and
/// hold their own data handles; this crate only resolves and invokes them.
///
/// `execute` may assume access control already ran for the request's action,
/// but it must still validate its own arguments and return
/// [`CommandOutcome::Error`] rather than panic on bad input.
pub trait Resource {
    /// The resource name as it appears on the command line, e.g. `"users"`.
    fn name(&self) -> &str;

    /// The actions this resource supports.
    ///
    /// The read/write [`ActionKind`] each action declares here is what the
    /// access layer gates on — a resource that mislabels a mutating action as
    /// `read` would let it bypass write approval. The classification is trusted,
    /// so it must match what `execute` actually does for that action.
    fn actions(&self) -> Vec<ActionSpec>;

    /// Run an action. Unknown actions should yield [`CommandOutcome::Error`].
    fn execute(&self, request: &CommandRequest) -> CommandOutcome;
}

/// Registering a resource failed.
#[derive(Debug, PartialEq, Eq)]
pub enum RegistryError {
    /// A resource with this name is already registered.
    DuplicateResource { name: String },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::DuplicateResource { name } => {
                write!(f, "resource {name:?} is already registered")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// The set of resources the host has wired, keyed by resource name.
#[derive(Default)]
pub struct CommandRegistry {
    resources: Vec<Box<dyn Resource>>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a resource. Rejects a second resource of the same name.
    pub fn register(&mut self, resource: Box<dyn Resource>) -> Result<(), RegistryError> {
        let name = resource.name().to_string();
        if self.resources.iter().any(|r| r.name() == name) {
            return Err(RegistryError::DuplicateResource { name });
        }
        self.resources.push(resource);
        Ok(())
    }

    /// The registered resource names, in registration order.
    pub fn names(&self) -> Vec<&str> {
        self.resources.iter().map(|r| r.name()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&dyn Resource> {
        self.resources
            .iter()
            .find(|r| r.name() == name)
            .map(|r| r.as_ref())
    }

    /// The kind (read/write) of a resource's action, for access gating.
    /// `None` when the resource or action is unknown.
    pub fn action_kind(&self, resource: &str, action: &str) -> Option<ActionKind> {
        self.get(resource)?
            .actions()
            .into_iter()
            .find(|a| a.name == action)
            .map(|a| a.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::ResultTable;

    /// A resource that records nothing — it only reports its name/actions and
    /// echoes the action back so dispatch can be observed.
    struct StubResource {
        name: &'static str,
    }

    impl Resource for StubResource {
        fn name(&self) -> &str {
            self.name
        }
        fn actions(&self) -> Vec<ActionSpec> {
            vec![ActionSpec::read("list"), ActionSpec::write("create")]
        }
        fn execute(&self, request: &CommandRequest) -> CommandOutcome {
            let mut table = ResultTable::new(["action"]);
            table.push_row([request.action.clone()]);
            CommandOutcome::table(table)
        }
    }

    #[test]
    fn registers_distinct_resources_and_rejects_duplicates() {
        let mut reg = CommandRegistry::new();
        reg.register(Box::new(StubResource { name: "users" })).unwrap();
        reg.register(Box::new(StubResource { name: "groups" })).unwrap();
        assert_eq!(reg.names(), vec!["users", "groups"]);

        let dup = reg.register(Box::new(StubResource { name: "users" }));
        assert_eq!(dup, Err(RegistryError::DuplicateResource { name: "users".to_string() }));
    }

    #[test]
    fn resolves_a_resource_and_runs_an_action() {
        let mut reg = CommandRegistry::new();
        reg.register(Box::new(StubResource { name: "users" })).unwrap();

        let outcome = reg
            .get("users")
            .unwrap()
            .execute(&CommandRequest::new("users", "list"));
        assert!(outcome.is_ok());
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn action_kind_reflects_read_vs_write() {
        let mut reg = CommandRegistry::new();
        reg.register(Box::new(StubResource { name: "users" })).unwrap();
        assert_eq!(reg.action_kind("users", "list"), Some(ActionKind::Read));
        assert_eq!(reg.action_kind("users", "create"), Some(ActionKind::Write));
        assert_eq!(reg.action_kind("users", "nope"), None);
        assert_eq!(reg.action_kind("missing", "list"), None);
    }
}
