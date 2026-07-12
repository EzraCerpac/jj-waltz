//! Ordered removal workflow for the Herdr adapter.
//!
//! Marker cleanup stays last: a failed or process-ending Herdr close must leave
//! recovery provenance behind.

use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CloseTarget {
    Workspace(String),
    Tab(String),
}

impl CloseTarget {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Workspace(_) => "Herdr workspace",
            Self::Tab(_) => "Herdr tab",
        }
    }
}

pub(crate) struct RemovalPlan<'a> {
    pub(crate) name: &'a str,
    pub(crate) default_root: &'a Path,
    pub(crate) target: &'a CloseTarget,
    pub(crate) marker: Option<&'a Path>,
}

pub(crate) trait RemovalEffects {
    fn remove_workspace(&mut self, name: &str, default_root: &Path) -> Result<(), String>;
    fn close_container(&mut self, target: &CloseTarget) -> Result<(), String>;
    fn clear_marker(&mut self, marker: &Path) -> Result<(), String>;
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RemovalError {
    Workspace(String),
    Close(String),
    Marker { path: PathBuf, error: String },
}

impl fmt::Display for RemovalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace(error) => formatter.write_str(error),
            Self::Close(error) => {
                write!(
                    formatter,
                    "JJ workspace was removed, but Herdr close failed: {error}"
                )
            }
            Self::Marker { path, error } => write!(
                formatter,
                "JJ workspace and Herdr container were removed, but marker cleanup failed at {}: {error}",
                path.display()
            ),
        }
    }
}

pub(crate) fn execute_removal(
    plan: RemovalPlan<'_>,
    effects: &mut impl RemovalEffects,
) -> Result<(), RemovalError> {
    effects
        .remove_workspace(plan.name, plan.default_root)
        .map_err(RemovalError::Workspace)?;
    effects
        .close_container(plan.target)
        .map_err(RemovalError::Close)?;

    if let Some(marker) = plan.marker {
        effects
            .clear_marker(marker)
            .map_err(|error| RemovalError::Marker {
                path: marker.to_path_buf(),
                error,
            })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Step {
        RemoveWorkspace,
        CloseContainer,
        ClearMarker,
    }

    #[derive(Default)]
    struct FakeEffects {
        steps: Vec<Step>,
        fail_at: Option<Step>,
    }

    impl FakeEffects {
        fn record(&mut self, step: Step) -> Result<(), String> {
            self.steps.push(step);
            if self.fail_at == Some(step) {
                Err(format!("{step:?} failed"))
            } else {
                Ok(())
            }
        }
    }

    impl RemovalEffects for FakeEffects {
        fn remove_workspace(&mut self, _name: &str, _default_root: &Path) -> Result<(), String> {
            self.record(Step::RemoveWorkspace)
        }

        fn close_container(&mut self, _target: &CloseTarget) -> Result<(), String> {
            self.record(Step::CloseContainer)
        }

        fn clear_marker(&mut self, _marker: &Path) -> Result<(), String> {
            self.record(Step::ClearMarker)
        }
    }

    fn plan<'a>(target: &'a CloseTarget, marker: Option<&'a Path>) -> RemovalPlan<'a> {
        RemovalPlan {
            name: "feature",
            default_root: Path::new("/repo"),
            target,
            marker,
        }
    }

    #[test]
    fn removal_orders_workspace_close_before_marker_cleanup() {
        let target = CloseTarget::Tab("w1:t2".to_owned());
        let mut effects = FakeEffects::default();

        execute_removal(
            plan(&target, Some(Path::new("/state/tab-w1_t2.json"))),
            &mut effects,
        )
        .unwrap();

        assert_eq!(
            effects.steps,
            [
                Step::RemoveWorkspace,
                Step::CloseContainer,
                Step::ClearMarker
            ]
        );
    }

    #[test]
    fn workspace_failure_stops_before_close_and_marker_cleanup() {
        let target = CloseTarget::Workspace("w2".to_owned());
        let mut effects = FakeEffects {
            fail_at: Some(Step::RemoveWorkspace),
            ..FakeEffects::default()
        };

        let error = execute_removal(
            plan(&target, Some(Path::new("/state/workspace-w2.json"))),
            &mut effects,
        )
        .unwrap_err();

        assert!(matches!(error, RemovalError::Workspace(_)));
        assert_eq!(effects.steps, [Step::RemoveWorkspace]);
    }

    #[test]
    fn close_failure_preserves_marker_for_recovery() {
        let target = CloseTarget::Workspace("w2".to_owned());
        let mut effects = FakeEffects {
            fail_at: Some(Step::CloseContainer),
            ..FakeEffects::default()
        };

        let error = execute_removal(
            plan(&target, Some(Path::new("/state/workspace-w2.json"))),
            &mut effects,
        )
        .unwrap_err();

        assert!(matches!(error, RemovalError::Close(_)));
        assert_eq!(effects.steps, [Step::RemoveWorkspace, Step::CloseContainer]);
    }
}
