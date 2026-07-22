//! `lr skill`: the agent skill document, bundled into the binary.
//!
//! The document teaches an agent how to drive a live review over the control
//! plane (`lr session`). `lr skill path` writes it next to the config and prints
//! the path (the hunk convention); `lr skill show` prints it to stdout.

use anyhow::{Context, Result};

use crate::cli::{SkillArgs, SkillVerb};

/// The bundled skill document.
const SKILL: &str = include_str!("../skill/SKILL.md");

/// Run a `lr skill` verb (defaulting to `path`).
pub fn run(args: SkillArgs) -> Result<()> {
    match args.verb.unwrap_or(SkillVerb::Path) {
        SkillVerb::Show => {
            print!("{SKILL}");
            Ok(())
        }
        SkillVerb::Path => {
            let dir = crate::config::config_dir()
                .context("no config directory to write the skill document to")?
                .join("loopreview");
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            let path = dir.join("SKILL.md");
            std::fs::write(&path, SKILL).with_context(|| format!("writing {}", path.display()))?;
            println!("{}", path.display());
            Ok(())
        }
    }
}
