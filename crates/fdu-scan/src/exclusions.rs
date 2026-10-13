use hashbrown::HashMap;
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, PathBuf};
use std::sync::Arc;

/// Exact paths beneath the selected scan root that should be omitted.
#[derive(Clone, Debug)]
pub struct ScanExclusions {
    nodes: Arc<Vec<ExclusionNode>>,
}

#[derive(Default, Debug)]
struct ExclusionNode {
    children: HashMap<OsString, usize>,
    excluded: bool,
}

impl ScanExclusions {
    /// Builds exact exclusions from paths relative to the scan root.
    ///
    /// Parent traversals and paths naming the root itself are rejected. Excluding a directory
    /// omits its whole subtree; names are matched one path component at a time.
    pub fn from_paths(paths: impl IntoIterator<Item = PathBuf>) -> io::Result<Self> {
        let mut nodes = vec![ExclusionNode::default()];
        for path in paths {
            let mut current = 0;
            let mut has_component = false;
            for component in path.components() {
                let Component::Normal(name) = component else {
                    match component {
                        Component::CurDir => continue,
                        Component::ParentDir => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "exclude paths cannot traverse above the scan root",
                            ));
                        }
                        Component::RootDir | Component::Prefix(_) => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "exclude paths must be relative to the scan root",
                            ));
                        }
                        Component::Normal(_) => unreachable!(),
                    }
                };
                has_component = true;
                let next = match nodes[current].children.get(name) {
                    Some(next) => *next,
                    None => {
                        let next = nodes.len();
                        nodes.push(ExclusionNode::default());
                        nodes[current].children.insert(name.to_os_string(), next);
                        next
                    }
                };
                current = next;
            }
            if !has_component {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "exclude paths must name an entry below the scan root",
                ));
            }
            nodes[current].excluded = true;
        }
        Ok(Self { nodes: Arc::new(nodes) })
    }

    pub(crate) fn root_node(&self) -> Option<usize> {
        (!self.nodes[0].children.is_empty()).then_some(0)
    }

    pub(crate) fn child(&self, parent: Option<usize>, name: &[u8]) -> Option<usize> {
        let parent = parent?;
        let name = std::ffi::OsStr::from_bytes(name);
        self.nodes[parent].children.get(name).copied()
    }

    pub(crate) fn excludes(&self, node: usize) -> bool {
        self.nodes[node].excluded
    }

    pub(crate) fn has_children(&self, node: usize) -> bool {
        !self.nodes[node].children.is_empty()
    }
}

impl Default for ScanExclusions {
    fn default() -> Self {
        Self {
            nodes: Arc::new(vec![ExclusionNode::default()]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ScanExclusions;
    use std::io;
    use std::path::PathBuf;

    #[test]
    fn matches_exact_path_components_and_rejects_paths_outside_the_root() -> io::Result<()> {
        let exclusions = ScanExclusions::from_paths([
            PathBuf::from("cache"),
            PathBuf::from("vendor/generated/output"),
        ])?;
        let root = exclusions.root_node();
        let cache = exclusions.child(root, b"cache").unwrap();
        assert!(exclusions.excludes(cache));
        assert!(exclusions.child(root, b"cache-other").is_none());
        assert!(exclusions.child(root, b"vendor").is_some());

        for path in [PathBuf::from("../cache"), PathBuf::from("/cache"), PathBuf::from(".")] {
            let error = ScanExclusions::from_paths([path]).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        Ok(())
    }
}
