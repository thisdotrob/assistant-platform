//! Product memory taxonomies.
//!
//! The taxonomy is the small, named set of top-level folders a product lays out
//! under an agent's memory root (e.g. `people/`, `decisions/`, `journal/`). The
//! mechanism lives here; the actual category lists are owned by each product
//! (the assistant and cleoclaw), because what a memory tree should be organised
//! into is a product decision, not a platform one.
//!
//! A category is validated to be a single safe path segment at construction
//! time, and [`Taxonomy::scaffold`] additionally routes every directory it
//! creates through the same [`MemoryRoot`] confinement the rest of the crate
//! uses, so a taxonomy can never write outside the owning agent's root. The
//! generated `INDEX.md` is deterministic: the same taxonomy always renders the
//! same file, so re-scaffolding is idempotent.

use std::collections::HashSet;
use std::path::{Component, Path};

use crate::root::{IsolationError, MemoryRoot};

/// The deterministic index file written at the root of a scaffolded taxonomy.
/// Reserved: it is rejected as a category name and skipped by the reindex walk
/// (it is a taxonomy map, not a memory entry).
pub(crate) const INDEX_FILE: &str = "INDEX.md";

/// A category name was rejected, or appeared twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaxonomyError {
    /// Not a single safe path segment, or collides with the reserved index file.
    InvalidCategory { category: String },
    /// The same category was listed more than once.
    DuplicateCategory { category: String },
}

impl std::fmt::Display for TaxonomyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaxonomyError::InvalidCategory { category } => {
                write!(f, "taxonomy category {category:?} is not a single safe path segment")
            }
            TaxonomyError::DuplicateCategory { category } => {
                write!(f, "taxonomy category {category:?} is listed more than once")
            }
        }
    }
}

impl std::error::Error for TaxonomyError {}

/// Scaffolding a taxonomy onto disk failed.
#[derive(Debug)]
pub enum ScaffoldError {
    Io(std::io::Error),
    Isolation(IsolationError),
}

impl std::fmt::Display for ScaffoldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScaffoldError::Io(e) => write!(f, "taxonomy scaffold io error: {e}"),
            ScaffoldError::Isolation(e) => write!(f, "taxonomy scaffold isolation error: {e}"),
        }
    }
}

impl std::error::Error for ScaffoldError {}

impl From<std::io::Error> for ScaffoldError {
    fn from(e: std::io::Error) -> Self {
        ScaffoldError::Io(e)
    }
}

impl From<IsolationError> for ScaffoldError {
    fn from(e: IsolationError) -> Self {
        ScaffoldError::Isolation(e)
    }
}

/// A validated, ordered list of top-level memory categories for a product.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Taxonomy {
    categories: Vec<String>,
}

impl Taxonomy {
    /// Build a taxonomy from category names, preserving declared order. Each name
    /// must be a single safe path segment (no separators, no `..`, not absolute,
    /// not empty) and must not collide with the reserved `INDEX.md`. Duplicates
    /// are rejected.
    pub fn new<I, S>(categories: I) -> Result<Self, TaxonomyError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for category in categories {
            let category = category.into();
            if !is_safe_category(&category) || category == INDEX_FILE {
                return Err(TaxonomyError::InvalidCategory { category });
            }
            if !seen.insert(category.clone()) {
                return Err(TaxonomyError::DuplicateCategory { category });
            }
            out.push(category);
        }
        Ok(Self { categories: out })
    }

    /// The categories, in declared order.
    pub fn categories(&self) -> &[String] {
        &self.categories
    }

    /// The `INDEX.md` body this taxonomy renders. Deterministic in declared order.
    pub fn render_index(&self) -> String {
        let mut out = String::from("# Memory taxonomy\n\n");
        for category in &self.categories {
            out.push_str("- ");
            out.push_str(category);
            out.push('\n');
        }
        out
    }

    /// Create the taxonomy's category directories under `root` and write the
    /// deterministic `INDEX.md`. Every path is confined to the agent's own root.
    /// Idempotent: existing directories are left alone and the index is rewritten
    /// with identical content.
    pub fn scaffold(&self, root: &MemoryRoot) -> Result<(), ScaffoldError> {
        std::fs::create_dir_all(root.path())?;
        for category in &self.categories {
            let dir = root.resolve(category)?;
            let dir = root.confine(&dir)?;
            std::fs::create_dir_all(&dir)?;
        }
        let index_path = root.resolve(INDEX_FILE)?;
        let index_path = root.confine(&index_path)?;
        std::fs::write(&index_path, self.render_index())?;
        Ok(())
    }
}

/// True when `candidate` is a single, safe path segment.
fn is_safe_category(candidate: &str) -> bool {
    if candidate.is_empty() {
        return false;
    }
    let mut comps = Path::new(candidate).components();
    matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample() -> Taxonomy {
        Taxonomy::new(["people", "decisions", "journal", "archive"]).unwrap()
    }

    #[test]
    fn new_preserves_declared_order() {
        let taxonomy = sample();
        assert_eq!(
            taxonomy.categories(),
            ["people", "decisions", "journal", "archive"]
        );
    }

    #[test]
    fn rejects_unsafe_categories() {
        for bad in ["../escape", "a/b", "/abs", "", ".", "INDEX.md"] {
            assert!(
                matches!(
                    Taxonomy::new([bad]),
                    Err(TaxonomyError::InvalidCategory { .. })
                ),
                "{bad} accepted as a category"
            );
        }
    }

    #[test]
    fn rejects_duplicate_categories() {
        assert!(matches!(
            Taxonomy::new(["people", "decisions", "people"]),
            Err(TaxonomyError::DuplicateCategory { category }) if category == "people"
        ));
    }

    #[test]
    fn index_is_deterministic() {
        let expected = "# Memory taxonomy\n\n- people\n- decisions\n- journal\n- archive\n";
        assert_eq!(sample().render_index(), expected);
    }

    #[test]
    fn scaffold_creates_category_dirs_and_index() {
        let tmp = TempDir::new().unwrap();
        let root = MemoryRoot::orchestrator(tmp.path(), "ag_orchestrator");
        sample().scaffold(&root).unwrap();
        for category in sample().categories() {
            let dir = root.path().join(category);
            assert!(dir.is_dir(), "missing category dir {category}");
        }
        let index = std::fs::read_to_string(root.path().join(INDEX_FILE)).unwrap();
        assert_eq!(index, sample().render_index());
    }

    #[test]
    fn scaffold_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let root = MemoryRoot::orchestrator(tmp.path(), "ag_orchestrator");
        sample().scaffold(&root).unwrap();
        // Drop a user entry into a category; re-scaffolding must not disturb it.
        let kept = root.path().join("people").join("alice.md");
        std::fs::write(&kept, "hello").unwrap();
        sample().scaffold(&root).unwrap();
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "hello");
        let index = std::fs::read_to_string(root.path().join(INDEX_FILE)).unwrap();
        assert_eq!(index, sample().render_index());
    }

    #[test]
    fn scaffold_stays_inside_the_agent_root() {
        let tmp = TempDir::new().unwrap();
        let root = MemoryRoot::orchestrator(tmp.path(), "ag_orchestrator");
        sample().scaffold(&root).unwrap();
        // Nothing was created as a sibling of the memory root.
        let groups = tmp.path().join("orchestrator");
        let mut top_level: Vec<_> = std::fs::read_dir(&groups)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        top_level.sort();
        assert_eq!(top_level, ["memory"]);
    }
}
