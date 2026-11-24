//! The `workspace-open { live "jump"; saved "focused" }` block.

use crate::error::ConfigError;
use crate::model::WorkspaceOpenConfig;
use crate::reader::NodeReader;

pub(super) fn parse_workspace_open(
    node: NodeReader<'_>,
) -> Result<WorkspaceOpenConfig, ConfigError> {
    let mut out = WorkspaceOpenConfig::default();
    for child in node.children_strict(&[], &[])? {
        let child = child?;
        match child.name() {
            "live" => out.live = child.enum_arg_as("\"jump\" or \"bring\"", "live")?,
            "saved" => {
                out.saved = child.enum_arg_as("\"focused\" or \"last-output\"", "saved")?;
            }
            _ => return Err(child.unknown_node("workspace-open")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::{err_at, parse};
    use crate::error::ConfigError;
    use crate::model::{Config, LiveOpen, SavedOpen, WorkspaceOpenConfig};

    #[test]
    fn workspace_open_defaults_to_jump_and_focused() {
        let d = WorkspaceOpenConfig::default();
        assert_eq!(d.live, LiveOpen::Jump);
        assert_eq!(d.saved, SavedOpen::Focused);
        assert_eq!(Config::default().workspace_open, d);
        assert_eq!(parse("").unwrap().workspace_open, d);
        assert_eq!(parse("workspace-open { }\n").unwrap().workspace_open, d);
    }

    #[test]
    fn workspace_open_parses_each_child_independently() {
        let c = parse("workspace-open { live \"bring\"; saved \"last-output\"; }\n").unwrap();
        assert_eq!(c.workspace_open.live, LiveOpen::Bring);
        assert_eq!(c.workspace_open.saved, SavedOpen::LastOutput);
        let c = parse("workspace-open { saved \"last-output\"; }\n").unwrap();
        assert_eq!(c.workspace_open.live, LiveOpen::Jump);
        assert_eq!(c.workspace_open.saved, SavedOpen::LastOutput);
        let c = parse("workspace-open { live \"jump\"; saved \"focused\"; }\n").unwrap();
        assert_eq!(c.workspace_open, WorkspaceOpenConfig::default());
    }

    #[test]
    fn workspace_open_rejects_bad_values_with_a_location() {
        for bad in ["\"warp\"", "\"Bring\"", "\"\"", "1", "#true"] {
            let (err, at) = err_at(&format!("\nworkspace-open {{ live {bad}; }}\n"));
            assert!(
                matches!(
                    err,
                    ConfigError::InvalidValue { .. } | ConfigError::WrongType { .. }
                ),
                "{bad}: {err:?}"
            );
            assert_eq!(at.line, 2, "{bad}");
        }
        let (err, at) = err_at("\nworkspace-open { saved \"live\"; }\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }), "{err:?}");
        assert_eq!(at.line, 2);
        let (err, _) = err_at("workspace-open { live; }\n");
        assert!(
            matches!(err, ConfigError::MissingArgument { .. }),
            "{err:?}"
        );
        let (err, _) = err_at("workspace-open { live \"jump\" \"bring\"; }\n");
        assert!(!matches!(err, ConfigError::UnknownNode { .. }), "{err:?}");
    }

    #[test]
    fn workspace_open_rejects_unknown_child_and_repeats() {
        let (err, _) = err_at("workspace-open { hotkey \"jump\"; }\n");
        assert!(matches!(err, ConfigError::UnknownNode { .. }), "{err:?}");
        let (err, _) = err_at("workspace-open { live \"jump\"; live \"bring\"; }\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
        let (err, _) = err_at("workspace-open { saved \"focused\"; saved \"focused\"; }\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
    }
}
